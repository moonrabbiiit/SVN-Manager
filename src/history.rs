use chrono::NaiveDate;
use egui::{
    Align, Color32, FontId, Frame, Layout, Rect, RichText, ScrollArea, TextEdit, Ui, Vec2,
};

use crate::jobs::Kind;
use crate::stats::{parse_date, resolve_range, reversed_hint, RangePreset};
use crate::svn::{LogEntry, LogPath};
use crate::{highlight, ink, SvnApp};

/// 「提交记录」页面状态。
#[derive(Clone)]
pub struct HistoryPage {
    pub dir: usize,
    pub label: String,
    pub limit: i64,
    pub entries: Vec<LogEntry>,
    pub error: String,
    pub filter: String,
    pub picked: Option<usize>,
    /// 本机 svn 登录人，空表示没取到
    pub author: String,
    /// 只看 `author` 的提交，初值由设置项 `history_only_mine` 决定
    pub mine: bool,
    /// 临时不限读取条数（不带 `-l` 拉全量）：只对本次页面生效，不写进设置
    pub unlimited: bool,
    /// 区间控件当前选中的预设：`None` = 按「条数」读最近 N 条。只管界面高亮。
    pub preset: Option<RangePreset>,
    /// 实际生效的服务器端日期区间（`None` = 不限日期）。读记录看这个，不看 `preset`。
    pub range: Option<(NaiveDate, NaiveDate)>,
    /// 「自定义」的两个输入缓冲（`YYYY-MM-DD`，结束留空 = 到今天）
    pub custom_from: String,
    pub custom_to: String,
    /// 区间输入有误时的提示：只报，不动已经读回来的列表
    pub range_error: String,
}

impl HistoryPage {
    /// `only_mine` 来自设置：关掉时打开历史页直接看所有人的记录
    pub fn new(dir: usize, label: String, limit: i64, author: String, only_mine: bool) -> Self {
        let mine = only_mine && !author.trim().is_empty();
        Self {
            dir,
            label,
            limit,
            entries: Vec::new(),
            error: String::new(),
            filter: String::new(),
            picked: None,
            author,
            mine,
            unlimited: false,
            preset: None,
            range: None,
            custom_from: String::new(),
            custom_to: String::new(),
            range_error: String::new(),
        }
    }

    fn visible(&self) -> Vec<usize> {
        let filter = self.filter.trim().to_lowercase();
        (0..self.entries.len())
            .filter(|index| {
                let entry = &self.entries[*index];
                // 按区间读时查询窗口两端各放宽了 1~2 天（躲开 `{日期}` 按本地还是 UTC 解释的
                // 分歧，也避免当天这种退化区间查不到东西），这里要按用户选的区间裁回去，
                // 否则列表里会混进区间外的提交，看着就像筛选没生效
                if let Some((from, to)) = self.range {
                    let Some(day) = entry.date.get(..10).and_then(parse_date) else {
                        return false;
                    };
                    if day < from || day > to {
                        return false;
                    }
                }
                filter.is_empty()
                    || entry.message.to_lowercase().contains(&filter)
                    || entry.author.to_lowercase().contains(&filter)
                    || entry.revision.contains(&filter)
                    || entry
                        .paths
                        .iter()
                        .any(|p| p.path.to_lowercase().contains(&filter))
            })
            .collect()
    }
}

/// 点「查看提交记录」后打开的窗口状态：这个文件 / 目录自己在服务器上的提交记录。
#[derive(Clone)]
pub struct FileLog {
    /// 所属目录行（同时作为任务池里的 dir，用于区分不同目录）
    pub dir: usize,
    /// 实际查询的仓库 URL
    pub url: String,
    /// svn log 里的相对路径，用于标题与匹配本次动作
    pub name: String,
    pub limit: i64,
    pub entries: Vec<LogEntry>,
    pub error: String,
    /// 最大化 / 还原 与 Esc 关闭
    pub zoom: Zoom,
}

/// 左右分栏对比里的一行。
enum DiffRow {
    /// 整行提示：`true` 表示 `@@ -旧,行数 +新,行数 @@`（标蓝，同时是行号起点）
    Span(String, bool),
    /// 左右两列各一行；行号为 `0` 表示那一侧没有这一行
    Pair {
        left: String,
        left_no: i64,
        right: String,
        right_no: i64,
        kind: PairKind,
    },
}

/// 一对左右行的来源：上下文 / 只有左边（删除）/ 只有右边（新增）/ 两侧都有（改动）。
enum PairKind {
    Context,
    Removed,
    Added,
    Changed,
}

/// 窗口的「最大化 / 还原」+ Esc 关闭。两个明细窗口共用一套，免得各写一遍。
///
/// egui 的 Window 没有内置最大化，窗口的矩形又存在 egui 内部的 Area 状态里
/// （`Areas::get` 是 pub(crate)，外面读不到），所以只能每帧从 response 里记账：
/// 还原时用 `fixed_rect` 把矩形写回一帧，之后再交回正常的可拖动 / 可缩放逻辑。
#[derive(Clone, Default)]
pub struct Zoom {
    /// 窗口是否已铺满工作区
    pub maximized: bool,
    /// 上一帧窗口画在哪儿、多大。只在非最大化时更新，于是它一直留着「最大化之前」的那一个
    rect: Option<Rect>,
    /// 点了「还原」之后，下一帧要摆回这里
    restore: Option<Rect>,
}

impl Zoom {
    /// 一打开就铺满工作区：看差异要的是横向空间。点「还原」（或按 Esc）回到默认大小，
    /// 那个大小由 `place` 从窗口自己的默认矩形里记下来。
    pub fn maximized() -> Self {
        Self {
            maximized: true,
            ..Self::default()
        }
    }

    /// Esc：最上面一层是这个窗口、并且没有输入框占着键盘时才算数，
    /// 免得在别处打字按 Esc 把窗口顺手关了。
    /// 返回 true 表示调用方该关窗了；最大化时只还原不关（防误按）。
    pub fn escape(&mut self, ctx: &egui::Context, id: egui::Id) -> bool {
        let on_top = ctx.top_layer_id() == Some(egui::LayerId::new(egui::Order::Middle, id));
        let typing = ctx.memory(|m| m.focused()).is_some();
        if !on_top || typing || !ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            return false;
        }
        if self.maximized {
            self.set(false);
            false
        } else {
            true
        }
    }

    /// 工具栏按钮用：在最大化 / 还原之间切换
    pub fn toggle(&mut self) {
        self.set(!self.maximized);
    }

    fn set(&mut self, maximized: bool) {
        if !maximized {
            // 还原：把「最大化之前」的矩形交给下一帧
            self.restore = self.rect;
        }
        self.maximized = maximized;
    }

    /// 摆窗口：最大化时铺满工作区；还原的那一帧写回原矩形；其余时候交给 egui 自己记。
    ///
    /// `normal` 是不最大化时长什么样：一打开就最大化的窗口没有「上一帧」可记，还原全靠它。
    pub fn place<'a>(
        &mut self,
        ctx: &egui::Context,
        window: egui::Window<'a>,
        normal: Rect,
    ) -> egui::Window<'a> {
        let window = window.default_rect(normal);
        if self.maximized {
            self.rect.get_or_insert(normal);
            window.fixed_rect(ctx.content_rect())
        } else if let Some(rect) = self.restore.take() {
            window.fixed_rect(rect)
        } else {
            window
        }
    }

    /// 每帧回写窗口实际画在哪儿（最大化期间不记，好把最大化之前的矩形一直留着）
    pub fn shown(&mut self, rect: Rect) {
        if !self.maximized {
            self.rect = Some(rect);
        }
    }
}

/// 双击历史页「涉及文件」后，某次提交对这个文件 / 目录的逐行改动（查询 = 仓库根 + 日志路径）。
#[derive(Clone)]
pub struct FileDiff {
    /// 所属目录行（同时作为任务池里的 dir，用于区分不同目录）
    pub dir: usize,
    /// 这条提交记录（版本号、作者、时间、说明）
    pub revision: String,
    pub author: String,
    pub date: String,
    pub message: String,
    /// svn log 里的相对路径（相对仓库根），也是窗口标题
    pub path: String,
    /// 该次提交对这个路径的动作：A 新增 / M 修改 / D 删除 …
    pub action: char,
    /// 实际查询的仓库 URL
    pub url: String,
    /// `svn diff -c` 的原文
    pub diff: String,
    pub error: String,
    /// 本次取差异是否忽略空白与换行（XML 打开时自动勾上，其余默认不勾）
    pub ignore_white: bool,
    /// 最大化 / 还原 与 Esc 关闭
    pub zoom: Zoom,
}

fn action_color(ui: &Ui, action: char) -> Color32 {
    ink(
        ui,
        match action {
            // 新增绿 / 修改蓝 / 删除红，和提交页的状态标记保持一致
            'A' => Color32::from_rgb(60, 190, 110),
            'M' => Color32::from_rgb(90, 160, 240),
            'D' => Color32::from_rgb(235, 90, 90),
            'R' => Color32::from_rgb(185, 140, 240),
            _ => Color32::from_gray(170),
        },
    )
}

impl SvnApp {
    pub fn history_page(&mut self, ui: &mut Ui) {
        let mut page = match self.history.clone() {
            Some(page) => page,
            None => {
                self.back_to_main();
                return;
            }
        };
        let path = self.dir_path(page.dir).map(|p| p.display().to_string()).unwrap_or_default();
        // 是否还在读取记录一律现问任务池，页面里不存标志位（否则任务异常结束就会一直转圈）
        let writing = self.pool.is_writing(page.dir);
        let loading = self.pool.has(Kind::Log, page.dir);
        let local_rev = self
            .dirs
            .get(page.dir)
            .and_then(|view| view.info.clone())
            .map(|info| info.revision)
            .unwrap_or_default();
        let server_rev = self
            .dirs
            .get(page.dir)
            .map(|view| view.remote_rev.clone())
            .unwrap_or_default();
        // 服务器比本地新：说明这批记录还没更新到本地
        let behind = match (local_rev.parse::<i64>(), server_rev.parse::<i64>()) {
            (Ok(local), Ok(server)) => server > local,
            _ => false,
        };
        let mine_text = if page.author.trim().is_empty() {
            "只看我的提交".to_owned()
        } else {
            format!("只看 {} 的提交", page.author)
        };

        ui.horizontal(|ui| {
            if ui.button("← 返回目录").clicked() {
                self.back_to_main();
                self.history = None;
                return;
            }
            ui.label(RichText::new(format!("提交记录：{}", page.label)).size(17.0).strong());
            ui.label(RichText::new(path).size(12.0).weak());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // AI 日志入口统一在主界面右上角：日志模式开启后本页本人提交行才有勾选框，
                // 勾选后点右上角「AI 日志（N）」弹窗口，这里不再重复放按钮
                if writing || loading {
                    ui.spinner();
                }
                if ui.button("刷新").clicked() {
                    page.error.clear();
                    page.entries.clear();
                    self.cfg.log_limit = page.limit;
                    self.persist();
                    self.history = Some(page.clone());
                    self.spawn_log_in(page.dir, page.mine, page.unlimited, page.range);
                }
                // 临时不限条数：只影响本次页面的读取，不写进设置；勾上的瞬间就按新模式重读
                let by_count = page.range.is_none();
                let unlimited = ui.add_enabled(
                    by_count,
                    egui::Checkbox::new(&mut page.unlimited, "不限"),
                );
                if unlimited
                    .on_hover_text(
                        "勾上：本次读取不带条数限制，把服务器上全部提交记录拉下来（大仓库会慢一些）。\n\
                         取消：恢复按「条数」读取。\n\
                         只对当前这个页面生效，不写入设置；切换后会立刻重新读取。\n\
                         选了日期区间时这里会置灰：区间要的是那几天，条数限制用不上。",
                    )
                    .changed()
                {
                    page.error.clear();
                    page.entries.clear();
                    page.picked = None;
                    self.cfg.log_limit = page.limit;
                    self.persist();
                    self.history = Some(page.clone());
                    self.spawn_log_in(page.dir, page.mine, page.unlimited, page.range);
                }
                ui.add_enabled(
                    by_count && !page.unlimited,
                    egui::DragValue::new(&mut page.limit).range(1..=2000).speed(5),
                );
                ui.label(
                    RichText::new(if by_count { "条数" } else { "条数（区间内不限）" })
                        .weak()
                        .size(12.0),
                )
                .on_hover_text(
                    "每次从服务器读多少条提交。\n\
                     选了下面的日期区间时改由区间决定读多少条，这个值暂时不生效。",
                );
                ui.add_sized(
                    Vec2::new(300.0, 22.0),
                    TextEdit::singleline(&mut page.filter).hint_text("按说明 / 作者 / 路径过滤"),
                );
            });
        });
        ui.horizontal(|ui| {
            let toggle = ui.toggle_value(&mut page.mine, mine_text).changed();
            if toggle {
                page.error.clear();
                page.entries.clear();
                page.picked = None;
                self.cfg.log_limit = page.limit;
                self.persist();
                self.history = Some(page.clone());
                self.spawn_log_in(page.dir, page.mine, page.unlimited, page.range);
            }
            if !server_rev.is_empty() {
                ui.label(
                    RichText::new(format!(
                        "记录取自服务器（最新 r{server_rev}，本地 r{local_rev}）{}",
                        if behind { " · 本地待更新" } else { "" }
                    ))
                    .size(12.0)
                    .color(if behind {
                        ink(ui, Color32::from_rgb(240, 190, 70))
                    } else {
                        ui.visuals().weak_text_color()
                    }),
                );
            } else {
                ui.label(RichText::new("记录取自服务器 HEAD").size(12.0).weak());
            }
        });
        // 日期区间：走服务器端 `-r {止}:{起}`，比在已读的那批里筛准（条数之外的旧提交也能拿到）
        let today = chrono::Local::now().date_naive();
        let mut refetch = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new("区间").strong());
            if ui
                .selectable_label(page.preset.is_none(), "按条数")
                .on_hover_text(
                    "不限制日期，按上面的「条数」读最近 N 条提交（默认就是这个）。",
                )
                .clicked()
                && page.preset.is_some()
            {
                page.preset = None;
                page.range = None;
                page.range_error.clear();
                refetch = true;
            }
            for preset in [
                RangePreset::Today,
                RangePreset::Week,
                RangePreset::Month,
                RangePreset::Year,
                RangePreset::Custom,
            ] {
                if !ui
                    .selectable_label(page.preset == Some(preset), preset.label())
                    .on_hover_text(if preset == RangePreset::Custom {
                        "手填起止日期（含两端），填完点「查询」；结束日期留空表示到今天".to_owned()
                    } else {
                        preset.hover().to_owned()
                    })
                    .clicked()
                {
                    continue;
                }
                if preset == RangePreset::Custom {
                    // 只切界面：日期还没填完就发请求，只会读到一批没意义的记录
                    page.preset = Some(preset);
                    continue;
                }
                match resolve_range(preset, &page.custom_from, &page.custom_to, today) {
                    Ok(span) => {
                        page.preset = Some(preset);
                        page.range = span;
                        page.range_error.clear();
                        refetch = true;
                    }
                    Err(message) => page.range_error = message,
                }
            }
            if page.preset == Some(RangePreset::Custom) {
                let from = parse_date(&page.custom_from);
                let to = parse_date(&page.custom_to);
                crate::stats::date_input(
                    ui,
                    egui::Id::new("hist_custom_from"),
                    &mut page.custom_from,
                    "2026-09-01",
                    today,
                    crate::stats::DayLimits { from: None, to },
                );
                ui.label("至");
                crate::stats::date_input(
                    ui,
                    egui::Id::new("hist_custom_to"),
                    &mut page.custom_to,
                    "留空=今天",
                    today,
                    crate::stats::DayLimits { from, to: None },
                );
                if ui.button("查询").clicked() {
                    match resolve_range(RangePreset::Custom, &page.custom_from, &page.custom_to, today) {
                        Ok(span) => {
                            page.range = span;
                            page.range_error.clear();
                            refetch = true;
                        }
                        Err(message) => page.range_error = message,
                    }
                }
            }
            if let Some((from, to)) = page.range {
                ui.label(
                    RichText::new(format!("读 {from} ~ {to} 的提交"))
                        .size(12.0)
                        .weak(),
                );
            }
        });
        if page.preset == Some(RangePreset::Custom) {
            if let Some(warn) = reversed_hint(&page.custom_from, &page.custom_to) {
                ui.label(
                    RichText::new(warn)
                        .size(11.5)
                        .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                );
            }
        }
        if refetch {
            page.error.clear();
            page.entries.clear();
            page.picked = None;
            self.history = Some(page.clone());
            self.spawn_log_in(page.dir, page.mine, page.unlimited, page.range);
        }
        if !page.range_error.is_empty() {
            ui.label(
                RichText::new(page.range_error.clone())
                    .size(11.5)
                    .color(ink(ui, Color32::from_rgb(240, 190, 70))),
            );
        }
        if page.author.trim().is_empty() && !loading {
            ui.label(
                RichText::new("没有取到本机 svn 登录人（svn auth），无法默认只看自己的提交")
                    .size(11.5)
                    .weak(),
            );
        }
        if !page.error.is_empty() {
            ui.label(
                RichText::new(format!("读取失败：{}", page.error))
                    .color(ink(ui, Color32::from_rgb(240, 100, 100))),
            );
        }
        if loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("正在读取服务器提交记录（svn log -r HEAD:1）…");
            });
        }
        ui.separator();

        let visible = page.visible();
        let summary = if visible.is_empty() {
                if page.entries.is_empty() {
                    if loading || !page.error.is_empty() {
                        String::new()
                    } else if page.mine {
                        if page.unlimited {
                            format!(
                                "服务器上没有 {} 的提交，可关掉「只看 {}」看看别人的",
                                page.author, page.author
                            )
                        } else {
                            format!(
                                "服务器最近 {} 条记录里没有 {} 的提交，可增大条数、勾「不限」或关掉「只看 {}」",
                                page.limit, page.author, page.author
                            )
                        }
                    } else {
                        "没有取到提交记录".to_owned()
                    }
                } else {
                "过滤后没有匹配记录".to_owned()
            }
        } else {
            format!(
                "{} 条记录{}",
                visible.len(),
                if page.mine {
                    format!("（{author}）", author = page.author)
                } else {
                    String::new()
                }
            )
        };
        ui.label(RichText::new(summary).weak().size(12.0));

        let body = ui.style().text_styles[&egui::TextStyle::Body].size;
        let filter = page.filter.trim().to_owned();
        ui.columns(2, |columns| {
            let width = columns[0].available_width();
            let picked = page.picked;
            let visible = page.visible();
            ScrollArea::vertical()
                .id_salt("history_list")
                .auto_shrink([false, false])
                .show(&mut columns[0], |ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    for index in visible {
                        let entry = page.entries[index].clone();
                        let first = entry
                            .message
                            .lines()
                            .next()
                            .unwrap_or("(无提交说明)")
                            .to_owned();
                        // 是本机登录人的提交才给勾选 AI 日志的复选框
                        let own = !page.author.trim().is_empty() && entry.author == page.author;
                        // 配色跟「提交记录·文件名」明细窗口一致：白底灰边（深色主题深底浅灰边）
                        let (row_fill, row_stroke) = if ui.visuals().dark_mode {
                            (Color32::from_gray(40), Color32::from_gray(65))
                        } else {
                            (Color32::WHITE, Color32::from_gray(200))
                        };
                        Frame::new()
                            .inner_margin(5.0)
                            .corner_radius(5.0)
                            .fill(if picked == Some(index) {
                                ui.visuals().selection.bg_fill
                            } else {
                                row_fill
                            })
                            .stroke(egui::Stroke::new(1.0, row_stroke))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    // 日志模式开启时才显示勾选框（右上角「AI 日志」开关控制）；
                                    // 是本机登录人的提交才有。勾选后收进「AI 工作日志」，跨目录汇总，
                                    // 状态每帧按勾选清单现算
                                    if own && self.ai_mode {
                                        let mut checked = self
                                            .ai_picks
                                            .iter()
                                            .any(|pick| pick.dir == page.dir && pick.entry.revision == entry.revision);
                                        if ui
                                            .checkbox(&mut checked, "")
                                            .on_hover_text(
                                                "勾选后加入「AI 工作日志」（可以到多个目录的历史页里反复勾选，\n\
                                                 最后点右上角「AI 日志（N）」弹出窗口，一次性交给 AI 生成工作日志）",
                                            )
                                            .changed()
                                        {
                                            if checked {
                                                self.ai_picks.push(crate::worklog::AiPick {
                                                    dir: page.dir,
                                                    dir_label: page.label.clone(),
                                                    entry: entry.clone(),
                                                });
                                            } else {
                                                self.ai_picks.retain(|pick| {
                                                    !(pick.dir == page.dir
                                                        && pick.entry.revision == entry.revision)
                                                });
                                            }
                                        }
                                    }
                                    ui.label(highlight(
                                        ui,
                                        &format!("r{}", entry.revision),
                                        &filter,
                                        FontId::monospace(body),
                                        ink(ui, Color32::from_rgb(120, 190, 240)),
                                    ));
                                    ui.label(highlight(
                                        ui,
                                        &entry.author,
                                        &filter,
                                        FontId::proportional(12.5),
                                        ui.visuals().text_color(),
                                    ));
                                    ui.label(RichText::new(&entry.date).size(12.0).weak());
                                    ui.label(RichText::new(format!("{} 项", entry.paths.len())).size(11.5).weak());
                                });
                                ui.add_sized(
                                    Vec2::new(width - 26.0, 18.0),
                                    egui::Label::new(highlight(
                                        ui,
                                        &first,
                                        &filter,
                                        FontId::proportional(12.5),
                                        ui.visuals().text_color(),
                                    ))
                                    .truncate(),
                                );
                                // 铺满整行：内容画完后把内部 ui 撑到剩余全宽，Frame 随之占满一行
                                ui.set_width(ui.available_width());
                                let rect = ui.max_rect();
                                // 有复选框时把整行点击区从行首缩进 26px：不缩的话这一块
                                // 最后注册、盖在复选框上面，点复选框会被当成点行
                                let click_rect = if own {
                                    egui::Rect::from_min_max(
                                        egui::pos2(rect.left() + 26.0, rect.top()),
                                        rect.max,
                                    )
                                } else {
                                    rect
                                };
                                if ui.interact(click_rect, ui.id().with(("log-row", index)), egui::Sense::click()).clicked() {
                                    page.picked = Some(index);
                                }
                            });
                    }
                });

            let detail = page.picked.and_then(|index| page.entries.get(index).cloned());
            let ui2 = &mut columns[1];
            match detail {
                None => {
                    ui2.label(RichText::new("选择左侧一条提交记录查看详情").weak());
                }
                Some(entry) => {
                    ScrollArea::vertical()
                        .id_salt("history_detail")
                        .auto_shrink([false, false])
                        .show(ui2, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!("r{}", entry.revision))
                                        .size(18.0)
                                        .strong()
                                        .color(ink(ui, Color32::from_rgb(120, 190, 240))),
                                );
                                if ui.button("复制说明").clicked() {
                                    ui.ctx().copy_text(entry.message.clone());
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("作者").weak().size(12.0));
                                ui.label(RichText::new(&entry.author).size(12.5));
                                ui.label(RichText::new("时间").weak().size(12.0));
                                ui.label(RichText::new(&entry.date).size(12.5));
                            });
                            ui.add_space(4.0);
                            ui.label(RichText::new("提交说明").strong().size(13.0));
                            if entry.message.trim().is_empty() {
                                ui.label(RichText::new("(无提交说明)").weak());
                            } else {
                                ui.label(highlight(
                                    ui,
                                    &entry.message,
                                    &filter,
                                    FontId::monospace(13.0),
                                    ui.visuals().text_color(),
                                ));
                            }
                            ui.add_space(6.0);
                            ui.label(
                                RichText::new(format!("涉及文件（{} 项）", entry.paths.len()))
                                    .strong()
                                    .size(13.0),
                            );
                            ui.label(
                                RichText::new("双击文件＝左右对比这一次提交对它的改动；鼠标停在文件上可点「查看提交记录」")
                                    .weak()
                                    .size(11.5),
                            );
                            for path in &entry.paths {
                                // 路径文字按剩余宽度截断、固定行高（悬停显示完整路径），
                                // 行最右侧始终留出「查看提交记录」的按钮位——长路径也不会把它挤出可视区
                                let color = action_color(ui, path.action);
                                // 目录名用粗体（Name("bold") 由 fonts::install_cjk 保证已绑定）
                                let font = if path.kind == "dir" {
                                    FontId {
                                        size: 12.0,
                                        family: egui::FontFamily::Name("bold".into()),
                                    }
                                } else {
                                    FontId::proportional(12.0)
                                };
                                let row = ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(path.action.to_string())
                                            .strong()
                                            .monospace()
                                            .color(color),
                                    );
                                    let full = path.path.clone();
                                    let path_w = (ui.available_width() - 118.0).max(120.0);
                                    // 不用 add_sized：它内部是居中布局，路径长短不一会显得没左对齐
                                    let text = ui
                                        .allocate_ui_with_layout(
                                            Vec2::new(path_w, 20.0),
                                            egui::Layout::left_to_right(egui::Align::Center),
                                            |ui| {
                                                ui.add(
                                                    egui::Label::new(highlight(
                                                        ui,
                                                        &path.path,
                                                        &filter,
                                                        font,
                                                        color,
                                                    ))
                                                    .truncate()
                                                    .sense(egui::Sense::click()),
                                                )
                                            },
                                        )
                                        .inner
                                        .on_hover_text(full);
                                    // 把行撑满到面板右缘（右侧留 8px），悬停按钮据此贴在最右边
                                    let spare = ui.available_width() - 8.0;
                                    if spare > 0.0 {
                                        ui.add_space(spare);
                                    }
                                    text
                                });
                                let button_rect = egui::Rect::from_min_size(
                                    egui::pos2(
                                        row.response.rect.max.x - 96.0,
                                        row.response.rect.center().y - 9.0,
                                    ),
                                    egui::vec2(90.0, 18.0),
                                );
                                // 用整行矩形判断，不用 Response::hovered()：指针停在行内的路径文字上时，
                                // 悬停会被那个标签独占，整行就永远不亮了（按钮也就永远点不出来）
                                if ui.rect_contains_pointer(row.response.rect) {
                                    // 必须用 place 不能用 put：put 会推进外层光标，按钮一出现
                                    // 后面几行就被顶下去一点，悬停一变整列跟着抖
                                    let button = ui.place(
                                        button_rect,
                                        egui::Button::new(RichText::new("查看提交记录").size(11.5))
                                            .small(),
                                    );
                                    if button.clicked() {
                                        self.open_file_log(page.dir, path);
                                    }
                                }
                                // 双击 -> 这一次提交对这个路径的左右逐行对比
                                if row.inner.double_clicked() {
                                    self.open_file_diff(page.dir, &entry, path);
                                }
                            }
                        });
                }
            }
        });

        self.history = Some(page);
    }

    /// 「某一次提交对这个文件的逐行改动」窗口：双击历史页的涉及文件打开，左右两栏对比。
    /// 只有一个窗口，双击别的路径就换内容，不会开一叠。
    pub fn file_diff_window(&mut self, ctx: &egui::Context) {
        let Some(mut diff) = self.file_diff.clone() else {
            return;
        };
        let dir = diff.dir;
        let revision = diff.revision.clone();
        let url = diff.url.clone();
        let path = diff.path.clone();
        let action = diff.action;
        // 是否还在读取一律现问任务池，窗口里不存标志位
        let loading = self.pool.has(Kind::FileDiff, dir);
        // Esc 关窗（最大化时先还原，再按一次才关）
        let close = diff.zoom.escape(ctx, egui::Id::new("file_diff"));
        // 关闭交给标题栏的 ×（和设置窗口一个样），工具栏里不再放「关闭」按钮。
        // open 是这一帧的局部变量：点了 × 时 egui 会把它写成 false，下面据此丢掉窗口内容
        let mut open = true;
        let window = egui::Window::new(format!(
            "r{revision} 改动明细 · {}",
            path.rsplit('/').next().unwrap_or(path.as_str())
        ))
        .id(egui::Id::new("file_diff"))
        .open(&mut open)
        .resizable(true);
        // 最大化时铺满工作区、还原那一帧把原矩形写回 egui，其余时候交给 egui 自己记
        let window = diff
            .zoom
            .place(
                ctx,
                window,
                Rect::from_min_size(egui::pos2(200.0, 100.0), Vec2::new(1080.0, 660.0)),
            )
            .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("r{revision}"))
                        .size(15.0)
                        .strong()
                        .color(ink(ui, Color32::from_rgb(120, 190, 240))),
                );
                // 字母 + 中文一起显示，避免只看到 A/M/D 不知道是什么意思
                let word = match action {
                    'A' => "新增",
                    'M' => "修改",
                    'D' => "删除",
                    'R' => "替换",
                    _ => "变更",
                };
                ui.label(
                    RichText::new(format!("{action} {word}"))
                        .strong()
                        .color(action_color(ui, action)),
                );
                ui.label(RichText::new(&diff.author).size(12.5));
                ui.label(RichText::new(&diff.date).size(12.0).weak());
                if loading {
                    ui.spinner();
                }
            });
            ui.label(RichText::new(&path).size(11.5).monospace().weak());
            if !diff.message.trim().is_empty() {
                ui.label(RichText::new(format!("说明：{}", diff.message.trim())).size(12.0));
            }
            ui.horizontal(|ui| {
                // 开关只跟着这个窗口走：XML 打开时已经自动勾上，其余文件默认看原始差异
                let mut ignore = diff.ignore_white;
                if ui
                    .checkbox(&mut ignore, "忽略空白与换行")
                    .on_hover_text(
                        "XML 默认已勾选（这类文件整份被报成改动，多半只是缩进 / 换行符变了），其余文件默认不勾。\n\
                         勾选后按 svn diff -x \"-w --ignore-eol-style\" 取差异，只看真正改了哪几行；\
                         取消勾选可看到原始差异。",
                    )
                    .changed()
                {
                    diff.ignore_white = ignore;
                    diff.diff.clear();
                    self.spawn_file_diff(dir, revision.clone(), url.clone(), ignore);
                }
                if ui.button("重新读取").clicked() {
                    diff.error.clear();
                    diff.diff.clear();
                    let ignore = diff.ignore_white;
                    self.spawn_file_diff(dir, revision.clone(), url.clone(), ignore);
                }
                // 最大化 / 还原：最大化之前的矩形由 Zoom 自己记着，还原时靠它摆回去
                let zoom = ui
                    .button(if diff.zoom.maximized { "还原大小" } else { "最大化" })
                    .on_hover_text(if diff.zoom.maximized {
                        "把窗口缩回最大化之前的大小（按 Esc 同样先还原）"
                    } else {
                        "把窗口铺满整个工作区（再按 Esc 或点「还原」回到原大小）"
                    });
                if zoom.clicked() {
                    diff.zoom.toggle();
                }
                ui.label(
                    RichText::new(
                        "左 = 上一个版本，右 = 本次版本；哪一侧空着表示那一版没有这一行。\
                         按 Esc 或点标题栏的 × 关闭",
                    )
                    .weak()
                    .size(11.5),
                );
            });
            if !diff.error.is_empty() {
                ui.label(
                    RichText::new(format!("读取失败：{}", diff.error))
                        .size(12.0)
                        .color(ink(ui, Color32::from_rgb(240, 100, 100))),
                );
                ui.label(
                    RichText::new("提示：这个路径在服务器最新版本上已被删除或改名时，按现在的地址取不到差异；先「更新」该目录通常就有。")
                        .weak()
                        .size(11.5),
                );
            }
            ui.separator();
            ScrollArea::vertical()
                .id_salt("file_diff_y")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    if diff.diff.trim().is_empty() && !loading && diff.error.is_empty() {
                        ui.label(
                            RichText::new("这一次提交没有改动这个路径的文本内容")
                                .weak()
                                .size(12.0),
                        );
                    }
                    // 左右各占面板宽度的一半（再减掉两条行号栏）；更长的行折行显示，不裁切
                    let col = ((ui.available_width() - 132.0) * 0.5).max(150.0);
                    // 列标题挪进滚动区，并且用和下面每一行一模一样的格子宽度，
                    // 否则标题会压偏自己那一列（滚动条占掉的宽度外面算不到）
                    ui.horizontal(|ui| {
                        ui.add_sized(Vec2::new(44.0, 18.0), egui::Label::new(""));
                        ui.add_sized(
                            Vec2::new(col, 18.0),
                            egui::Label::new(
                                RichText::new("上一个版本")
                                    .strong()
                                    .size(12.0)
                                    .color(ink(ui, Color32::from_rgb(235, 110, 110))),
                            ),
                        );
                        ui.add_sized(Vec2::new(44.0, 18.0), egui::Label::new(""));
                        ui.add_sized(
                            Vec2::new(col, 18.0),
                            egui::Label::new(
                                RichText::new("本次版本")
                                    .strong()
                                    .size(12.0)
                                    .color(ink(ui, Color32::from_rgb(90, 210, 130))),
                            ),
                        );
                    });
                    ui.separator();
                    // 先把 unified diff 配成左右成对的行：一段删除行 + 一段新增行 = 若干「改动」行
                    let mut rows: Vec<DiffRow> = Vec::new();
                    let mut old_line: i64 = 0;
                    let mut new_line: i64 = 0;
                    let mut dels: Vec<(i64, String)> = Vec::new();
                    let mut adds: Vec<(i64, String)> = Vec::new();
                    for line in diff.diff.lines().chain(std::iter::once("<结束>")) {
                        if line.starts_with('-') && !line.starts_with("---") {
                            dels.push((old_line, line[1..].to_owned()));
                            old_line += 1;
                            continue;
                        }
                        if line.starts_with('+') && !line.starts_with("+++") {
                            adds.push((new_line, line[1..].to_owned()));
                            new_line += 1;
                            continue;
                        }
                        // 一段 +/- 到此结束：左右按顺序配成对，多出来的那一侧单独占一行
                        for i in 0..dels.len().max(adds.len()) {
                            rows.push(DiffRow::Pair {
                                left: dels.get(i).map(|(_, text)| text.clone()).unwrap_or_default(),
                                left_no: dels.get(i).map(|(no, _)| *no).unwrap_or(0),
                                right: adds.get(i).map(|(_, text)| text.clone()).unwrap_or_default(),
                                right_no: adds.get(i).map(|(no, _)| *no).unwrap_or(0),
                                kind: match (dels.get(i).is_some(), adds.get(i).is_some()) {
                                    (true, true) => PairKind::Changed,
                                    (true, false) => PairKind::Removed,
                                    _ => PairKind::Added,
                                },
                            });
                        }
                        dels.clear();
                        adds.clear();
                        if line == "<结束>" {
                            break;
                        }
                        if let Some(header) = line.strip_prefix("@@ ") {
                            // 行号起点：@@ -旧起始,行数 +新起始,行数 @@
                            let mut parts = header.split_whitespace();
                            old_line = parts
                                .next()
                                .unwrap_or_default()
                                .trim_start_matches('-')
                                .split(',')
                                .next()
                                .unwrap_or_default()
                                .parse()
                                .unwrap_or(0);
                            new_line = parts
                                .next()
                                .unwrap_or_default()
                                .trim_start_matches('+')
                                .split(',')
                                .next()
                                .unwrap_or_default()
                                .parse()
                                .unwrap_or(0);
                            rows.push(DiffRow::Span(line.to_owned(), true));
                        } else if line.starts_with("Index:")
                            || line.starts_with("====")
                            || line.starts_with("---")
                            || line.starts_with("+++")
                            || line.starts_with("Propchange:")
                            || line.starts_with('\\')
                        {
                            // 文件头 / 属性变更 / 「无行尾换行」这些提示行不参与行号
                            rows.push(DiffRow::Span(line.to_owned(), false));
                        } else {
                            rows.push(DiffRow::Pair {
                                left: line.strip_prefix(' ').unwrap_or(line).to_owned(),
                                left_no: old_line,
                                right: line.strip_prefix(' ').unwrap_or(line).to_owned(),
                                right_no: new_line,
                                kind: PairKind::Context,
                            });
                            old_line += 1;
                            new_line += 1;
                        }
                        if rows.len() > 2000 {
                            rows.push(DiffRow::Span("差异过长，只展开前 2000 行".to_owned(), false));
                            break;
                        }
                    }
                    for row in &rows {
                        match row {
                            DiffRow::Span(text, header) => {
                                ui.monospace(
                                    RichText::new(text.as_str())
                                        .size(12.0)
                                        .color(if *header {
                                            ink(ui, Color32::from_rgb(120, 190, 240))
                                        } else {
                                            ui.visuals().weak_text_color()
                                        }),
                                );
                            }
                            DiffRow::Pair {
                                left,
                                left_no,
                                right,
                                right_no,
                                kind,
                            } => {
                                // 整行顶部对齐：代码折成几行，行号也始终贴着自己那一行，不会被垂直居中到行中间
                                ui.horizontal_top(|ui| {
                                    // 行号与代码同字号，并且只占一行高，基线才和代码第一行对齐
                                    let weak = ui.visuals().weak_text_color();
                                    let number = if *left_no > 0 {
                                        left_no.to_string()
                                    } else {
                                        String::new()
                                    };
                                    let galley = ui.fonts_mut(|fonts| {
                                        fonts.layout_no_wrap(number, FontId::monospace(12.0), weak)
                                    });
                                    // 行号交给 Label 自己画：只占一行高、压进 44pt 的栏里右对齐，
                                    // 顶边自然和代码顶边重合（自己算绘制坐标会把整列行号挪到下一行上）
                                    ui.add_sized(
                                        Vec2::new(44.0, galley.size().y),
                                        egui::Label::new(galley).halign(Align::Max),
                                    );
                                    let cell = ui.allocate_ui_with_layout(
                                        Vec2::new(col, 0.0),
                                        Layout::top_down(Align::Min),
                                        |ui| {
                                            ui.add(
                                                egui::Label::new(
                                                    RichText::new(if left.is_empty() { " " } else { left.as_str() })
                                                        .monospace()
                                                        .size(12.0)
                                                        .color(match kind {
                                                            PairKind::Removed
                                                            | PairKind::Changed => {
                                                                ink(ui, Color32::from_rgb(235, 110, 110))
                                                            }
                                                            _ => ui.visuals().text_color(),
                                                        }),
                                                )
                                                .wrap_mode(egui::TextWrapMode::Wrap)
                                                .selectable(true),
                                            );
                                        },
                                    );
                                    // 子 UI 只按「实际用掉」的宽度回报，短行要把右列拉回固定位置
                                    let pad = col - cell.response.rect.width();
                                    if pad > 0.0 {
                                        ui.add_space(pad);
                                    }
                                    let weak = ui.visuals().weak_text_color();
                                    let number = if *right_no > 0 {
                                        right_no.to_string()
                                    } else {
                                        String::new()
                                    };
                                    let galley = ui.fonts_mut(|fonts| {
                                        fonts.layout_no_wrap(number, FontId::monospace(12.0), weak)
                                    });
                                    ui.add_sized(
                                        Vec2::new(44.0, galley.size().y),
                                        egui::Label::new(galley).halign(Align::Max),
                                    );
                                    let cell = ui.allocate_ui_with_layout(
                                        Vec2::new(col, 0.0),
                                        Layout::top_down(Align::Min),
                                        |ui| {
                                            ui.add(
                                                egui::Label::new(
                                                    RichText::new(if right.is_empty() {
                                                        " "
                                                    } else {
                                                        right.as_str()
                                                    })
                                                    .monospace()
                                                    .size(12.0)
                                                    .color(match kind {
                                                        PairKind::Added | PairKind::Changed => {
                                                            ink(ui, Color32::from_rgb(90, 210, 130))
                                                        }
                                                        _ => ui.visuals().text_color(),
                                                    }),
                                                )
                                                .wrap_mode(egui::TextWrapMode::Wrap)
                                                .selectable(true),
                                            );
                                        },
                                    );
                                    let pad = col - cell.response.rect.width();
                                    if pad > 0.0 {
                                        ui.add_space(pad);
                                    }
                                });
                            }
                        }
                    }
                });
        });
        if let Some(shown) = &window {
            diff.zoom.shown(shown.response.rect);
        }
        // 点标题栏的 × 时 egui 只把 open 写成 false，那一帧窗口还在淡出（show 照旧返回 Some）。
        // open 是每帧新建的局部变量，不在这里丢掉内容的话，下一帧它又变回 true、窗口重新冒出来。
        self.file_diff = if window.is_none() || close || !open {
            None
        } else {
            Some(diff)
        };
    }

    /// 「这个路径自己的提交记录」窗口：点历史页涉及文件行右侧的「查看提交记录」打开。
    pub fn file_log_window(&mut self, ctx: &egui::Context) {
        let Some(mut log) = self.file_log.clone() else {
            return;
        };
        let dir = log.dir;
        let limit = log.limit;
        let url = log.url.clone();
        let name = log.name.clone();
        // 是否还在读取一律现问任务池，窗口里不存标志位
        let loading = self.pool.has(Kind::FileLog, dir);
        // Esc 关窗（最大化时先还原，再按一次才关）
        let close = log.zoom.escape(ctx, egui::Id::new("file_log"));
        // 和设置窗口一样，关闭交给标题栏的 ×
        let mut open = true;
        let window = egui::Window::new(format!(
            "提交记录 · {}",
            name.rsplit('/').next().unwrap_or(name.as_str())
        ))
        .id(egui::Id::new("file_log"))
        .open(&mut open)
        .resizable(true);
        // 最大化时铺满工作区、还原那一帧把原矩形写回 egui，其余时候交给 egui 自己记
        let window = log
            .zoom
            .place(
                ctx,
                window,
                Rect::from_min_size(egui::pos2(420.0, 200.0), Vec2::new(720.0, 500.0)),
            )
            .show(ctx, |ui| {
            ui.label(RichText::new(&url).size(11.5).monospace().weak());
            ui.horizontal(|ui| {
                if ui.button("重新读取").clicked() {
                    log.error.clear();
                    log.entries.clear();
                    self.spawn_file_log(dir, url.clone(), limit);
                }
                let zoom = ui
                    .button(if log.zoom.maximized { "还原" } else { "最大化" })
                    .on_hover_text(if log.zoom.maximized {
                        "把窗口缩回最大化之前的大小（按 Esc 同样先还原）"
                    } else {
                        "把窗口铺满整个工作区（再按 Esc 或点「还原」回到原大小）"
                    });
                if zoom.clicked() {
                    log.zoom.toggle();
                }
                ui.label(
                    RichText::new("按 Esc 或点标题栏的 × 关闭")
                        .weak()
                        .size(11.5),
                );
                if loading {
                    ui.spinner();
                    ui.label(RichText::new("svn log …").weak().size(11.5));
                } else if log.error.is_empty() {
                    ui.label(
                        RichText::new(format!("该文件共 {} 次提交", log.entries.len())).size(12.0),
                    );
                }
            });
            if !log.error.is_empty() {
                ui.label(
                    RichText::new(format!("读取失败：{}", log.error))
                        .size(12.0)
                        .color(ink(ui, Color32::from_rgb(240, 100, 100))),
                );
            }
            ui.label(
                RichText::new("双击某一条版本＝左右对比该版本与上一个版本的改动")
                    .weak()
                    .size(11.5),
            );
            ui.separator();
            ScrollArea::vertical()
                .id_salt("file_log_list")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 5.0;
                    if log.entries.is_empty() && !loading && log.error.is_empty() {
                        ui.label(RichText::new("没有取到该文件的提交记录").weak());
                    }
                    for entry in &log.entries {
                        // 这个文件在该版本里的动作：新增绿 / 修改蓝 / 删除红
                        let action = entry
                            .paths
                            .iter()
                            .find(|item| item.path == name)
                            .map(|item| item.action)
                            .unwrap_or('M');
                        let block = Frame::new()
                            .inner_margin(6.0)
                            .corner_radius(5.0)
                            // 灰边白底（深色主题换深底浅灰边），宽度和悬停描边保持一致观感
                            .fill(if ui.visuals().dark_mode {
                                Color32::from_gray(40)
                            } else {
                                Color32::WHITE
                            })
                            .stroke(egui::Stroke::new(
                                1.0,
                                if ui.visuals().dark_mode {
                                    Color32::from_gray(65)
                                } else {
                                    Color32::from_gray(200)
                                },
                            ))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(format!("r{}", entry.revision))
                                            .size(14.0)
                                            .strong()
                                            .color(ink(ui, Color32::from_rgb(120, 190, 240))),
                                    );
                                    ui.label(
                                        RichText::new(format!(
                                            "{} {}",
                                            action,
                                            match action {
                                                'A' => "新增",
                                                'D' => "删除",
                                                'R' => "替换",
                                                _ => "修改",
                                            }
                                        ))
                                        .strong()
                                        .size(12.5)
                                        .color(action_color(ui, action)),
                                    );
                                    ui.label(RichText::new(&entry.author).size(12.5));
                                    ui.label(RichText::new(&entry.date).size(12.0).weak());
                                    ui.label(
                                        RichText::new(format!("本次共 {} 项", entry.paths.len()))
                                            .size(11.5)
                                            .weak(),
                                    );
                                });
                                ui.label(
                                    RichText::new(if entry.message.trim().is_empty() {
                                        "(无提交说明)"
                                    } else {
                                        entry.message.trim()
                                    })
                                    .size(12.5),
                                );
                                // 铺满整行：内容画完后把内部 ui 撑到剩余全宽，Frame 随之占满一行
                                ui.set_width(ui.available_width());
                            });
                        // egui 0.36 的 Frame::show 只按 Sense::hover 分配空间，直接在它的
                        // response 上查 double_clicked() 永远是假——必须再叠一块真正感应点击的区域
                        let hit = ui.interact(
                            block.response.rect,
                            ui.id().with(("file_log_row", &entry.revision)),
                            egui::Sense::click(),
                        );
                        if hit.hovered() {
                            // 悬停描边提示这一行可以双击（颜色跟主题走）
                            let stroke = if ui.visuals().dark_mode {
                                egui::Stroke::new(1.0, Color32::from_rgb(120, 170, 230))
                            } else {
                                egui::Stroke::new(1.0, Color32::from_rgb(70, 120, 190))
                            };
                            ui.painter().rect_stroke(
                                block.response.rect,
                                5.0,
                                stroke,
                                egui::StrokeKind::Inside,
                            );
                        }
                        // 双击某一条 -> 看这一次提交对该文件的左右逐行对比
                        if hit.double_clicked() {
                            self.open_file_diff(
                                dir,
                                entry,
                                &LogPath {
                                    action,
                                    kind: String::new(),
                                    path: name.clone(),
                                },
                            );
                        }
                    }
                });
        });
        if let Some(shown) = &window {
            log.zoom.shown(shown.response.rect);
        }
        // open 被标题栏的 × 写成 false 时立刻丢内容，否则下一帧它又变回 true、窗口重新冒出来
        self.file_log = if window.is_none() || close || !open {
            None
        } else {
            Some(log)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::parse_date;

    fn entry(revision: &str, when: &str) -> LogEntry {
        LogEntry {
            revision: revision.to_owned(),
            author: "zhangsan".to_owned(),
            date: when.to_owned(),
            message: String::new(),
            paths: Vec::new(),
        }
    }

    /// 按区间读时查询窗口两端各放宽了 1~2 天，列表必须按用户选的区间裁回去
    #[test]
    fn range_prunes_the_widened_query_window_out_of_the_list() {
        let mut page = HistoryPage::new(0, "订单服务".to_owned(), 100, "zhangsan".to_owned(), true);
        page.entries = vec![
            entry("1", "2026-08-19 10:00:00"),
            entry("2", "2026-08-20 10:00:00"),
            entry("3", "2026-08-25 23:00:00"),
            entry("4", "2026-08-27 10:00:00"),
            entry("5", "看不懂的时间"),
        ];
        page.range = Some((parse_date("2026-08-20").unwrap(), parse_date("2026-08-25").unwrap()));
        assert_eq!(
            page.visible(),
            vec![1, 2],
            "放宽窗口带回来的边界外提交、以及读不出日期的记录都不能显示"
        );

        page.range = None;
        assert_eq!(page.visible(), vec![0, 1, 2, 3, 4], "不选区间时一切照旧");
    }
}

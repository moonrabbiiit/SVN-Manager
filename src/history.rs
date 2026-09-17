use chrono::NaiveDate;
use egui::{
    Align, Color32, FontId, Frame, Layout, Rect, RichText, ScrollArea, TextEdit, Ui, Vec2,
};

use crate::jobs::Kind;
use crate::stats::{parse_date, resolve_range, reversed_hint, RangePreset};
use crate::svn::{LogEntry, LogPath};
use crate::{highlight_with, ink, Search, SvnApp};

/// 「提交记录」页面状态。
#[derive(Clone)]
pub struct HistoryPage {
    pub dir: usize,
    pub label: String,
    pub limit: i64,
    pub entries: Vec<LogEntry>,
    pub error: String,
    pub filter: String,
    /// 过滤框旁的三个附加开关：大小写敏感 / 正则 / 整词。只对这一页生效，不写进设置
    pub search_case: bool,
    pub search_regex: bool,
    pub search_word: bool,
    /// 这一帧按了「上一处 / 下一处」：-1 / +1，等落点列表算出来后消费掉
    pub step: Option<i64>,
    /// 当前停在第几处命中（落点列表每帧按结果摊平算出来，所以只是个下标）
    pub spot: Option<usize>,
    /// 要把左侧列表滚动到哪一条记录（命中在说明上时用，滚到就清空）
    pub jump: Option<usize>,
    /// 要把右侧「涉及文件」滚动到第几行并标出来（命中在文件路径上时用，滚到就清空）
    pub focus_path: Option<usize>,
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
            search_case: false,
            search_regex: false,
            search_word: false,
            step: None,
            spot: None,
            jump: None,
            focus_path: None,
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

    /// 按调用方给的条件挑出该显示的行。条件（尤其是正则）由外面一次构建：
    /// 页面每帧要筛两次，不能让正则编译两遍。
    fn visible_with(&self, search: &Search) -> Vec<usize> {
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
                search.is_empty()
                    || search.hits(&entry.message)
                    || search.hits(&entry.author)
                    || search.hits(&entry.revision)
                    || entry.paths.iter().any(|p| search.hits(&p.path))
            })
            .collect()
    }
}

/// 一处命中的落点：说的是它是哪条记录里的、在不在「涉及文件」那一列里。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Spot {
    /// 命中在说明 / 作者 / 版本号上，左侧那条记录就能看到
    Row(usize),
    /// 命中在第 `entry` 条记录的涉及文件第 `path` 项上（右侧详情里那一行）
    Path(usize, usize),
}

impl Spot {
    /// 这一处命中属于哪条记录
    fn entry(self) -> usize {
        match self {
            Self::Row(index) | Self::Path(index, _) => index,
        }
    }
}

/// 把结果摊平成一串落点，顺序跟眼睛看到的顺序一致：先按列表自上而下，
/// 同一条记录里先看提交说明，再按涉及文件自上而下。
fn spots_of(entries: &[LogEntry], visible: &[usize], search: &Search) -> Vec<Spot> {
    let mut spots = Vec::new();
    for index in visible {
        let entry = &entries[*index];
        if search.hits(&entry.message) || search.hits(&entry.author) || search.hits(&entry.revision)
        {
            spots.push(Spot::Row(*index));
        }
        for (path_index, path) in entry.paths.iter().enumerate() {
            if search.hits(&path.path) {
                spots.push(Spot::Path(*index, path_index));
            }
        }
    }
    spots
}

/// 在落点列表里前后走一位（±1），走到头绕回另一头。还没定位过时：
/// 往下走给第一处、往上走给最后一处，第一次点就有落点。
fn step_spot(count: usize, current: Option<usize>, delta: i64) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let at = match current {
        Some(at) if at < count => at as i64,
        _ if delta > 0 => -1,
        _ => 0,
    };
    Some((at + delta).rem_euclid(count as i64) as usize)
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
                // 这一行从右往左排：标签先加、数字框后加，「条数」才会显示在输入框的右边
                ui.label(
                    RichText::new(if by_count { "条数" } else { "条数（区间内不限）" })
                        .weak()
                        .size(12.0),
                )
                .on_hover_text(
                    "每次从服务器读多少条提交。\n\
                     选了下面的日期区间时改由区间决定读多少条，这个值暂时不生效。",
                );
                ui.add_enabled(
                    by_count && !page.unlimited,
                    egui::DragValue::new(&mut page.limit).range(1..=2000).speed(5),
                );
                // 输入框 + 三枚模式开关 + 两枚定位按钮装进同一只圆角框，看起来是一个控件。
                // 宽度要按「这一行还剩多少」收口：直接 show 一个 Frame 会占满整行剩下的宽度；
                // 写死又会在窗口窄、或右侧标签变长（选了日期区间）时叠到旁边的控件上
                let chrome: f32 = 5.0 * 24.0 + 6.0 * 3.0 + 12.0;
                // 和右边「条数」那只数字框之间空出 20 像素：两处都是白底，贴在一起会看成一只控件，
                // 大框的白底还会压住数字框的左半截。这一行是 right_to_left，先占下这 20 像素的位，
                // 后面的大框就整体往左挪 20；这 20 也算进了 available_width，
                // 下面按「这一行还剩多少」收口时不用再单独扣
                ui.add_space(20.0);
                let room = (ui.available_width() - 8.0).max(260.0);
                let group_w = (400.0 + chrome).min(room);
                let input_w = (group_w - chrome).max(60.0);
                let group = ui.allocate_ui_with_layout(
                    Vec2::new(group_w, 24.0),
                    Layout::top_down(Align::Min),
                    |ui| {
                        Frame::new()
                            .corner_radius(6.0)
                            .inner_margin(egui::Margin::symmetric(6, 1))
                            .fill(ui.style().visuals.extreme_bg_color)
                            .stroke(egui::Stroke::new(
                                1.0,
                                ui.style().visuals.widgets.inactive.bg_stroke.color,
                            ))
                            .show(ui, |ui| {
                                // 这一行整体在 right_to_left 布局里，框内要显式改回从左往右，
                                // 否则按钮会跑到输入框左边、顺序还是反的
                                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                    ui.spacing_mut().item_spacing.x = 3.0;
                                    ui.add_sized(
                                        Vec2::new(input_w, 20.0),
                                        TextEdit::singleline(&mut page.filter)
                                            // 这个分支的 frame() 收 Frame 不是 bool：给个透明无描边的，
                                            // 让外面那只大框成为唯一的边
                                            .frame(
                                                Frame::new()
                                                    .fill(Color32::TRANSPARENT)
                                                    .stroke(egui::Stroke::NONE),
                                            )
                                            .hint_text("按说明 / 作者 / 路径过滤"),
                                    );
                                    // 三枚小按钮：Aa 区分大小写、.* 正则、\b 整词（都是编辑器里的通用记号）
                                    let chip = |ui: &mut Ui,
                                                 on: &mut bool,
                                                 glyph: &str,
                                                 tip: &str| {
                                        let button = egui::Button::selectable(
                                            *on,
                                            RichText::new(glyph).size(11.5),
                                        )
                                        .small()
                                        .min_size(Vec2::new(24.0, 18.0));
                                        if ui.add(button).on_hover_text(tip).clicked() {
                                            *on = !*on;
                                        }
                                    };
                                    chip(
                                        ui,
                                        &mut page.search_case,
                                        "Aa",
                                        "区分大小写：FIX 不再匹配 Fixed。",
                                    );
                                    chip(
                                        ui,
                                        &mut page.search_regex,
                                        ".*",
                                        "正则表达式：输入按正则解析，例如 r\\d+ 或 药品|明细。\n\
                                         表达式写错时列表不动，旁边会说明错在哪。",
                                    );
                                    chip(
                                        ui,
                                        &mut page.search_word,
                                        "\\b",
                                        "整词匹配：只命中完整的词。\n\
                                         add 命中「add-on」但不命中「address」；中文每个字都算词字符，\n\
                                         所以「药品」不会命中「修复药品明细」，被标点隔开才算。",
                                    );
                                    // 两枚定位按钮：在结果里逐处命中走，含「涉及文件」里被涂色的那些行
                                    let arrow = |ui: &mut Ui, glyph: &str, tip: &str| {
                                        let ready =
                                            !page.filter.trim().is_empty() && !page.entries.is_empty();
                                        ui.add_enabled(
                                            ready,
                                            egui::Button::new(RichText::new(glyph).size(11.5))
                                                .small()
                                                .min_size(Vec2::new(24.0, 18.0)),
                                        )
                                        .on_hover_text(tip)
                                        .clicked()
                                    };
                                    if arrow(
                                        ui,
                                        "▲",
                                        "上一处命中：往前跳到上一处高亮（提交说明或「涉及文件」里的那一行），并滚动到它。",
                                    ) {
                                        page.step = Some(-1);
                                    }
                                    if arrow(
                                        ui,
                                        "▼",
                                        "下一处命中：往后跳到下一处高亮（提交说明或「涉及文件」里的那一行），并滚动到它。",
                                    ) {
                                        page.step = Some(1);
                                    }
                                });
                            });
                });
                let search = Search::build(
                    &page.filter,
                    page.search_case,
                    page.search_regex,
                    page.search_word,
                );
                // 表达式写错不占版面：在大框下沿浮一条提示，改对了自然就没有了
                if !search.error.is_empty() {
                    let below = group.response.rect.left_bottom() + Vec2::new(0.0, 4.0);
                    egui::Area::new(ui.id().with("search_error"))
                        .order(egui::Order::Foreground)
                        .fixed_pos(below)
                        .show(ui.ctx(), |ui| {
                            crate::worklog::note_card(ui, &search.error, true);
                        });
                }
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

        // 判定与高亮共用一份条件：列出来的和标出来的必须严格是同一批字
        let search = Search::build(
            &page.filter,
            page.search_case,
            page.search_regex,
            page.search_word,
        );
        let visible = page.visible_with(&search);
        // 点三个开关或改过滤词之后，选中的那条可能被筛掉了：当场取消选中，
        // 否则右侧详情还停在一条列表里已经看不见的记录上，看着就像没刷新
        if page.picked.is_some_and(|index| !visible.contains(&index)) {
            page.picked = None;
            page.spot = None;
        }
        let spots = spots_of(&page.entries, &visible, &search);
        // 落点会随过滤条件变（同一处下标指向的可能已是另一条记录）：对不上就重新定位
        if page
            .spot
            .is_some_and(|at| spots.get(at).map(|spot| spot.entry()) != page.picked)
        {
            page.spot = None;
        }
        // 「上一处 / 下一处」在这里消费：按钮在上面那行按下时还不知道落点列表长什么样
        if let Some(delta) = page.step.take() {
            if let Some(at) = step_spot(spots.len(), page.spot, delta) {
                page.spot = Some(at);
                match spots[at] {
                    // 命中在说明 / 作者 / 版本号上：选中这条，并把左侧列表滚到它
                    Spot::Row(index) => {
                        page.picked = Some(index);
                        page.jump = Some(index);
                        page.focus_path = None;
                    }
                    // 命中在涉及文件里：选中这条，并把右侧详情滚到那一行（并标出来）
                    Spot::Path(index, path) => {
                        page.picked = Some(index);
                        page.focus_path = Some(path);
                    }
                }
            }
        }
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
            // 走 ▲▼ 时得知道自己站在第几处；只有一处命中就不啰嗦了
            let progress = match (page.spot, spots.len()) {
                (Some(at), count) if count > 1 => format!(" · 第 {}/{} 处命中", at + 1, count),
                (_, count) if count > 1 => format!(" · {count} 处命中"),
                _ => String::new(),
            };
            format!(
                "{} 条记录{}{progress}",
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
        ui.columns(2, |columns| {
            let width = columns[0].available_width();
            let picked = page.picked;
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
                        let row = Frame::new()
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
                                    ui.label(highlight_with(
                                        ui,
                                        &format!("r{}", entry.revision),
                                        &search,
                                        FontId::monospace(body),
                                        ink(ui, Color32::from_rgb(120, 190, 240)),
                                    ));
                                    ui.label(highlight_with(
                                        ui,
                                        &entry.author,
                                        &search,
                                        FontId::proportional(12.5),
                                        ui.visuals().text_color(),
                                    ));
                                    ui.label(RichText::new(&entry.date).size(12.0).weak());
                                    ui.label(RichText::new(format!("{} 项", entry.paths.len())).size(11.5).weak());
                                });
                                ui.add_sized(
                                    Vec2::new(width - 26.0, 18.0),
                                    egui::Label::new(highlight_with(
                                        ui,
                                        &first,
                                        &search,
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
                        // 这一行正是「上一处 / 下一处」跳过来的目标：滚进视野，标志用完就清
                        if page.jump == Some(index) {
                            row.response.scroll_to_me(Some(Align::Center));
                            page.jump = None;
                        }
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
                                ui.label(highlight_with(
                                    ui,
                                    &entry.message,
                                    &search,
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
                            for (path_index, path) in entry.paths.iter().enumerate() {
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
                                                    egui::Label::new(highlight_with(
                                                        ui,
                                                        &path.path,
                                                        &search,
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
                                // 这一行正是 ▲▼ 停住的那一处命中：滚到视野中间并描一圈——
                                // 描边不遮字也不改布局，所以整列不会跟着抖
                                if page.focus_path == Some(path_index) {
                                    row.response.scroll_to_me(Some(Align::Center));
                                    ui.painter().rect_stroke(
                                        row.response.rect.expand(1.0),
                                        3.0,
                                        egui::Stroke::new(
                                            1.5,
                                            ink(ui, Color32::from_rgb(240, 190, 70)),
                                        ),
                                        egui::StrokeKind::Inside,
                                    );
                                    page.focus_path = None;
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

    /// 三个开关只改判定，不改已经读回来的记录：同一批数据各自筛出什么
    #[test]
    fn search_modes_narrow_the_same_list() {
        let mut page = HistoryPage::new(0, "订单服务".to_owned(), 100, "zhangsan".to_owned(), false);
        let mut fix = entry("101", "2026-09-01 10:00:00");
        fix.message = "fix 药品明细".to_owned();
        fix.paths = vec![LogPath {
            action: 'M',
            kind: "file".to_owned(),
            path: "/src/address.vue".to_owned(),
        }];
        let mut caps = entry("102", "2026-09-02 10:00:00");
        caps.message = "FIX 药品".to_owned();
        let mut word = entry("103", "2026-09-03 10:00:00");
        word.message = "保存 add-on".to_owned();
        page.entries = vec![fix, caps, word];

        page.filter = "fix".to_owned();
        assert_eq!(visible(&page), vec![0, 1], "默认不分大小写");
        page.search_case = true;
        assert_eq!(visible(&page), vec![0], "大小写敏感后 FIX 那条要掉出去");

        page.search_case = false;
        page.filter = r"r?\d{3}".to_owned();
        assert!(
            visible(&page).is_empty(),
            "没开正则时这就是个字面串，谁都匹配不上"
        );
        page.search_regex = true;
        assert_eq!(visible(&page), vec![0, 1, 2], "开了正则，版本号三位数三条都命中");

        page.search_regex = false;
        page.filter = "add".to_owned();
        assert_eq!(visible(&page), vec![0, 2], "address 里的 add 也算子串");
        page.search_word = true;
        assert_eq!(visible(&page), vec![2], "整词只剩 add-on 那条");
    }

    /// 搜索框和三枚模式按钮要看起来是一个控件：同一只圆角框、同一行
    #[test]
    fn search_box_and_chips_paint_as_one_element() {
        let ctx = egui::Context::default();
        crate::fonts::install_cjk(&ctx);
        let mut app = crate::testbed::stub_app(false);
        app.page = crate::Page::History;
        app.history = Some(HistoryPage::new(
            0,
            "订单服务".to_owned(),
            100,
            "zhangsan".to_owned(),
            false,
        ));
        let mut out = ctx.run_ui(crate::testbed::base_input(), |ui| app.history_page(ui));
        out.textures_delta.clear();
        let texts: Vec<(String, egui::Rect)> = out
            .shapes
            .iter()
            .filter_map(|item| match &item.shape {
                egui::Shape::Text(text) => Some((
                    text.galley.text().to_string(),
                    item.shape.visual_bounding_rect(),
                )),
                _ => None,
            })
            .collect();
        let labels = ["按说明 / 作者 / 路径过滤", "Aa", ".*", "\\b", "▲", "▼"];
        let rects: Vec<egui::Rect> = labels
            .iter()
            .map(|label| {
                texts
                    .iter()
                    .find(|(text, _)| text == label)
                    .map(|(_, rect)| *rect)
                    .unwrap_or_else(|| panic!("搜索框那一行该有「{label}」"))
            })
            .collect();
        // 四段文字在同一行上（中心 y 相差 3 像素内：提示字与按钮字的行高本就不同），
        // 从左到右依次是输入框、三枚按钮
        for pair in rects.windows(2) {
            assert!(
                (pair[0].center().y - pair[1].center().y).abs() < 3.0,
                "输入框与按钮要并排在一行：{:?}",
                rects
            );
            assert!(pair[0].right() <= pair[1].left(), "顺序不对：{:?}", rects);
        }
        // 包住它们的那只大框：圆角 6，宽度对得上「400 输入框 + 三枚按钮 + 间距」
        let group = out
            .shapes
            .iter()
            .filter_map(|item| match &item.shape {
                egui::Shape::Rect(rect) if rect.corner_radius.nw == 6 => Some(rect.rect),
                _ => None,
            })
            .find(|rect| {
                rect.contains_rect(rects[0]) && rect.contains_rect(*rects.last().expect("有按钮"))
            })
            .expect("这几样东西该被同一只框包住");
        assert!(
            (540.0..=580.0).contains(&group.width()),
            "输入框 400 + 五枚小按钮，整组宽度应为 540~580，实际 {:.0}",
            group.width()
        );
        // 大框要和右边「条数」的数字框隔开：两处都是白底，贴住会看成一只控件，
        // 大框的白底还会压住数字框的左半截。除了这 20 像素，中间只该有默认的控件间距
        let drag = texts
            .iter()
            .find(|(text, _)| text == "100")
            .map(|(_, rect)| *rect)
            .expect("「条数」该有个数字框，值是 100");
        assert!(
            drag.left() - group.right() >= 25.0,
            "大框右侧要往左让出 20 像素，实际只空出 {:.0}",
            drag.left() - group.right()
        );
    }

    /// 测试用的便捷入口：按页面当前状态筛一次。页面本身走 visible_with，
    /// 条件（含正则编译）每帧只构建一次。
    fn visible(page: &HistoryPage) -> Vec<usize> {
        page.visible_with(&Search::build(
            &page.filter,
            page.search_case,
            page.search_regex,
            page.search_word,
        ))
    }

    /// 临时探针用：跑一帧历史页，返回画出来的「文字 + 矩形」
    fn painted_frame(
        ctx: &egui::Context,
        app: &mut crate::SvnApp,
        click: Option<egui::Pos2>,
    ) -> Vec<(String, egui::Rect)> {
        let mut events = Vec::new();
        if let Some(pos) = click {
            events.push(egui::Event::PointerMoved(pos));
            for pressed in [true, false] {
                events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                });
            }
        }
        let mut input = crate::testbed::base_input();
        input.events = events;
        let mut out = ctx.run_ui(input, |ui| app.history_page(ui));
        out.textures_delta.clear();
        out.shapes
            .iter()
            .filter_map(|item| match &item.shape {
                egui::Shape::Text(text) => Some((
                    text.galley.text().to_string(),
                    item.shape.visual_bounding_rect(),
                )),
                _ => None,
            })
            .filter(|(text, _)| !text.trim().is_empty())
            .collect()
    }

    /// 落点列表的顺序 = 眼睛看到的顺序：列表自上而下，同一条记录里先说说明、再按文件自上而下
    #[test]
    fn spots_follow_what_the_eye_sees() {
        let search = Search::build("vip", false, false, false);
        let mut first = entry("101", "2026-09-01 10:00:00");
        first.message = "vip 改造".to_owned();
        first.paths = vec![
            LogPath { action: 'M', kind: "file".to_owned(), path: "/a/vip.java".to_owned() },
            LogPath { action: 'A', kind: "file".to_owned(), path: "/b/other.java".to_owned() },
            LogPath { action: 'M', kind: "file".to_owned(), path: "/c/vip.jsp".to_owned() },
        ];
        // 第二条说明里没有，只有涉及文件命中
        let mut second = entry("102", "2026-09-02 10:00:00");
        second.message = "日常维护".to_owned();
        second.paths = vec![
            LogPath { action: 'M', kind: "file".to_owned(), path: "/d/VIP.xml".to_owned() },
        ];
        let entries = vec![first, second];
        assert_eq!(
            spots_of(&entries, &[0, 1], &search),
            vec![Spot::Row(0), Spot::Path(0, 0), Spot::Path(0, 2), Spot::Path(1, 0)],
            "说明在先、文件按顺序、记录自上而下"
        );
        // 只看第 2 条时的落点（可见列表变了，落点跟着变）
        assert_eq!(spots_of(&entries, &[1], &search), vec![Spot::Path(1, 0)]);
        assert!(spots_of(&entries, &[], &search).is_empty());
    }

    /// 前后走的落点：到头绕回，还没定位过时第一次点就有落点
    #[test]
    fn step_spot_walks_and_wraps_around() {
        assert_eq!(step_spot(5, Some(0), 1), Some(1));
        assert_eq!(step_spot(5, Some(4), 1), Some(0), "最后一处再往下绕回第一处");
        assert_eq!(step_spot(5, Some(0), -1), Some(4), "第一处再往上绕回最后一处");
        assert_eq!(step_spot(5, None, 1), Some(0), "还没定位过时往下走给第一处");
        assert_eq!(step_spot(5, None, -1), Some(4), "还没定位过时往上走给最后一处");
        // 落点变少后旧下标已越界：当成还没定位过处理
        assert_eq!(step_spot(3, Some(9), 1), Some(0));
        assert_eq!(step_spot(0, Some(0), 1), None, "没有命中就无处可跳");
    }


    /// 选了日期区间后右侧标签变长（「条数（区间内不限）」），窄窗口下搜索元素不能叠到它上面
    #[test]
    fn search_box_never_overlaps_the_neighbouring_label() {
        for width in [1700.0_f32, 1526.0, 1400.0, 1272.0, 1150.0, 1000.0, 900.0] {
            let ctx = egui::Context::default();
            crate::fonts::install_cjk(&ctx);
            let mut app = crate::testbed::stub_app(false);
            app.page = crate::Page::History;
            // 照用户窗口的样子：左侧挂一条很长的真实路径，右侧还选着日期区间（标签变长）
            app.cfg.dirs[0].path = r"D:\Program\Work\hhyp\Code\HRP_server".to_owned();
            let mut page =
                HistoryPage::new(0, "旧系统后端".to_owned(), 100, "wangzhanpeng".to_owned(), false);
            page.preset = Some(RangePreset::Year);
            page.range = Some((
                parse_date("2025-09-17").expect("起始日"),
                parse_date("2026-09-16").expect("结束日"),
            ));
            app.history = Some(page);
            let mut input = crate::testbed::base_input();
            input.screen_rect = Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::Vec2::new(width, 800.0),
            ));
            let mut out = ctx.run_ui(input, |ui| app.history_page(ui));
            out.textures_delta.clear();

            let texts: Vec<(String, egui::Rect)> = out
                .shapes
                .iter()
                .filter_map(|item| match &item.shape {
                    egui::Shape::Text(text) => Some((
                        text.galley.text().to_string(),
                        item.shape.visual_bounding_rect(),
                    )),
                    _ => None,
                })
                .collect();
            let input_rect = texts
                .iter()
                .find(|(text, _)| text.starts_with("按说明"))
                .map(|(_, rect)| *rect)
                .expect("搜索框里的提示文字");
            let group = out
                .shapes
                .iter()
                .filter_map(|item| match &item.shape {
                    egui::Shape::Rect(rect) if rect.corner_radius.nw == 6 => Some(rect.rect),
                    _ => None,
                })
                .filter(|rect| rect.contains_rect(input_rect))
                .min_by(|a, b| a.width().total_cmp(&b.width()))
                .expect("包住搜索框的那只大框");
            // 右侧这行的标签（「条数」或「条数（区间内不限）」）不能被压住
            let label = texts
                .iter()
                .find(|(text, _)| text.starts_with("条数"))
                .map(|(text, rect)| (text.clone(), *rect))
                .expect("条数标签");
            assert!(
                !group.intersects(label.1),
                "宽 {width:.0}：搜索元素压住了「{}」（框 {group:?} vs 文字 {:?}）",
                label.0,
                label.1
            );
            assert!(
                group.width() >= 240.0,
                "宽 {width:.0}：搜索元素被压得过小（{:.0}）",
                group.width()
            );
        }
    }

    /// 用户实际遇到的那种情况：说明里没有命中，命中全在「涉及文件」里
    #[test]
    fn arrow_buttons_reach_hits_inside_the_file_list() {
        let ctx = egui::Context::default();
        crate::fonts::install_cjk(&ctx);
        let mut app = crate::testbed::stub_app(false);
        app.page = crate::Page::History;
        let mut page = HistoryPage::new(0, "旧系统后端".to_owned(), 100, "zhangsan".to_owned(), false);
        let mut row = entry("5232", "2026-01-26 18:23:25");
        row.message = "日常维护".to_owned();
        row.paths = vec![
            LogPath { action: 'M', kind: "file".to_owned(), path: "/code/a/BudgetAmountService.java".to_owned() },
            LogPath { action: 'M', kind: "file".to_owned(), path: "/code/b/ExternalDBService.java".to_owned() },
            LogPath { action: 'A', kind: "file".to_owned(), path: "/code/b/externalDB.xml".to_owned() },
        ];
        page.entries = vec![row];
        page.filter = "externalDB".to_owned();
        app.history = Some(page);

        let click = |glyph: &str, app: &mut crate::SvnApp| {
            let frame = painted_frame(&ctx, app, None);
            let target = frame
                .iter()
                .find(|(text, _)| text == glyph)
                .map(|(_, rect)| *rect)
                .unwrap_or_else(|| panic!("搜索框那一行该有 {glyph} 按钮"));
            painted_frame(&ctx, app, Some(target.center()))
        };
        let page = |app: &crate::SvnApp| app.history.clone().expect("页面状态还在");

        // 命中两处，都在涉及文件里（第 2、3 项），说明那条不算落点
        click("▼", &mut app);
        assert_eq!(page(&app).picked, Some(0), "要选中这条记录，右侧才会显示涉及文件");
        assert_eq!(page(&app).spot, Some(0), "第一处命中是第一个 externalDB 文件");
        assert_eq!(
            page(&app).focus_path,
            None,
            "右侧详情该渲染并处理过那一行（坐标用完即清）"
        );
        click("▼", &mut app);
        assert_eq!(page(&app).spot, Some(1), "再点往下走第二处命中");
        click("▲", &mut app);
        assert_eq!(page(&app).spot, Some(0), "▲ 回到第一处");
        click("▲", &mut app);
        assert_eq!(page(&app).spot, Some(1), "第一处再往上绕回最后一处");
    }

    /// 点 ▼ / ▲ 就按结果顺序换选中那条，滚动标志用完即清
    #[test]
    fn arrow_buttons_walk_through_the_hits() {
        let ctx = egui::Context::default();
        crate::fonts::install_cjk(&ctx);
        let mut app = crate::testbed::stub_app(false);
        app.page = crate::Page::History;
        let mut page = HistoryPage::new(0, "订单服务".to_owned(), 100, "zhangsan".to_owned(), false);
        page.entries = ["fix 药品明细", "FIX 药品", "保存 药品"]
            .into_iter()
            .enumerate()
            .map(|(index, message)| {
                let mut entry = entry(&format!("10{index}"), "2026-09-01 10:00:00");
                entry.message = message.to_owned();
                entry
            })
            .collect();
        page.filter = "药品".to_owned();
        app.history = Some(page);

        let click = |glyph: &str, app: &mut crate::SvnApp| {
            let frame = painted_frame(&ctx, app, None);
            let target = frame
                .iter()
                .find(|(text, _)| text == glyph)
                .map(|(_, rect)| *rect)
                .unwrap_or_else(|| panic!("搜索框那一行该有 {glyph} 按钮"));
            painted_frame(&ctx, app, Some(target.center()))
        };
        let page = |app: &crate::SvnApp| app.history.clone().expect("页面状态还在");

        click("▼", &mut app);
        assert_eq!(page(&app).picked, Some(0), "第一次点 ▼ 落到第一条");
        assert_eq!(page(&app).jump, None, "滚到了就该把一次性标志清掉");
        click("▼", &mut app);
        assert_eq!(page(&app).picked, Some(1), "再点 ▼ 走第二条");
        click("▲", &mut app);
        assert_eq!(page(&app).picked, Some(0), "点 ▲ 回到第一条");
        click("▲", &mut app);
        assert_eq!(page(&app).picked, Some(2), "第一条再往上绕回最后一条");
    }

    /// 正则写错：错误说明走浮动气泡（Area），不占搜索框那行的版面
    #[test]
    fn bad_regex_floats_a_bubble_instead_of_taking_layout() {
        let ctx = egui::Context::default();
        crate::fonts::install_cjk(&ctx);
        let mut app = crate::testbed::stub_app(false);
        app.page = crate::Page::History;
        let mut page = HistoryPage::new(0, "订单服务".to_owned(), 100, "zhangsan".to_owned(), false);
        let mut entry_row = entry("101", "2026-09-01 10:00:00");
        entry_row.message = "fix 药品明细".to_owned();
        page.entries = vec![entry_row];
        page.filter = "(".to_owned();
        page.search_regex = true;
        app.history = Some(page);

        // 浮层（Area）要第二帧才画得出来，跟弹层菜单是同一个规律
        painted_frame(&ctx, &mut app, None);
        let frame = painted_frame(&ctx, &mut app, None);
        let (bubble_text, bubble) = frame
            .iter()
            .find(|(text, _)| text.starts_with("正则不合法"))
            .cloned()
            .unwrap_or_else(|| panic!("不合法的正则要浮出提示"));
        let chip = frame
            .iter()
            .find(|(text, _)| text == ".*")
            .map(|(_, rect)| *rect)
            .expect("没有正则按钮");
        assert!(
            bubble.top() >= chip.bottom(),
            "气泡要挂在大框下面，不能盖住按钮：{bubble:?} vs {chip:?}"
        );
        assert!(bubble_text.contains('('), "提示里要带上出错的那个式子：{bubble_text}");
        // 列表不受影响：那条记录照常显示（不筛成空，也不清空已经读回来的东西）
        assert!(frame.iter().any(|(text, _)| text == "fix 药品明细"));
    }

    /// 点一下模式按钮就要同步看到结果刷新：被筛掉的那条既要从列表消失，
    /// 也不能继续留在右侧详情里（否则看着像点了没反应）
    #[test]
    fn chip_click_refreshes_the_result_in_the_same_frame() {
        let ctx = egui::Context::default();
        crate::fonts::install_cjk(&ctx);
        let mut app = crate::testbed::stub_app(false);
        app.page = crate::Page::History;
        let mut page = HistoryPage::new(0, "订单服务".to_owned(), 100, "zhangsan".to_owned(), false);
        let mut lower = entry("101", "2026-09-01 10:00:00");
        lower.message = "fix 药品明细".to_owned();
        let mut upper = entry("102", "2026-09-02 10:00:00");
        upper.message = "FIX 药品".to_owned();
        page.entries = vec![lower, upper];
        page.filter = "fix".to_owned();
        // 先选中那条大写 FIX 的：它正是开大小写敏感后会被筛掉的那条
        page.picked = Some(1);
        app.history = Some(page);

        let has = |texts: &[(String, egui::Rect)], label: &str| {
            texts.iter().any(|(text, _)| text == label)
        };
        let first = painted_frame(&ctx, &mut app, None);
        assert!(has(&first, "fix 药品明细") && has(&first, "FIX 药品"), "默认不分大小写，两条都该在");
        let aa = first
            .iter()
            .find(|(text, _)| text == "Aa")
            .map(|(_, rect)| *rect)
            .expect("没有 Aa 按钮");

        // 点下去的那一帧就该看到新结果，不用等下一帧
        let second = painted_frame(&ctx, &mut app, Some(aa.center()));
        assert!(has(&second, "fix 药品明细"), "小写那条不该被误筛");
        assert!(
            !has(&second, "FIX 药品"),
            "开了大小写敏感，FIX 那条要立刻从列表里消失"
        );
        let page = app.history.as_ref().expect("页面状态还在");
        assert!(page.search_case, "按钮要点一下就生效");
        assert_eq!(page.picked, None, "被筛掉的选中记录要当场取消，右侧详情不能继续停在那条上");
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
            visible(&page),
            vec![1, 2],
            "放宽窗口带回来的边界外提交、以及读不出日期的记录都不能显示"
        );

        page.range = None;
        assert_eq!(visible(&page), vec![0, 1, 2, 3, 4], "不选区间时一切照旧");
    }
}

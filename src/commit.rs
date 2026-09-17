use std::path::Path;

use egui::{Align, Button, Color32, FontId, Frame, Layout, RichText, ScrollArea, TextEdit, Ui, Vec2};

use crate::jobs::{Data, Kind, Sink};
use crate::svn::{blocked_count, Item, StatusEntry, Svn};
use crate::{highlight, ink, Level, Maintain, SvnApp};

/// 「上传 / 提交」页面状态。
///
/// 这里不存任何「正在读取 / 正在提交」标志位：这类标志位一旦后台任务异常结束就再也清不掉，
/// 界面会一直停在「看着能点、点了没反应」的状态。是否忙一律现问 `Pool`（见 `commit_page`）。
#[derive(Clone)]
pub struct CommitPage {
    pub dir: usize,
    pub label: String,
    pub entries: Vec<StatusEntry>,
    pub message: String,
    pub error: String,
    pub done_ok: bool,
    pub filter: String,
    pub picked: usize,
    pub diff: String,
    /// 当前这份差异是否忽略空白与换行（按正在看的那一项决定：XML 自动勾选，其余默认不勾）
    pub diff_ignore: bool,
    pub confirm_revert: bool,
    /// 还没更新到最新时的第二下确认：第一下只把后果摆出来，再点一次才真的提交
    pub confirm_outdated: bool,
}

impl CommitPage {
    pub fn new(dir: usize, label: String) -> Self {
        Self {
            dir,
            label,
            entries: Vec::new(),
            message: String::new(),
            error: String::new(),
            done_ok: false,
            filter: String::new(),
            picked: usize::MAX,
            diff: String::new(),
            diff_ignore: false,
            confirm_revert: false,
            confirm_outdated: false,
        }
    }

    pub fn set_entries(&mut self, entries: Vec<StatusEntry>) {
        self.entries = entries;
        self.picked = usize::MAX;
        self.diff.clear();
        self.error.clear();
        // 重新读过状态就把「没更新到最新」的确认撤掉：清单变了，之前那一下点的是旧清单
        self.confirm_outdated = false;
    }

    /// 这次要提交的路径。未版本化(?) 与已丢失(!) 也算在内：提交前会先替它们补上
    /// svn add / svn delete，然后和修改项一起提交；冲突、不完整的一律不进来。
    pub fn commit_targets(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|entry| {
                entry.checked
                    && (entry.item.committable()
                        || entry.item.needs_add()
                        || entry.item.needs_delete())
            })
            .map(|entry| entry.path.clone())
            .collect()
    }

    /// 勾选出来、提交前需要先执行 svn add / svn delete 的条目。
    pub fn selected_targets(&self, op: Maintain) -> Vec<String> {
        self.entries
            .iter()
            .filter(|entry| entry.checked)
            .filter(|entry| match op {
                Maintain::Add => entry.item == Item::Unversioned,
                Maintain::Delete => entry.item == Item::Missing,
                _ => false,
            })
            .map(|entry| entry.path.clone())
            .collect()
    }

    /// 未版本化算进「新增」、已丢失算进「删除」（提交时会自动补 add / delete），
    /// 「需处理」只剩冲突、不完整这类必须人工干预的条目。
    fn counts(&self) -> (usize, usize, usize, usize) {
        let mut added = 0;
        let mut modified = 0;
        let mut deleted = 0;
        for entry in &self.entries {
            match entry.item {
                Item::Added | Item::Unversioned => added += 1,
                Item::Modified | Item::Replaced => modified += 1,
                Item::Deleted | Item::Missing => deleted += 1,
                _ => {}
            }
        }
        (added, modified, deleted, blocked_count(&self.entries))
    }

    fn visible(&self) -> Vec<usize> {
        let filter = self.filter.trim().to_lowercase();
        (0..self.entries.len())
            .filter(|index| {
                filter.is_empty()
                    || self.entries[*index].name.to_lowercase().contains(&filter)
                    || self.entries[*index].path.to_lowercase().contains(&filter)
            })
            .collect()
    }
}

/// 待提交列表展示用的相对路径：把绝对路径里的工作副本根前缀去掉，
/// 只留 `src\views\foo.vue` 这样的部分。路径不在根下（理论上不会发生）就原样返回。
fn relative_to_root(full: &str, root: &Path) -> String {
    let mut root = root.display().to_string();
    while root.ends_with(['\\', '/']) {
        root.pop();
    }
    let mut prefix = root;
    prefix.push(std::path::MAIN_SEPARATOR);
    if let Some(rest) = full.strip_prefix(&prefix) {
        if !rest.is_empty() {
            return rest.to_owned();
        }
    }
    full.to_owned()
}

/// 一次提交的清单：真正交给 `svn commit` 的路径，以及提交前要先补 svn add / svn delete 的路径。
pub struct CommitPlan {
    pub targets: Vec<String>,
    pub adds: Vec<String>,
    pub deletes: Vec<String>,
}

impl SvnApp {
    /// 上传前「还没更新到最新」的提醒文案：最近一次检测读到服务器上还有没拉到本地的新改动
    /// （`svn status -u` 的口径，见 `DirView::out_of_date`）时给一句，否则 `None`。
    /// 还没读到（断网、刚启动还没检测）就不给——无从判断，宁可不说，也不能把「不知道」当「未更新」。
    pub fn not_up_to_date_hint(&self, index: usize) -> Option<String> {
        let view = self.dirs.get(index)?;
        let pending = view.out_of_date.filter(|count| *count > 0)?;
        let revision = match view.remote_rev.as_str() {
            "" => String::new(),
            rev => format!("，远端已是 r{rev}"),
        };
        Some(format!(
            "未更新到最新版本{revision}：服务器还有 {pending} 项改动没拉到本地"
        ))
    }

    /// 提交。勾选里的未版本化 / 已丢失条目会先补上 svn add / svn delete，
    /// 再和修改项一起 svn commit；add / delete 只成一部分时那几条会被跳过并写明原因。
    pub fn spawn_commit(&mut self, index: usize, plan: CommitPlan, message: String) {
        let (Some(svn), Some(path)) = (self.svn_or_none(), self.dir_path(index)) else {
            self.hint("无法提交：svn.exe 不可用，或该目录已从列表中移除");
            return;
        };
        if plan.targets.is_empty() {
            self.hint("请先勾选要提交的文件（新增 / 修改 / 删除可以一起提交）");
            return;
        }
        if self.pool.has(Kind::Commit, index) {
            self.hint("该目录的提交任务还在执行中，请等当前提交结束");
            return;
        }
        let label = self.dir_label(index);
        let count = plan.targets.len();
        let update_after = self.cfg.update_after_commit;
        if let Some(commit) = self.commit.as_mut() {
            commit.done_ok = false;
        }
        self.pool
            .spawn(Kind::Commit, index, format!("{label} 提交 {count} 项"), move |sink| {
                commit_run(&svn, &sink, &path, &label, index, plan, &message, update_after)
            });
    }

    /// 「全部上传」：不看勾选，把该目录里所有能提交的条目一次提交上去。
    /// 清单在后台现读 `svn status`，口径和提交页的「只选可提交项」一致：未版本化 / 已丢失
    /// 先补 svn add / svn delete，再和修改项一起提交；没有可提交改动时这个目录自己跳过。
    pub fn spawn_upload_all(&mut self, index: usize, message: String) {
        let (Some(svn), Some(path)) = (self.svn_or_none(), self.dir_path(index)) else {
            self.hint("无法提交：svn.exe 不可用，或该目录已从列表中移除");
            return;
        };
        if self.pool.has(Kind::Commit, index) {
            self.hint("该目录的提交任务还在执行中，请等当前提交结束");
            return;
        }
        let label = self.dir_label(index);
        let update_after = self.cfg.update_after_commit;
        self.pool
            .spawn(Kind::Commit, index, format!("{label} 全部上传"), move |sink| {
                let (entries, stat) = svn.status(&path);
                if !stat.ok {
                    let text = format!("{label}：读取本地修改失败——{}", stat.summary());
                    return Data::Run {
                        dir: index,
                        ok: false,
                        message: text,
                        reload: false,
                    };
                }
                let picked: Vec<&StatusEntry> = entries
                    .iter()
                    .filter(|entry| entry.item.uploadable())
                    .collect();
                let plan = CommitPlan {
                    targets: picked.iter().map(|entry| entry.path.clone()).collect(),
                    adds: picked
                        .iter()
                        .filter(|entry| entry.item.needs_add())
                        .map(|entry| entry.path.clone())
                        .collect(),
                    deletes: picked
                        .iter()
                        .filter(|entry| entry.item.needs_delete())
                        .map(|entry| entry.path.clone())
                        .collect(),
                };
                if plan.targets.is_empty() {
                    let text = format!("{label}：没有可提交的改动，已跳过");
                    return Data::Run {
                        dir: index,
                        ok: true,
                        message: text,
                        reload: false,
                    };
                }
                sink.line(format!(
                    "{label}：全部上传 {} 项（其中先 add {} 项、先 delete {} 项）",
                    plan.targets.len(),
                    plan.adds.len(),
                    plan.deletes.len()
                ));
                commit_run(&svn, &sink, &path, &label, index, plan, &message, update_after)
            });
    }

    pub fn commit_page(&mut self, ui: &mut Ui) {
        let mut page = match self.commit.clone() {
            Some(page) => page,
            None => {
                self.back_to_main();
                return;
            }
        };
        let Some(path) = self.dir_path(page.dir) else {
            self.back_to_main();
            return;
        };
        let url = self
            .dirs
            .get(page.dir)
            .and_then(|view| view.info.as_ref())
            .map(|info| info.url.clone())
            .unwrap_or_default();
        // 是否忙一律由任务池现算：写类任务挡住「提交 / add / delete / revert」，
        // 读类任务（status、diff）只用于显示转圈，绝不影响按钮能否点中
        let writing = self.pool.is_writing(page.dir);
        let reading = self.pool.has(Kind::Status, page.dir);
        let diffing = self.pool.has(Kind::Diff, page.dir);

        ui.horizontal(|ui| {
            if ui.button("← 返回目录").clicked() {
                self.back_to_main();
                self.commit = None;
                return;
            }
            ui.label(RichText::new(format!("上传 / 提交：{}", page.label)).size(17.0).strong());
            ui.label(RichText::new(path.display().to_string()).size(12.0).weak());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if writing || reading || diffing {
                    ui.spinner();
                }
                if ui.button("重新读取修改").clicked() {
                    page.error.clear();
                    self.spawn_status(page.dir, false);
                }
            });
        });
        if !url.is_empty() {
            ui.label(
                RichText::new(format!("→ {url}"))
                    .size(12.0)
                    .color(ink(ui, Color32::from_rgb(140, 205, 255))),
            );
        }
        // 上传前先亮出基线：服务器上还有没拉到本地的新改动时，这次提交是在旧基线上改出来的，
        // 别人也动过同一处就会撞出冲突。点这一句直接跑该目录的 svn update；有写操作在执行时
        // 不给点——避免更新和提交同时改一个工作副本。
        if let Some(warning) = self.not_up_to_date_hint(page.dir) {
            let hit = ui.add(
                egui::Label::new(
                    RichText::new(&warning)
                        .size(12.5)
                        .strong()
                        .underline()
                        .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                )
                .sense(egui::Sense::click()),
            );
            if hit.clicked() && !writing {
                // 去更新就等于放弃了「没更新也提交」的确认，回来要重新点两下
                page.confirm_outdated = false;
                self.hint(format!("{}：正在更新到最新 …", page.label));
                self.spawn_update(page.dir);
            }
            hit.on_hover_text(if writing {
                "服务器上还有没拉到本地的新改动；该目录正有 SVN 写操作在执行，等它结束再更新"
            } else {
                "服务器上还有没拉到本地的新改动，这次上传是在旧基线上做的；点这一句直接执行该目录的 svn update"
            });
        }
        ui.separator();

        let (added, modified, deleted, blocked) = page.counts();
        let total = page.entries.len();
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("本次修改内容：共 {total} 项")).strong().size(13.5));
            ui.label(
                RichText::new(format!("新增 {added}"))
                    .color(ink(ui, Color32::from_rgb(60, 190, 110)))
                    .size(12.5),
            );
            ui.label(
                RichText::new(format!("修改 {modified}"))
                    .color(ink(ui, Color32::from_rgb(90, 160, 240)))
                    .size(12.5),
            );
            ui.label(
                RichText::new(format!("删除 {deleted}"))
                    .color(ink(ui, Color32::from_rgb(235, 90, 90)))
                    .size(12.5),
            );
            if blocked > 0 {
                ui.label(
                    RichText::new(format!("需处理 {blocked}"))
                        .color(ink(ui, Color32::from_rgb(240, 190, 70)))
                        .size(12.5),
                );
            }
        });
        ui.horizontal(|ui| {
            // 「全选」是字面意义全勾（含冲突）；计数只认能提交的条目，冲突等特殊状态
            // 勾上了也不会进提交——默认进场就是按 uploadable 勾好的，一般用不着它
            if ui
                .button("全选")
                .on_hover_text("勾上列表里的所有条目。注意：冲突等必须先人工处理的状态即使勾上也不会被提交；要回到默认勾选请点「只选可提交项」")
                .clicked()
            {
                for entry in page.entries.iter_mut() {
                    entry.checked = true;
                }
            }
            if ui.button("全不选").clicked() {
                for entry in page.entries.iter_mut() {
                    entry.checked = false;
                }
            }
            if ui
                .button("只选可提交项")
                .on_hover_text("恢复默认勾选：能随上传走的全部勾上（? 自动 add、! 自动 delete 也算），冲突等先人工处理的不勾")
                .clicked()
            {
                for entry in page.entries.iter_mut() {
                    entry.checked = entry.item.committable()
                        || entry.item.needs_add()
                        || entry.item.needs_delete();
                }
            }
            ui.label(RichText::new("|").weak());
            // 会改动工作副本的按钮：有写任务时直接置灰（置灰即点不动，显示与行为不会打架）
            ui.add_enabled_ui(!writing, |ui| {
                // 这两个按钮只是「提前单独执行一次」，不点也行：提交时会自动补上 add / delete
                let mut add = ui
                    .button("加入版本控制")
                    .on_hover_text("对勾选的未版本化文件执行 svn add（提交时会自动执行，无需先点这里）");
                if !page.entries.iter().any(|entry| entry.item.needs_add()) {
                    add = add.on_hover_text("没有未版本化条目");
                }
                if add.clicked() {
                    self.spawn_maintain(page.dir, Maintain::Add);
                }
                let mut remove = ui
                    .button("标记删除")
                    .on_hover_text("对勾选的丢失文件执行 svn delete（提交时会自动执行，无需先点这里）");
                if !page.entries.iter().any(|entry| entry.item.needs_delete()) {
                    remove = remove.on_hover_text("没有已丢失条目");
                }
                if remove.clicked() {
                    self.spawn_maintain(page.dir, Maintain::Delete);
                }
                let revert = if page.confirm_revert {
                    ui.add(
                        Button::new(
                            RichText::new("确认撤销全部修改？")
                                .strong()
                                .color(Color32::from_rgb(255, 240, 240)),
                        )
                        .fill(Color32::from_rgb(150, 60, 60)),
                    )
                } else {
                    ui.button("撤销全部修改").on_hover_text("svn revert --recursive")
                };
                if revert.clicked() {
                    if page.confirm_revert {
                        page.confirm_revert = false;
                        self.spawn_maintain(page.dir, Maintain::Revert);
                    } else {
                        page.confirm_revert = true;
                        self.hint("再次点击将丢弃该目录下所有本地修改（不可恢复）");
                    }
                }
                if page.confirm_revert && ui.button("放弃撤销").clicked() {
                    page.confirm_revert = false;
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_sized(
                    Vec2::new(210.0, 22.0),
                    TextEdit::singleline(&mut page.filter).hint_text("按名称过滤（命中处高亮）"),
                );
            });
        });
        if !page.error.is_empty() {
            ui.label(
                RichText::new(format!("读取失败：{}", page.error))
                    .color(ink(ui, Color32::from_rgb(240, 100, 100))),
            );
        }
        if page.done_ok {
            ui.label(
                RichText::new("✓ 提交成功，已重新读取修改列表")
                    .color(ink(ui, Color32::from_rgb(80, 200, 120))),
            );
        }

        ui.horizontal(|ui| {
            ui.label(RichText::new("提交备注").strong());
            ui.label(
                RichText::new("可留空；不会自动带入任何内容")
                    .weak()
                    .size(11.5),
            );
        });
        let targets = page.commit_targets();
        let picked_count = targets.len();
        // 勾选里的未版本化 / 已丢失条目：提交时先自动 svn add / svn delete，再和修改一起提交
        let adds = page.selected_targets(Maintain::Add);
        let deletes = page.selected_targets(Maintain::Delete);
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 140.0).max(240.0);
            ui.add(
                TextEdit::multiline(&mut page.message)
                    .desired_width(width)
                    .desired_rows(3)
                    .hint_text("填写本次提交说明（可留空）"),
            );
            ui.with_layout(Layout::top_down(Align::Max), |ui| {
                // 配色、点击结果、按钮下方的说明文字全部来自这一个判断：
                // 不能提交时一定说明原因，能提交时点了就一定有动作
                let blocked = if writing {
                    Some("该目录有 SVN 写操作正在执行，请稍候")
                } else if picked_count == 0 {
                    Some("未勾选要提交的文件")
                } else {
                    None
                };
                // 没更新到最新就拦一下：第一下只把后果摆出来，再点一次才真的提交
                // （两步确认的写法同下面的「撤销全部修改」）
                let outdated = self.not_up_to_date_hint(page.dir);
                let armed = outdated.is_some() && page.confirm_outdated;
                // egui 0.36 的按钮不会根据底色自动调整文字颜色，浅色主题下需要自己同时指定底色与字色
                let (fill, text) = if armed && blocked.is_none() {
                    (
                        Color32::from_rgb(150, 60, 60),
                        Color32::from_rgb(255, 240, 240),
                    )
                } else {
                    match (ui.visuals().dark_mode, blocked.is_none()) {
                        (true, false) => (Color32::from_gray(70), Color32::from_gray(148)),
                        (true, true) => (
                            Color32::from_rgb(38, 112, 70),
                            Color32::from_rgb(228, 245, 235),
                        ),
                        (false, false) => (Color32::from_gray(214), Color32::from_gray(120)),
                        (false, true) => (
                            Color32::from_rgb(23, 102, 61),
                            Color32::from_rgb(255, 255, 255),
                        ),
                    }
                };
                let caption = if armed {
                    format!("仍要提交 {picked_count} 项？")
                } else {
                    format!("提交 {picked_count} 项")
                };
                let button =
                    egui::Button::new(RichText::new(caption).size(14.0).strong().color(text))
                        .fill(fill);
                let response = ui.add_sized(Vec2::new(126.0, 60.0), button);
                if response.clicked() {
                    match blocked {
                        Some(why) => self.hint(why),
                        // 还没确认过就先拦住，只说明后果，不上传
                        None if outdated.is_some() && !page.confirm_outdated => {
                            page.confirm_outdated = true;
                            self.hint(format!(
                                "{}——再点一次「仍要提交」才会按本地版本上传，或点上面的提示条先更新到最新",
                                outdated.unwrap_or_default()
                            ));
                        }
                        None => {
                            let staging = if adds.is_empty() && deletes.is_empty() {
                                String::new()
                            } else {
                                format!("，先 add {} 项、delete {} 项", adds.len(), deletes.len())
                            };
                            // 没更新到最新还是让它传（用户可能就是要把手上这份传上去，而且已经确认过），
                            // 但要说清基线，并往输出记录写一条——提示条马上被下一句覆盖，只有记录留得住
                            match outdated.as_ref() {
                                Some(warning) => {
                                    self.push(
                                        Level::Warning,
                                        format!("{warning}，建议先「↓ 更新」再上传"),
                                    );
                                    self.hint(format!(
                                        "{warning}；仍按本地当前版本提交 {picked_count} 项 …"
                                    ));
                                }
                                None => self.hint(format!("正在提交 {picked_count} 项{staging} …")),
                            }
                            // 确认已经用掉了：下次进来要重新拦一遍（提交完的状态重读也会清掉它）
                            page.confirm_outdated = false;
                            self.spawn_commit(
                                page.dir,
                                CommitPlan {
                                    targets: targets.clone(),
                                    adds: adds.clone(),
                                    deletes: deletes.clone(),
                                },
                                page.message.clone(),
                            );
                        }
                    }
                }
                if armed && blocked.is_none() {
                    // 拦住的这一下要给退路：要么先更新到最新，要么明确放弃
                    ui.horizontal(|ui| {
                        if ui
                            .button("先更新到最新")
                            .on_hover_text("对该目录执行 svn update，再重新读一遍本地改动")
                            .clicked()
                        {
                            page.confirm_outdated = false;
                            self.hint(format!("{}：正在更新到最新 …", page.label));
                            self.spawn_update(page.dir);
                        }
                        if ui.button("放弃提交").clicked() {
                            page.confirm_outdated = false;
                        }
                    });
                }
                if !adds.is_empty() || !deletes.is_empty() {
                    ui.label(
                        RichText::new(format!("自动 add {} 项 · delete {} 项", adds.len(), deletes.len()))
                            .color(ink(ui, Color32::from_rgb(240, 190, 70)))
                            .size(11.5),
                    )
                    .on_hover_text("勾选的未版本化 / 已丢失条目会在提交前自动执行 svn add / svn delete，和修改一起提交");
                }
                if let Some(why) = blocked {
                    ui.label(RichText::new(why).weak().size(11.5));
                }
            });
        });
        ui.separator();

        let filter = page.filter.trim().to_owned();
        ui.columns(2, |columns| {
            let list_width = columns[0].available_width();
            let picked = page.picked;
            let visible = page.visible();
            if reading {
                columns[0].horizontal(|ui| {
                    ui.spinner();
                    ui.label("正在读取 svn status …");
                });
            }
            ScrollArea::vertical()
                .id_salt("commit_list")
                .auto_shrink([false, false])
                .show(&mut columns[0], |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    if visible.is_empty() && !reading {
                        ui.label(
                            RichText::new(if page.filter.is_empty() {
                                "没有本地修改，无需提交"
                            } else {
                                "过滤后没有匹配项"
                            })
                            .weak(),
                        );
                    }
                    for index in visible {
                        let entry = page.entries[index].clone();
                        // A/M/D 标记与文件名共用同一个类型色
                        let rgb = entry.item.color();
                        let color = ink(ui, Color32::from_rgb(rgb.0, rgb.1, rgb.2));
                        Frame::new()
                            .inner_margin(3.0)
                            .corner_radius(4.0)
                            .fill(if picked == index {
                                ui.visuals().selection.bg_fill
                            } else {
                                Color32::TRANSPARENT
                            })
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let mut checked = entry.checked;
                                    if ui.checkbox(&mut checked, "").on_hover_text("勾选后提交该项").changed() {
                                        page.entries[index].checked = checked;
                                    }
                                    ui.label(
                                        RichText::new(entry.item.mark().to_string())
                                            .strong()
                                            .monospace()
                                            .size(13.0)
                                            .color(color),
                                    )
                                    .on_hover_text(entry.item.text());
                                    // 文件名跟着类型着色（新增绿 / 修改蓝 / 删除红 …）；
                                    // 列表展示相对路径（去掉工作副本根前缀），悬停可见完整路径。
                                    // 不能用 add_sized：它内部是居中布局，会把文字摆到格子中间；
                                    // 这里显式分配整格宽度并用左对齐布局，文字才真正贴左
                                    let display = relative_to_root(&entry.path, &path);
                                    let name = ui
                                        .allocate_ui_with_layout(
                                            Vec2::new((list_width - 130.0).max(90.0), 18.0),
                                            Layout::left_to_right(Align::Center),
                                            |ui| {
                                                ui.add(
                                                    egui::Label::new(highlight(
                                                        ui,
                                                        &display,
                                                        &filter,
                                                        FontId::proportional(12.5),
                                                        color,
                                                    ))
                                                    .truncate()
                                                    .sense(egui::Sense::click()),
                                                )
                                            },
                                        )
                                        .inner
                                        .on_hover_text(format!(
                                            "{}\n状态：{}",
                                            entry.path,
                                            entry.item.text()
                                        ));
                                    if name.clicked() {
                                        page.picked = index;
                                        page.diff.clear();
                                        // 点选文件即自动读取差异，不必再点「查看该项差异」
                                        if entry.item == Item::Unversioned || entry.item == Item::Ignored {
                                            page.diff = "该条目尚未加入版本控制，没有差异可比对。".to_owned();
                                        } else {
                                            page.diff = "正在读取差异 …".to_owned();
                                            // XML 被编辑器整体改掉缩进 / 换行符的情况最多，这类默认忽略空白
                                            page.diff_ignore =
                                                entry.path.to_ascii_lowercase().ends_with(".xml");
                                            let ignore = page.diff_ignore;
                                            self.spawn_diff(page.dir, entry.path.clone(), ignore);
                                        }
                                    }
                                    // 这两类不再需要先手动处理：勾上就会在提交时自动补 add / delete
                                    if entry.item.needs_add() {
                                        ui.label(
                                            RichText::new("自动 add")
                                                .size(11.0)
                                                .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                                        )
                                        .on_hover_text("勾选后提交时自动执行 svn add，并和修改一起提交");
                                    } else if entry.item.needs_delete() {
                                        ui.label(
                                            RichText::new("自动 delete")
                                                .size(11.0)
                                                .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                                        )
                                        .on_hover_text("勾选后提交时自动执行 svn delete，并和修改一起提交");
                                    }
                                    if entry.props != "none" {
                                        ui.label(RichText::new("属性变更").size(11.0).weak());
                                    }
                                });
                            });
                    }
                });
            if picked != usize::MAX {
                let target = page.entries.get(picked).map(|e| (e.path.clone(), e.item));
                if let Some((target_path, item)) = target {
                    columns[0].add_enabled_ui(!(diffing || writing), |ui| {
                        ui.horizontal(|ui| {
                            if ui.button("查看该项差异").clicked() {
                                if item == Item::Unversioned || item == Item::Ignored {
                                    page.diff = "该条目尚未加入版本控制，没有差异可比对。".to_owned();
                                } else {
                                    page.diff = "正在读取差异 …".to_owned();
                                    page.diff_ignore =
                                        target_path.to_ascii_lowercase().ends_with(".xml");
                                    let ignore = page.diff_ignore;
                                    self.spawn_diff(page.dir, target_path.clone(), ignore);
                                }
                            }
                            if ui
                                .button("Beyond Compare")
                                .on_hover_text("用 Beyond Compare 对比 BASE 版本与本地文件")
                                .clicked()
                            {
                                self.open_bcompare_file(page.dir, target_path.clone());
                            }
                        });
                    });
                    // 选中项的完整路径也跟着类型着色
                    let rgb = item.color();
                    let job = highlight(
                        &columns[0],
                        &target_path,
                        &filter,
                        FontId::proportional(11.0),
                        ink(&columns[0], Color32::from_rgb(rgb.0, rgb.1, rgb.2)),
                    );
                    columns[0].label(job);
                }
            }

            // ---- 右列：差异 / 说明 ----
            let ui2 = &mut columns[1];
            ui2.horizontal(|ui| {
                ui.label(RichText::new("差异 / 明细").strong());
                if !page.diff.is_empty() && ui.button("清空").clicked() {
                    page.diff.clear();
                }
                // 这个开关只管当前这一项：换文件看差异时会按扩展名重新决定默认值
                let mut ignore = page.diff_ignore;
                if ui
                    .checkbox(&mut ignore, "忽略空白与换行")
                    .on_hover_text(
                        "XML 文件默认已勾选（本项目的 XML 常被编辑器整体改掉缩进或换行符 CRLF↔LF，\
                         不忽略时 svn 会把整份文件报成改动，看不出真正改了哪几行），其余文件默认不勾。\n\
                         勾选后按 svn diff -x \"-w --ignore-eol-style\" 比较；取消勾选可看到原始差异。",
                    )
                    .changed()
                {
                    page.diff_ignore = ignore;
                    page.diff.clear();
                    if picked != usize::MAX {
                        if let Some(entry) = page.entries.get(picked) {
                            self.spawn_diff(page.dir, entry.path.clone(), ignore);
                        }
                    }
                }
                ui.label(RichText::new("点选左侧文件自动显示差异").weak().size(11.5));
            });
            ui2.separator();
            if diffing {
                ui2.horizontal(|ui| {
                    ui.spinner();
                    ui.label("svn diff …");
                });
            } else if page.diff.trim().is_empty() {
                ui2.label(
                    RichText::new(
                        "提示：\n· 勾选要提交的文件，备注可留空，点「提交」即可\n· 新增 / 修改 / 删除可以一起提交：未版本化(?) 会先自动 svn add，\n  已丢失(!) 会先自动 svn delete，再和修改项一次提交\n· 冲突(C) 的条目要先解决，不会被带进提交",
                    )
                    .weak(),
                );
                if page.entries.is_empty() && !reading {
                    ui2.label(
                        RichText::new("该目录当前没有本地修改")
                            .color(ink(ui2, Color32::from_rgb(80, 200, 120))),
                    );
                }
            } else {
                ScrollArea::horizontal().id_salt("diff_x").show(ui2, |ui| {
                    ScrollArea::vertical()
                        .id_salt("diff_y")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            for line in page.diff.lines() {
                                let color = if line.starts_with('+') && !line.starts_with("+++") {
                                    ink(ui, Color32::from_rgb(90, 210, 130))
                                } else if line.starts_with('-') && !line.starts_with("---") {
                                    ink(ui, Color32::from_rgb(235, 110, 110))
                                } else if line.starts_with("@@") || line.starts_with("Index:") {
                                    ink(ui, Color32::from_rgb(120, 190, 240))
                                } else {
                                    ui.visuals().text_color()
                                };
                                ui.monospace(RichText::new(line).size(12.0).color(color));
                            }
                        });
                });
            }
        });

        self.commit = Some(page);
    }
}

/// 提交的实际流程：add → delete → 复核状态 → commit（成功后按设置补一次 update）。
/// 「上传 / 提交」页的提交按钮和「全部上传」共用这一条，区别只是清单由谁凑出来。
fn commit_run(
    svn: &Svn,
    sink: &Sink,
    path: &Path,
    label: &str,
    index: usize,
    plan: CommitPlan,
    message: &str,
    update_after_commit: bool,
) -> Data {
    let CommitPlan { targets, adds, deletes } = plan;
    let count = targets.len();
    if !adds.is_empty() {
        sink.line(format!("$ svn add --force （{} 项）", adds.len()));
        let run = svn.add(path, &adds, &|line| sink.line(line));
        if !run.ok {
            sink.line(format!("{label}：svn add 没有全部成功——{}", run.summary()));
        }
    }
    if !deletes.is_empty() {
        sink.line(format!("$ svn delete （{} 项）", deletes.len()));
        let run = svn.delete(path, &deletes, &|line| sink.line(line));
        if !run.ok {
            sink.line(format!("{label}：svn delete 没有全部成功——{}", run.summary()));
        }
    }
    // add / delete 可能只成功了几条，而混进一条未版本化的路径会让整次提交失败，
    // 所以提交前重新读一遍状态，只把真正进入 A/M/D/R 调度的路径交给 svn commit。
    let (fresh, stat) = svn.status(path);
    let mut commit_list: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    if stat.ok {
        for want in &targets {
            if fresh
                .iter()
                .any(|entry| &entry.path == want && entry.item.committable())
            {
                commit_list.push(want.clone());
            } else {
                skipped.push(want.clone());
            }
        }
    } else {
        sink.line(format!(
            "{label}：提交前复核状态没读成功，按清单直接提交——{}",
            stat.summary()
        ));
        commit_list = targets;
    }
    // 整目录 add 时子项会跟着一起进入 A 调度，但勾选的只有目录那一行；
    // --depth empty 只提交目录本身（实测子项留在本地待提交），所以把子项一并带上。
    // delete 不用管：提交被删的目录，服务端会连带删掉整棵子树。
    for want in &adds {
        if !commit_list.iter().any(|got| got == want) {
            continue;
        }
        let prefix = format!("{}\\", want.trim_end_matches(['\\', '/']));
        for child in &fresh {
            if child.item.committable()
                && child.path.starts_with(&prefix)
                && !commit_list.contains(&child.path)
            {
                commit_list.push(child.path.clone());
            }
        }
    }
    for miss in &skipped {
        sink.line(format!("已跳过（未能加入版本控制或标记删除）：{miss}"));
    }
    if commit_list.is_empty() {
        let text = format!("{label}：{count} 项都没能进入提交调度，已放弃提交");
        return Data::Run {
            dir: index,
            ok: false,
            message: text,
            reload: true,
        };
    }
    sink.line(format!(
        "$ svn commit --depth empty （{} 项）说明：{}",
        commit_list.len(),
        match message.trim() {
            "" => "（空）".to_owned(),
            text => text.lines().next().unwrap_or("").trim().to_owned(),
        }
    ));
    let run = svn.commit(path, &commit_list, message, &|line| sink.line(line));
    let text = if run.ok {
        let mut done = format!("{label}：已提交 {} 项", commit_list.len());
        if !skipped.is_empty() {
            done.push_str(&format!("，另有 {} 项被跳过（见输出记录）", skipped.len()));
        }
        done
    } else {
        format!("{label}：提交失败——{}", run.summary())
    };
    // 收尾文案只交给任务回收端统一写进输出区；这边再 sink.line 一次就会显示两遍
    // 提交只把被提交路径的版本推进，工作副本根目录还停在旧版本上；补一次 update
    // 将整棵树的 BASE 推到 HEAD，列表里的「本地 r」才会立刻跟上新提交的版本。
    // 注意：update 会把别人已提交的改动一起拉到本地，所以做成了「设置 → 常规」里的开关。
    // 收尾性质，正常时不往输出区刷 svn update 的逐行日志；只有没更新成功才提示一句。
    if run.ok && update_after_commit {
        let update = svn.update(path, &|_| {});
        if !update.ok {
            sink.line(format!(
                "{label}：提交成功，但提交后的更新没完成——{}",
                update.summary()
            ));
        }
    }

    // 就算提交没成功，add / delete 已经改过工作副本了，列表一律要重读
    Data::Run {
        dir: index,
        ok: run.ok,
        message: text,
        reload: true,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::svn::StatusEntry;

    fn entry(item: Item, checked: bool) -> StatusEntry {
        StatusEntry {
            path: format!("D:\\wc\\{}.java", item.mark()),
            name: format!("{}.java", item.mark()),
            item,
            props: "none".to_owned(),
            checked,
        }
    }

    fn page_of(items: Vec<Item>, checked: &[Item]) -> CommitPage {
        let mut page = CommitPage::new(0, "测试".to_owned());
        page.set_entries(
            items
                .into_iter()
                .map(|item| entry(item, checked.contains(&item)))
                .collect(),
        );
        page
    }

    /// 新增(?)、已丢失(!) 和修改一样能进提交目标，冲突 / 不完整不行。
    #[test]
    fn checked_new_and_missing_join_the_commit() {
        let page = page_of(
            vec![
                Item::Modified,
                Item::Added,
                Item::Deleted,
                Item::Replaced,
                Item::Unversioned,
                Item::Missing,
                Item::Conflict,
                Item::Incomplete,
                Item::Normal,
            ],
            &[
                Item::Modified,
                Item::Added,
                Item::Deleted,
                Item::Replaced,
                Item::Unversioned,
                Item::Missing,
                Item::Conflict,
                Item::Incomplete,
                Item::Normal,
            ],
        );
        let targets = page.commit_targets();
        assert_eq!(targets.len(), 6, "{targets:?}");
        assert!(targets.iter().any(|path| path.ends_with("?.java")));
        assert!(targets.iter().any(|path| path.ends_with("!.java")));
        assert!(!targets.iter().any(|path| path.ends_with("C.java")));
        // 没勾选的一律不算
        let unchecked = page_of(vec![Item::Unversioned, Item::Missing], &[]);
        assert!(unchecked.commit_targets().is_empty());
    }

    /// 提交前要补的 add / delete 名单只收勾选过的对应条目。
    #[test]
    fn staging_lists_only_take_checked_entries() {
        let page = page_of(
            vec![Item::Unversioned, Item::Missing, Item::Modified, Item::Added, Item::Deleted],
            &[Item::Unversioned, Item::Missing, Item::Modified],
        );
        assert_eq!(page.selected_targets(Maintain::Add), vec![r"D:\wc\?.java"]);
        assert_eq!(page.selected_targets(Maintain::Delete), vec![r"D:\wc\!.java"]);
        assert!(page.selected_targets(Maintain::Revert).is_empty());
    }

    /// 计数里未版本化算新增、已丢失算删除，「需处理」只剩冲突和不完整。
    #[test]
    fn counters_fold_pending_add_and_delete_into_totals() {
        let page = page_of(
            vec![
                Item::Added,
                Item::Unversioned,
                Item::Modified,
                Item::Deleted,
                Item::Missing,
                Item::Conflict,
            ],
            &[],
        );
        assert_eq!(page.counts(), (2, 1, 2, 1));
    }

    /// 列表展示相对路径：去掉工作副本根前缀，前缀之外的同名目录不能误伤。
    #[test]
    fn list_shows_paths_relative_to_working_copy_root() {
        assert_eq!(
            relative_to_root(r"D:\wc\src\views\index.vue", Path::new(r"D:\wc")),
            r"src\views\index.vue"
        );
        // 根路径尾部带不带分隔符都认
        assert_eq!(
            relative_to_root(r"D:\wc\a.vue", Path::new(r"D:\wc\")),
            r"a.vue"
        );
        // 根目录本身就是条目时保持原样
        assert_eq!(relative_to_root(r"D:\wc", Path::new(r"D:\wc")), r"D:\wc");
        // 只是共享前缀的兄弟目录不能被切掉
        assert_eq!(
            relative_to_root(r"D:\wc2\a.vue", Path::new(r"D:\wc")),
            r"D:\wc2\a.vue"
        );
    }

    /// 上传前的提醒只在「读到服务器上确实还有没拉到本地的新改动」时给：
    /// 没检测过（`None`，断网或刚启动）不猜，已是最新（`Some(0)`）也不说。
    #[test]
    fn not_up_to_date_hint_only_fires_on_known_pending_changes() {
        let mut app = crate::testbed::stub_app(false);
        // 测试台里 dir 0 = 服务器还有 5 项没拉下来，远端 r128
        let hint = app.not_up_to_date_hint(0).expect("有可更新项就要提示");
        assert!(hint.starts_with("未更新到最新版本"), "{hint}");
        assert!(hint.contains("r128") && hint.contains('5'), "{hint}");

        assert_eq!(app.not_up_to_date_hint(1), None, "已是最新就不提示");
        assert_eq!(app.not_up_to_date_hint(2), None, "还没读到过就不猜");
        assert_eq!(app.not_up_to_date_hint(99), None, "越界的目录不能 panic");

        // 远端版本号没读到时不带那半句，其余照旧
        app.dirs[0].remote_rev.clear();
        let hint = app.not_up_to_date_hint(0).expect("有可更新项就要提示");
        assert!(!hint.contains("远端已是"), "{hint}");
    }

    /// 提交页跑一帧，收下画出来的「文字 + 矩形」（同 testbed 里主页那套收法）。
    fn commit_frame(ctx: &egui::Context, app: &mut SvnApp) -> Vec<(String, egui::Rect)> {
        let out = ctx.run_ui(crate::testbed::base_input(), |ui| app.commit_page(ui));
        commit_drawn(out)
    }

    /// 在提交页上点一下（位置由上一帧的矩形给），再收一帧。
    fn commit_click(
        ctx: &egui::Context,
        app: &mut SvnApp,
        pos: egui::Pos2,
    ) -> Vec<(String, egui::Rect)> {
        let press = |pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let mut input = crate::testbed::base_input();
        input.events = vec![egui::Event::PointerMoved(pos), press(true), press(false)];
        let out = ctx.run_ui(input, |ui| app.commit_page(ui));
        commit_drawn(out)
    }

    fn commit_drawn(mut out: egui::FullOutput) -> Vec<(String, egui::Rect)> {
        // 测试里没有渲染器，字体增量不应用就得显式丢弃，否则 epaint 在 Drop 时 panic
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

    fn rect_of(drawn: &[(String, egui::Rect)], label: &str) -> egui::Rect {
        drawn
            .iter()
            .find(|(text, _)| text == label)
            .map(|(_, rect)| *rect)
            .unwrap_or_else(|| panic!("页面上没画出「{label}」"))
    }

    fn has(drawn: &[(String, egui::Rect)], label: &str) -> bool {
        drawn.iter().any(|(text, _)| text == label)
    }

    /// 这句要在提交页一直亮着（点它直接更新），不是只在按下提交那一瞬闪一下。
    #[test]
    fn commit_page_shows_the_not_up_to_date_warning() {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        crate::fonts::install_cjk(&ctx);
        let mut app = crate::testbed::stub_app(false);

        app.commit = Some(page_of(vec![Item::Modified], &[Item::Modified]));
        let drawn = commit_frame(&ctx, &mut app);
        let warning = drawn
            .iter()
            .find(|(text, _)| text.starts_with("未更新到最新版本"))
            .expect("服务器上还有 5 项没拉下来，提交页要亮这句");
        assert!(warning.0.contains("5 项改动没拉到本地"), "{}", warning.0);

        // 已是最新的目录（dir 1）页面上不该有这句
        app.commit = Some(CommitPage::new(1, "测试".to_owned()));
        if let Some(page) = app.commit.as_mut() {
            page.set_entries(vec![entry(Item::Modified, true)]);
        }
        let drawn = commit_frame(&ctx, &mut app);
        assert!(
            !drawn
                .iter()
                .any(|(text, _)| text.starts_with("未更新到最新版本")),
            "已是最新还提示就成了误报"
        );
    }

    /// 没更新到最新时点提交要被拦住：第一下只说后果，第二下才真的进提交流程。
    #[test]
    fn commit_button_gates_when_not_up_to_date() {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        crate::fonts::install_cjk(&ctx);
        let mut app = crate::testbed::stub_app(false);
        app.commit = Some(page_of(vec![Item::Modified], &[Item::Modified]));

        // 第一下：只确认，不起提交任务
        let drawn = commit_frame(&ctx, &mut app);
        let _ = commit_click(&ctx, &mut app, rect_of(&drawn, "提交 1 项").center());
        assert!(app.commit.as_ref().unwrap().confirm_outdated, "第一下该只拦住");
        assert!(!app.pool.has(Kind::Commit, 0), "拦住的这一下不能起提交任务");
        assert!(app.hint.contains("再点一次"), "{}", app.hint);

        // 按钮文案与退路按钮要再跑一帧才画得出来：这一帧的文案是帧首算好的
        let drawn = commit_frame(&ctx, &mut app);
        assert!(has(&drawn, "仍要提交 1 项？"), "按钮文案要换成确认口径");
        assert!(has(&drawn, "先更新到最新") && has(&drawn, "放弃提交"), "要给出退路");

        // 第二下：放行。测试台的 app 没有 svn.exe，所以走到的是「无法提交」那句——
        // 正是它说明这一下已经越过拦截、进了 spawn_commit
        let _ = commit_click(&ctx, &mut app, rect_of(&drawn, "仍要提交 1 项？").center());
        assert!(
            app.output
                .iter()
                .any(|line| line.text.contains("未更新到最新版本")),
            "放行时要往输出记录留痕：{:?}",
            app.output.iter().map(|line| line.text.clone()).collect::<Vec<_>>()
        );
        assert!(app.hint.contains("无法提交"), "第二下该越过拦截：{}", app.hint);
        assert!(!app.commit.as_ref().unwrap().confirm_outdated, "放行后确认态要清掉");
    }

    /// 拦住的那一下要能退出来：点「放弃提交」就松开，别第二次进来莫名其妙还是确认态。
    #[test]
    fn commit_gate_can_be_cancelled() {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        crate::fonts::install_cjk(&ctx);
        let mut app = crate::testbed::stub_app(false);
        app.commit = Some(page_of(vec![Item::Modified], &[Item::Modified]));

        let drawn = commit_frame(&ctx, &mut app);
        let _ = commit_click(&ctx, &mut app, rect_of(&drawn, "提交 1 项").center());
        let drawn = commit_frame(&ctx, &mut app);
        let _ = commit_click(&ctx, &mut app, rect_of(&drawn, "放弃提交").center());
        assert!(!app.commit.as_ref().unwrap().confirm_outdated, "放弃要真的松开");
        assert!(!app.pool.has(Kind::Commit, 0));

        // 松开之后按钮要回到普通文案，再点又能拦一次
        let drawn = commit_frame(&ctx, &mut app);
        let _ = commit_click(&ctx, &mut app, rect_of(&drawn, "提交 1 项").center());
        assert!(app.commit.as_ref().unwrap().confirm_outdated);
        app.commit.as_mut().unwrap().set_entries(vec![entry(Item::Modified, true)]);
        assert!(
            !app.commit.as_ref().unwrap().confirm_outdated,
            "重新读过清单就不能留着上一次的确认"
        );
    }
}

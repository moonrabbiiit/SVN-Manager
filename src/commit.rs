use std::path::Path;

use egui::{Align, Button, Color32, FontId, Frame, Layout, RichText, ScrollArea, TextEdit, Ui, Vec2};

use crate::jobs::{Data, Kind, Sink};
use crate::svn::{blocked_count, Item, StatusEntry, Svn};
use crate::{highlight, ink, Maintain, SvnApp};

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
        }
    }

    pub fn set_entries(&mut self, entries: Vec<StatusEntry>) {
        self.entries = entries;
        self.picked = usize::MAX;
        self.diff.clear();
        self.error.clear();
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
                // egui 0.36 的按钮不会根据底色自动调整文字颜色，浅色主题下需要自己同时指定底色与字色
                let (fill, text) = match (ui.visuals().dark_mode, blocked.is_none()) {
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
                };
                let button = egui::Button::new(
                    RichText::new(format!("提交 {picked_count} 项"))
                        .size(14.0)
                        .strong()
                        .color(text),
                )
                .fill(fill);
                let response = ui.add_sized(Vec2::new(126.0, 60.0), button);
                if response.clicked() {
                    match blocked {
                        Some(why) => self.hint(why),
                        None => {
                            let staging = if adds.is_empty() && deletes.is_empty() {
                                String::new()
                            } else {
                                format!("，先 add {} 项、delete {} 项", adds.len(), deletes.len())
                            };
                            self.hint(format!("正在提交 {picked_count} 项{staging} …"));
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
}

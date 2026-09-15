//! 主页：目录列表（详细整行 / 简约正方形卡片）、样式切换器、行内编辑器，
//! 以及卡片右下角那个收纳全部操作的「+」菜单。

use egui::{Align, Color32, Frame, Layout, RichText, ScrollArea, TextEdit, Ui, Vec2};
use egui::Key;

use crate::{ink, DirView, Maintain, SvnApp, UploadAll};

/// 简约形态下目录卡片的边长（逻辑点）：四角各摆一块内容（状态灯、计数标记、版本行、
/// 「+」），名称横排在正中；168 是让这几块互不相撞、名称还能放六七个汉字的最小尺寸
const CARD_SIZE: f32 = 168.0;

/// 卡片计数标记的底色：这个方向上没有待办时是实心绿，有待办换成更深的黄
const MARK_QUIET: Color32 = Color32::from_rgb(56, 168, 96);
const MARK_BUSY: Color32 = Color32::from_rgb(214, 154, 22);
/// 计数标记的字号与高度：取上一版（11.5 号字 + 19 高）缩 5% 再落整。
/// 17 = 10.9 号字的行高 15 + 上下各 1 的内边距；圆角由它取半向下推出来（胶囊）
const MARK_FONT: f32 = 10.9;
const MARK_H: f32 = 17.0;

/// 只换 alpha、不动 RGB：卡片上计数标记的底色 / 描边要的是「同一颜色淡一点」，
/// 而 gamma_multiply 会连 alpha 一起乘、把颜色本身也调淡。
fn tint(color: Color32, alpha: u8) -> Color32 {
    let [r, g, b, _] = color.to_array();
    Color32::from_rgba_unmultiplied(r, g, b, alpha)
}

/// 在指定矩形的左上角放一行文字，放不下就截断。
/// 不用 `Ui::put`：它走的是居中 + 两端对齐的布局，卡片里的名称和版本号要贴着左边摆。
fn card_text(ui: &mut Ui, rect: egui::Rect, text: RichText) -> egui::Response {
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Min))
            .sense(egui::Sense::hover()),
    );
    child.add(egui::Label::new(text).truncate())
}

impl SvnApp {
    // ------------------------------------------------------------ 主页面

    fn dir_row(&mut self, ui: &mut Ui, index: usize) {
        let dir = self.cfg.dirs[index].clone();
        let view = self.dirs.get(index).cloned().unwrap_or_default();
        let busy = self.pool.is_busy(index);
        let busy_label = self.pool.busy_label(index);
        let label = self.dir_label(index);
        let is_selected = self.selected == Some(index);

        let lamp = ink(
            ui,
            match view.remote {
                Some(true) => Color32::from_rgb(80, 200, 120),
                Some(false) => Color32::from_rgb(240, 100, 100),
                None if busy => Color32::from_rgb(240, 190, 70),
                None => Color32::from_gray(120),
            },
        );
        let row_fill = if ui.visuals().dark_mode {
            Color32::from_gray(38)
        } else {
            Color32::from_gray(236)
        };
        let frame = Frame::new()
            .inner_margin(7.0)
            .corner_radius(6.0)
            .fill(if is_selected {
                ui.visuals().selection.bg_fill
            } else {
                row_fill
            });
        frame.show(ui, |ui| {
            // 行内文字默认可选中，会抢占点击；关掉后整行空白处才能选中本行
            ui.style_mut().interaction.selectable_labels = false;
            ui.horizontal(|ui| {
                ui.label(RichText::new("●").size(15.0).color(lamp)).on_hover_text({
                    let mut tip = view.remote_msg.clone();
                    if tip.is_empty() {
                        tip = "尚未检测".to_owned();
                    }
                    if !view.checked_at.is_empty() {
                        tip.push_str(&format!("（{}）", view.checked_at));
                    }
                    tip
                });
                if self.edit_label == Some(index) {
                    self.alias_editor(ui, index);
                } else {
                    ui.label(RichText::new(&label).strong()).on_hover_text(
                        "别名可随时改：更多 → 修改别名（留空则显示文件夹名）",
                    );
                }
                ui.label(
                    RichText::new(&dir.path)
                        .size(12.5)
                        .color(ui.visuals().weak_text_color()),
                );
                if let Some(changed) = view.changed {
                    if changed > 0 {
                        ui.label(
                            RichText::new(format!("本地修改 {changed} 项"))
                                .size(12.0)
                                .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                        )
                        .on_hover_text(
                            "口径与「全部上传」一致：新增(?)、已丢失(!) 也算在内——\n\
                             全部上传时自动 svn add / svn delete 后一并提交，无需手动标记",
                        );
                    } else {
                        ui.label(
                            RichText::new("无本地修改")
                                .size(12.0)
                                .color(ink(ui, Color32::from_rgb(80, 200, 120))),
                        );
                    }
                }
                if let Some(blocked) = view.conflicts.filter(|count| *count > 0) {
                    // 冲突条目提交不了，所以不算进上面的「本地修改」；不单独标出来，
                    // 用户就只能进了提交页才知道这个目录卡住了
                    let hit = ui.add(
                        egui::Label::new(
                            RichText::new(format!("冲突 {blocked} 项"))
                                .size(12.0)
                                .strong()
                                .underline()
                                .color(ink(ui, Color32::from_rgb(255, 80, 160))),
                        )
                        .sense(egui::Sense::click()),
                    );
                    if hit.clicked() {
                        self.open_commit(index);
                    }
                    hit.on_hover_text(
                        "冲突、不完整这类必须人工处理的条目，解决之前这个目录提交不上去。\n\
                         点击打开该目录的提交页。",
                    );
                }
                if busy {
                    ui.spinner();
                    if let Some(text) = busy_label {
                        ui.label(RichText::new(text).weak().size(11.5));
                    }
                }
                // 六个按钮必须排在同一个 horizontal 里：不同按钮的文字混排高度略有差异
                // （例如含 ↑↓ 箭头时行高比纯中文略高），一旦把「移除 / 更多」和它们分成
                // 两组并列，两组就会按各自高度居中而错开约半个像素（实测 0.5px）。
                // 同一个 horizontal 内由同一个 Ui 摆放，中心线才完全一致。
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {

                    ui.horizontal(|ui| {
                        let confirm = self.confirm_remove == Some(index);
                        let mut button = egui::Button::new(if confirm {
                            RichText::new("确认移除?")
                                .strong()
                                .color(Color32::from_rgb(255, 240, 240))
                        } else {
                            RichText::new("移除")
                        });
                        if confirm {
                            button = button.fill(Color32::from_rgb(150, 60, 60));
                        }
                        if ui
                            .add_enabled(!busy, button)
                            .on_hover_text("仅从列表移除，不删除磁盘文件")
                            .clicked()
                        {
                            if confirm {
                                self.remove_directory(index);
                            } else {
                                self.confirm_remove = Some(index);
                                self.hint("再次点击「确认移除」可把该目录从列表移除（不会删除文件）");
                            }
                        }
                        ui.menu_button("更多", |ui| self.more_menu(ui, index, busy, &view));
                        if ui.button("打开目录").clicked() {
                            self.open_folder(index);
                        }
                        if ui.button("历史").clicked() {
                            self.selected = Some(index);
                            self.open_history(index);
                        }
                        if ui.button("↑ 上传").clicked() {
                            self.selected = Some(index);
                            self.open_commit(index);
                        }
                        if ui.button("↓ 更新").clicked() {
                            self.selected = Some(index);
                            self.confirm_remove = None;
                            self.spawn_update(index);
                        }
                    });
                });
            });
            // 名称筛选的行内编辑器：Enter 或点别处提交，Esc 取消（交互同上面的别名编辑）
            if self.edit_filter == Some(index) {
                self.filter_editor(ui, index);
            }
            ui.horizontal(|ui| {
                match &view.info {
                    Some(info) => {
                        ui.label(
                            RichText::new(&info.url)
                                .size(12.0)
                                .color(ink(ui, Color32::from_rgb(140, 205, 255))),
                        );
                        // 「本地」按内容算：没有待更新项就说明工作副本已经包含 HEAD 的全部
                        // 改动，根目录的版本号因为混合版本停在旧值，不代表内容还是旧的
                        let up_to_date =
                            view.out_of_date == Some(0) && !view.remote_rev.is_empty();
                        ui.label(
                            RichText::new(format!(
                                "本地 r{}",
                                if up_to_date { &view.remote_rev } else { &info.revision }
                            ))
                            .size(12.0)
                            .weak(),
                        );
                        if up_to_date {
                            ui.label(
                                RichText::new("已是最新")
                                    .size(12.0)
                                    .color(ink(ui, Color32::from_rgb(80, 200, 120))),
                            );
                        } else if let Some(pending) =
                            view.out_of_date.filter(|count| *count > 0)
                        {
                            ui.label(
                                RichText::new(format!(
                                    "远端 r{}（可更新 {} 项）",
                                    view.remote_rev, pending
                                ))
                                .size(12.0)
                                .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                            );
                        }
                        // 优先显示服务器上的最后提交（检测时从仓库 URL 读取）；
                        // 服务器不可达时才退回本地 BASE 的信息，并标注来源
                        let from_server = !view.last_rev.is_empty() || !view.last_author.is_empty();
                        let (last_rev, last_author, last_date) = if from_server {
                            (view.last_rev.clone(), view.last_author.clone(), view.last_date.clone())
                        } else {
                            (info.revision.clone(), info.last_author.clone(), info.last_date.clone())
                        };
                        if !last_date.is_empty() {
                            ui.label(
                                RichText::new(format!(
                                    "{}最后提交 r{last_rev} · {last_author} · {last_date}",
                                    if from_server { "服务器" } else { "本地" }
                                ))
                                .size(12.0)
                                .weak(),
                            );
                        }
                        if !info.repos_root.is_empty() {
                            if let Some(host) = info.repos_root.strip_prefix("https://").or(info.repos_root.strip_prefix("http://")) {
                                ui.label(RichText::new(format!("仓库 {}", host.split('/').next().unwrap_or(""))).size(12.0).weak());
                            }
                        }
                    }
                    None => {
                        ui.label(
                            RichText::new(if view.checked_at.is_empty() { "未检测" } else { "非工作副本或无法读取" })
                                .size(12.0)
                                .color(ink(ui, Color32::from_rgb(240, 100, 100))),
                        );
                    }
                }
            });
        });
    }

    // ------------------------------------------------------------ 简约样式（正方形卡片）

    /// 「更多」菜单的内容。详细模式的目录行直接弹它，简约模式的「+」菜单把它当作二级菜单，
    /// 两边共用一份，免得改了这处漏了那处
    fn more_menu(&mut self, ui: &mut Ui, index: usize, busy: bool, view: &DirView) {
        if ui.button("修改别名").clicked() {
            self.edit_label = Some(index);
            self.label_buf = self.cfg.dirs[index].label.clone();
            self.label_focus = true;
            ui.close();
        }
        if ui
            .button("修改仓库地址")
            .on_hover_text("服务器换地址 / 换端口后，把工作副本指到新的仓库地址，只改元数据不动文件")
            .clicked()
        {
            self.open_relocate(index);
            ui.close();
        }
        if ui
            .button(if self.cfg.dirs[index].bc_target.is_empty() {
                "⚖ 绑定对比对象"
            } else {
                "⚖ 重新绑定对比对象"
            })
            .on_hover_text("给这个目录绑定 Beyond Compare 对比的另一侧（例如服务器上的 *_server 副本），打开时就不靠文件夹名去猜记录")
            .clicked()
        {
            if let Some(target) = rfd::FileDialog::new()
                .set_title("选择对比对象目录")
                .pick_folder()
            {
                self.cfg.dirs[index].bc_target = target.display().to_string();
                self.persist();
                self.hint(format!("已绑定对比对象：{}", self.cfg.dirs[index].bc_target));
                ui.close();
            }
        }
        if !self.cfg.dirs[index].bc_target.is_empty() {
            ui.label(
                RichText::new(format!("已绑定：{}", self.cfg.dirs[index].bc_target))
                    .weak()
                    .size(11.5),
            );
            if ui.button("✖ 解除绑定").clicked() {
                self.cfg.dirs[index].bc_target.clear();
                self.persist();
                self.hint("已解除绑定，Beyond Compare 退回按对比记录匹配");
                ui.close();
            }
        }
        ui.separator();
        // 对比筛选条件的编辑器放在目录行里，不放菜单里：
        // menu_button 的菜单点任何地方都会收起，输入框一点就没了
        if ui
            .button(if self.cfg.dirs[index].bc_filter.is_empty() {
                "设置对比筛选条件"
            } else {
                "修改对比筛选条件"
            })
            .on_hover_text("文件夹对比的对比筛选条件，存在本程序配置里，不依赖本机 Beyond Compare 记录；换机器、换人也能一致地带出")
            .clicked()
        {
            self.edit_filter = Some(index);
            self.filter_buf = self.cfg.dirs[index].bc_filter.clone();
            self.filter_focus = true;
            ui.close();
        }
        ui.separator();
        ui.add_enabled_ui(!busy, |ui| {
            if ui.button(Maintain::Cleanup.label()).clicked() {
                self.spawn_maintain(index, Maintain::Cleanup);
                ui.close();
            }
            if ui.button(Maintain::Resolve.label()).clicked() {
                self.spawn_maintain(index, Maintain::Resolve);
                ui.close();
            }
        });
        if ui.button("▲ 上移").clicked() {
            self.move_directory(index, -1);
            ui.close();
        }
        if ui.button("▼ 下移").clicked() {
            self.move_directory(index, 1);
            ui.close();
        }
        if ui.button("复制仓库 URL").clicked() {
            match view.info.clone() {
                Some(info) => {
                    ui.ctx().copy_text(info.url);
                    self.hint("已复制仓库地址到剪贴板");
                }
                None => self.hint("尚未取得仓库地址，请先刷新"),
            }
            ui.close();
        }
    }

    /// 别名输入框：详细模式画在目录行的名称位置，简约模式画在列表上方那一行
    fn alias_editor(&mut self, ui: &mut Ui, index: usize) {
        let field = ui.add(
            TextEdit::singleline(&mut self.label_buf)
                .id_salt(("alias", index))
                .desired_width(240.0)
                .hint_text("别名，留空则显示文件夹名"),
        );
        if self.label_focus {
            field.request_focus();
            self.label_focus = false;
        }
        let enter = field.has_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        let esc = field.has_focus() && ui.input(|i| i.key_pressed(Key::Escape));
        if esc {
            self.edit_label = None;
            self.hint("已取消修改别名");
        } else if enter || field.lost_focus() {
            let text = self.label_buf.trim().to_owned();
            self.edit_label = None;
            if self.cfg.dirs[index].label != text {
                self.cfg.dirs[index].label.clone_from(&text);
                self.persist();
                self.hint(if text.is_empty() {
                    "已清除别名，列表改为显示文件夹名".to_owned()
                } else {
                    format!("别名已改为：{text}")
                });
            }
        }
    }

    /// 对比筛选条件的输入框（带前缀说明），位置同上
    fn filter_editor(&mut self, ui: &mut Ui, index: usize) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("对比筛选条件：").size(12.0));
            let field = ui.add(
                TextEdit::singleline(&mut self.filter_buf)
                    .id_salt(("bc_filter", index))
                    .desired_width(320.0)
                    .hint_text("如 -*.iml;-*.classpath，多个用分号隔开；留空则走 BC 记录里的筛选"),
            );
            if self.filter_focus {
                field.request_focus();
                self.filter_focus = false;
            }
            let enter = field.has_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            let esc = field.has_focus() && ui.input(|i| i.key_pressed(Key::Escape));
            if esc {
                self.edit_filter = None;
                self.hint("已取消修改对比筛选条件");
            } else if enter || field.lost_focus() {
                self.edit_filter = None;
                let text = self.filter_buf.trim().to_owned();
                if self.cfg.dirs[index].bc_filter != text {
                    self.cfg.dirs[index].bc_filter.clone_from(&text);
                    self.persist();
                }
                self.hint(if text.is_empty() {
                    "已清空对比筛选条件，将退回 Beyond Compare 记录里的值".to_owned()
                } else {
                    format!("对比筛选条件已保存：{text}")
                });
            }
        });
    }

    /// 主页最右侧的样式切换器：两个选项同在一只圆角长方形里，当前那项被一个框圈住。
    /// 这个 egui 分支既没有 Switch 也没有 SelectableLabel，整只手画。
    fn compact_switch(&mut self, ui: &mut Ui) {
        let on = self.cfg.home_compact;
        let dark = ui.visuals().dark_mode;
        let pad = 2.0;
        let cell = Vec2::new(46.0, 22.0);
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(cell.x * 2.0 + pad * 2.0, cell.y + pad * 2.0),
            egui::Sense::hover(),
        );
        let cell_rect = |index: usize| {
            egui::Rect::from_min_size(
                egui::pos2(rect.min.x + pad + index as f32 * cell.x, rect.min.y + pad),
                cell,
            )
        };
        let accent = ui.visuals().selection.bg_fill;
        let painter = ui.painter();
        painter.rect_filled(rect, 7.0, Color32::from_gray(if dark { 30 } else { 224 }));
        painter.rect_stroke(
            rect,
            7.0,
            egui::Stroke::new(1.0, Color32::from_gray(if dark { 66 } else { 196 })),
            egui::StrokeKind::Inside,
        );
        let current = cell_rect(on as usize);
        painter.rect_filled(current, 5.0, tint(accent, if dark { 64 } else { 34 }));
        painter.rect_stroke(
            current,
            5.0,
            egui::Stroke::new(1.5, ink(ui, accent)),
            egui::StrokeKind::Inside,
        );
        let strong = ui.visuals().strong_text_color();
        let weak = ui.visuals().weak_text_color();
        let tip = "切换主页目录列表的样式\n详细：整行 + 一排按钮，信息全\n简约：正方形卡片，只留名称和版本，按钮收进「+」";
        for (index, name) in ["详细", "简约"].into_iter().enumerate() {
            let cell = cell_rect(index);
            // `place` 只摆这一格、不动父 Ui 的光标，而且它的布局本来就是居中的
            ui.place(
                cell,
                egui::Label::new(
                    RichText::new(name)
                        .size(12.0)
                        .color(if (index == 1) == on { strong } else { weak }),
                ),
            );
            // 感应区压在字之上：点当前那项什么都不做，点另一侧才切
            if ui
                .interact(cell, egui::Id::new(("home_style", index)), egui::Sense::click())
                .on_hover_text(tip)
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
                && (index == 1) != on
            {
                self.cfg.home_compact = index == 1;
                self.persist();
                self.hint(if self.cfg.home_compact {
                    "已切到简约样式：目录显示为卡片"
                } else {
                    "已切回详细样式"
                });
            }
        }
    }

    /// 卡片里塞不下行内输入框，简约形态下别名 / 对比筛选条件单独占一行，摆在列表上方
    fn compact_editors(&mut self, ui: &mut Ui) {
        let total = self.cfg.dirs.len();
        let fill = Color32::from_gray(if ui.visuals().dark_mode { 34 } else { 240 });
        if let Some(index) = self.edit_label.filter(|i| *i < total) {
            let label = self.dir_label(index);
            Frame::new()
                .inner_margin(6.0)
                .corner_radius(6.0)
                .fill(fill)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("修改别名 · {label}：")).size(12.0));
                        self.alias_editor(ui, index);
                    });
                });
            ui.add_space(4.0);
        }
        if let Some(index) = self.edit_filter.filter(|i| *i < total) {
            let label = self.dir_label(index);
            Frame::new()
                .inner_margin(6.0)
                .corner_radius(6.0)
                .fill(fill)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{label}")).size(12.0));
                        self.filter_editor(ui, index);
                    });
                });
            ui.add_space(4.0);
        }
    }

    /// 简约形态的网格：列数按可用宽度算，一横行一横行地摆正方形卡片
    fn dir_cards(&mut self, ui: &mut Ui) {
        let gap = 10.0;
        let cols = (((ui.available_width() + gap) / (CARD_SIZE + gap)).floor() as usize).max(1);
        ScrollArea::vertical()
            .id_salt("dir_cards")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let total = self.cfg.dirs.len();
                let mut start = 0;
                while start < total {
                    ui.horizontal(|ui| {
                        for offset in 0..cols {
                            let index = start + offset;
                            // 行内「移除」会当场缩短 cfg.dirs，每格都重新比一次上界
                            if index >= total {
                                break;
                            }
                            let (clicked, double_clicked) = self.dir_card(ui, index);
                            if clicked {
                                self.selected = Some(index);
                                self.confirm_remove = None;
                            }
                            if double_clicked {
                                self.open_folder(index);
                            }
                            if offset + 1 < cols && index + 1 < total {
                                ui.add_space(gap);
                            }
                        }
                    });
                    ui.add_space(gap);
                    start += cols;
                }
            });
    }

    /// 简约形态下的一张卡片：圆角正方形，检测状态灯在左上、目录名称在正中、
    /// 两行版本在左下、两枚计数标记在右上、圆形「+」在右下。返回 (点中卡片, 双击卡片)。
    fn dir_card(&mut self, ui: &mut Ui, index: usize) -> (bool, bool) {
        let view = self.dirs.get(index).cloned().unwrap_or_default();
        let busy = self.pool.is_busy(index);
        let label = self.dir_label(index);
        let path = self.cfg.dirs[index].path.clone();
        let is_selected = self.selected == Some(index);
        let dark = ui.visuals().dark_mode;
        let pending = view.out_of_date.unwrap_or(0);
        let changed = view.changed.unwrap_or(0);
        let conflicts = view.conflicts.unwrap_or(0);
        let weak = ui.visuals().weak_text_color();

        let (_id, rect) = ui.allocate_space(Vec2::splat(CARD_SIZE));
        // 整张卡片的感应区先注册，里面的标记和「+」都注册在它之后：
        // 点在控件上时控件优先命中，点在空白处才是「选中这一张」
        let hit = ui.interact(rect, egui::Id::new(("dir_card", index)), egui::Sense::click());
        // 边框把状态再标一遍，余光里也能看出来：冲突最要紧（不人工处理就提交不上去），
        // 其次是连不上服务器，再次是正在跑任务
        let border = if conflicts > 0 {
            ink(ui, Color32::from_rgb(255, 80, 160))
        } else if view.remote == Some(false) {
            ink(ui, Color32::from_rgb(240, 100, 100))
        } else if busy {
            ink(ui, Color32::from_rgb(240, 190, 70))
        } else {
            Color32::from_gray(if dark { 60 } else { 206 })
        };
        ui.painter().rect_filled(
            rect,
            10.0,
            if is_selected {
                ui.visuals().selection.bg_fill
            } else {
                Color32::from_gray(if dark { 38 } else { 236 })
            },
        );
        ui.painter().rect_stroke(
            rect,
            10.0,
            egui::Stroke::new(if is_selected { 1.6 } else { 1.0 }, border),
            egui::StrokeKind::Inside,
        );
        let inner = rect.shrink(10.0);
        // 左上角：检测状态灯（配色和悬停说明同详细模式的那枚 ●）
        let lamp = ink(
            ui,
            match view.remote {
                Some(true) => Color32::from_rgb(80, 200, 120),
                Some(false) => Color32::from_rgb(240, 100, 100),
                None if busy => Color32::from_rgb(240, 190, 70),
                None => Color32::from_gray(120),
            },
        );
        ui.place(
            egui::Rect::from_min_size(inner.min, Vec2::splat(20.0)),
            egui::Label::new(RichText::new("●").size(15.0).color(lamp)),
        );
        // 目录名称摆在卡片正中
        ui.place(
            egui::Rect::from_center_size(rect.center(), Vec2::new(inner.width() - 8.0, 26.0)),
            egui::Label::new(RichText::new(&label).strong().size(13.5)).truncate(),
        );
        if busy {
            // 加载只留一枚转圈：卡片地方小，任务文字挤在名称上面不好看，进度看边框颜色就够了。
            // 颜色显式给：跟着主题走的话浅色底上那圈几乎看不见
            egui::Spinner::new()
                .size(15.0)
                .color(ink(ui, Color32::from_rgb(240, 190, 70)))
                .paint_at(
                    ui,
                    egui::Rect::from_min_size(
                        egui::pos2(inner.min.x + 22.0, inner.min.y + 3.0),
                        Vec2::splat(15.0),
                    ),
                );
        }
        // 左下两行：当前版本 / 远端版本，口径与详细模式一致
        let up_to_date = view.out_of_date == Some(0) && !view.remote_rev.is_empty();
        let local = match &view.info {
            Some(info) => format!(
                "本地 r{}",
                if up_to_date { &view.remote_rev } else { &info.revision }
            ),
            None => "本地 —".to_owned(),
        };
        let remote = if view.remote_rev.is_empty() {
            "远端 —".to_owned()
        } else {
            format!("远端 r{}", view.remote_rev)
        };
        // 远端有更新时，远端那行用详细模式里「远端 rX（可更新 N 项）」的黄色
        let remote_color = if pending > 0 {
            ink(ui, Color32::from_rgb(240, 190, 70))
        } else {
            weak
        };
        for (y, text, color) in [
            (inner.max.y - 34.0, local.as_str(), weak),
            (inner.max.y - 14.0, remote.as_str(), remote_color),
        ] {
            card_text(
                ui,
                egui::Rect::from_min_size(
                    egui::pos2(inner.min.x, y),
                    Vec2::new(inner.width() - 36.0, 18.0),
                ),
                RichText::new(text).size(10.5).color(color),
            );
        }
        // 右上角两枚计数标记：上=服务器可更新，下=本地待提交。
        // 用右对齐的竖排子 Ui，标记宽度跟着数字走、右边缘始终贴着卡片内侧
        let mut band = ui.new_child(
            egui::UiBuilder::new()
                .id(egui::Id::new(("dir_card_band", index)))
                .max_rect(egui::Rect::from_min_size(
                    inner.min,
                    Vec2::new(inner.width(), 44.0),
                ))
                .layout(Layout::top_down(Align::Max))
                .sense(egui::Sense::hover()),
        );
        band.spacing_mut().item_spacing.y = 4.0;
        let shown = |count: usize| {
            if count > 999 {
                "999+".to_owned()
            } else {
                count.to_string()
            }
        };
        // 两枚标记只靠箭头区分方向：↓ 是从服务器拉下来，↑ 是要传上去
        let update_clicked = self.count_badge(
            &mut band,
            &format!("↓{}", shown(pending)),
            pending > 0,
            &format!("服务器上有 {pending} 项本地还没更新\n点击立即更新该目录"),
        );
        let commit_clicked = self.count_badge(
            &mut band,
            &format!("↑{}", shown(changed)),
            changed > 0,
            &format!(
                "本地有 {changed} 项待提交（口径同「全部上传」）\n点击打开该目录的提交页"
            ),
        );
        drop(band);
        // 右下角带圆圈的「+」：详细模式摊在行尾的那些按钮全收进这里
        let plus = ui.place(
            egui::Rect::from_min_size(
                egui::pos2(inner.max.x - 30.0, inner.max.y - 30.0),
                Vec2::splat(30.0),
            ),
            egui::Button::new(
                RichText::new("+")
                    .size(19.0)
                    .color(Color32::from_rgb(246, 249, 253)),
            )
            .min_size(Vec2::splat(30.0))
            .corner_radius(15)
            .fill(ink(ui, Color32::from_rgb(52, 122, 196)))
            .stroke(egui::Stroke::new(
                1.0,
                ink(ui, Color32::from_rgb(120, 190, 240)),
            )),
        );
        plus.clone().on_hover_text("该目录的全部操作");
        egui::Popup::menu(&plus).show(|ui| self.card_menu(ui, index, busy, &view));

        let mut tip = format!("{label}\n{path}\n");
        if let Some(info) = &view.info {
            tip.push_str(&format!("仓库：{}\n", info.url));
        }
        tip.push_str(&format!("{local} · {remote}\n"));
        tip.push_str(&format!(
            "服务器可更新 {pending} 项 · 本地待提交 {changed} 项\n"
        ));
        if conflicts > 0 {
            tip.push_str(&format!("冲突 / 不完整 {conflicts} 项，必须先人工处理\n"));
        }
        if !view.remote_msg.is_empty() {
            tip.push_str(&view.remote_msg);
            if !view.checked_at.is_empty() {
                tip.push_str(&format!("（{}）", view.checked_at));
            }
        }
        let hit = hit.on_hover_ui(|ui| {
            ui.add(
                egui::Label::new(RichText::new(&tip).size(12.0))
                    .wrap_mode(egui::TextWrapMode::Extend),
            );
        });
        if update_clicked && !busy {
            self.selected = Some(index);
            self.spawn_update(index);
        }
        if commit_clicked {
            self.selected = Some(index);
            self.open_commit(index);
        }
        (hit.clicked(), hit.double_clicked())
    }

    /// 卡片右上角的圆角长方形计数标记：这个方向没有待办就是实心绿，有待办换成更深的黄。
    /// 不走 `Button`：它的底色 / 描边 / 圆角 / 文字色分 inactive、hovered、active 三套 visuals，
    /// 鼠标进出就换一套，看着就是标记跳一下；`Frame` 不吃控件状态，进出画出来完全一样。
    fn count_badge(&mut self, ui: &mut Ui, text: &str, active: bool, tip: &str) -> bool {
        let fill = if active { MARK_BUSY } else { MARK_QUIET };
        let [r, g, b, _] = fill.to_array();
        // 数字颜色按底色亮度取近黑 / 近白，深浅两套主题都不用各调一遍
        let text_color = if 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32 > 150.0 {
            Color32::from_rgb(26, 20, 4)
        } else {
            Color32::from_rgb(250, 252, 250)
        };
        let edge = Color32::from_rgb(
            (r as f32 * 0.68) as u8,
            (g as f32 * 0.68) as u8,
            (b as f32 * 0.68) as u8,
        );
        let chip = Frame::new()
            .fill(fill)
            .stroke(egui::Stroke::new(1.0, edge))
            .corner_radius((MARK_H / 2.0).floor())
            .inner_margin(egui::Margin::symmetric(5, 1))
            .show(ui, |ui| {
                ui.add(
                    egui::Label::new(RichText::new(text).size(MARK_FONT).color(text_color))
                        .selectable(false),
                );
            });
        // 感应区最后注册，压在字之上；0 的那枚点了没意义，所以按下也不响应
        let area = ui.interact(
            chip.response.rect,
            ui.id().with(("dir_card_badge", text)),
            egui::Sense::click(),
        );
        area.on_hover_text(tip).clicked() && active
    }

    /// 「+」里收纳的操作菜单：一级是四个常用动作，「更多」下面接详细模式那份菜单
    fn card_menu(&mut self, ui: &mut Ui, index: usize, busy: bool, view: &DirView) {
        if ui.button("↓ 更新").clicked() {
            self.selected = Some(index);
            self.confirm_remove = None;
            self.spawn_update(index);
            ui.close();
        }
        if ui.button("↑ 上传").clicked() {
            self.selected = Some(index);
            self.open_commit(index);
            ui.close();
        }
        if ui.button("历史").clicked() {
            self.selected = Some(index);
            self.open_history(index);
            ui.close();
        }
        if ui.button("打开目录").clicked() {
            self.open_folder(index);
            ui.close();
        }
        ui.separator();
        ui.menu_button("更多", |ui| self.more_menu(ui, index, busy, view));
        ui.separator();
        // 移除仍要点两次确认：菜单一点就收，所以第二下要重新展开点「确认移除」
        let confirm = self.confirm_remove == Some(index);
        let mut button = egui::Button::new(if confirm {
            RichText::new("确认移除?")
                .strong()
                .color(Color32::from_rgb(255, 240, 240))
        } else {
            RichText::new("移除")
        });
        if confirm {
            button = button.fill(Color32::from_rgb(150, 60, 60));
        }
        if ui
            .add_enabled(!busy, button)
            .on_hover_text("仅从列表移除，不删除磁盘文件")
            .clicked()
        {
            if confirm {
                self.remove_directory(index);
            } else {
                self.confirm_remove = Some(index);
                self.hint("再次展开「+」点「确认移除」可把该目录从列表移除（不会删除文件）");
            }
            ui.close();
        }
    }

    pub fn main_page(&mut self, ui: &mut Ui) {
        Frame::new()
            .inner_margin(8.0)
            .corner_radius(6.0)
            .fill(if ui.visuals().dark_mode {
                Color32::from_gray(34)
            } else {
                Color32::from_gray(240)
            })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("添加目录").strong());
                    let width = (ui.available_width() - 250.0).max(200.0);
                    let field = ui.add(
                        TextEdit::singleline(&mut self.new_path)
                            .hint_text(r"工作副本目录，例如 C:\Test")
                            .desired_width(width),
                    );
                    let enter = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    if ui.button("选择文件夹…").clicked() {
                        if let Some(folder) = rfd::FileDialog::new()
                            .set_title("选择 SVN 工作副本目录")
                            .pick_folder()
                        {
                            self.new_path = folder.display().to_string();
                        }
                    }
                    if enter || ui.button("＋ 添加").clicked() {
                        let path = self.new_path.clone();
                        self.add_directory(path);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("别名(可选)").weak().size(12.0));
                    ui.add(TextEdit::singleline(&mut self.new_label).desired_width(200.0).hint_text("列表中显示的名称"));
                    ui.label(RichText::new("也可以直接把文件夹拖到窗口里添加").weak().size(12.0));
                });
            });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let count = self.cfg.dirs.len();
            let changes: usize = self.dirs.iter().filter_map(|d| d.changed).sum();
            ui.label(RichText::new(format!("目录列表（{count} 个，本地修改合计 {changes} 项）")).strong());
            // 目录多的时候冲突标记要滚动才看得见，这里聚合一份：不用滚就知道有没有卡住
            let blocked: Vec<(usize, usize)> = self
                .dirs
                .iter()
                .enumerate()
                .filter_map(|(index, view)| {
                    Some((index, view.conflicts.filter(|count| *count > 0)?))
                })
                .collect();
            if !blocked.is_empty() {
                let total: usize = blocked.iter().map(|(_, count)| count).sum();
                let list = blocked
                    .iter()
                    .map(|(index, count)| format!("{}：{count} 项", self.dir_label(*index)))
                    .collect::<Vec<_>>()
                    .join("\n");
                let first = blocked[0].0;
                let hit = ui.add(
                    egui::Label::new(
                        RichText::new(format!("，{total} 项冲突待处理"))
                            .size(12.0)
                            .strong()
                            .underline()
                            .color(ink(ui, Color32::from_rgb(255, 80, 160))),
                    )
                    .sense(egui::Sense::click()),
                );
                if hit.clicked() {
                    self.open_commit(first);
                }
                hit.on_hover_text(format!(
                    "必须人工处理才能继续提交的条目（冲突 / 不完整），按目录列：\n{list}\n\n点击打开第一个有冲突目录的提交页。"
                ));
            }
            ui.label(
                RichText::new(if self.cfg.home_compact {
                    "单击卡片选中，双击打开目录"
                } else {
                    "单击整行选中，双击打开目录"
                })
                .weak()
                .size(11.5),
            );
            if ui.button("全部检测").clicked() {
                self.hint("正在检测全部目录 …");
                self.spawn_all_refresh();
            }
            if ui.button("全部更新").clicked() {
                for index in 0..self.cfg.dirs.len() {
                    self.spawn_update(index);
                }
            }
            if ui
                .button("全部上传")
                .on_hover_text("所有目录用同一条说明一次提交（会改动服务器，需再点一次确认）")
                .clicked()
            {
                self.upload_all = Some(UploadAll {
                    message: String::new(),
                    confirm: false,
                    focus: true,
                    // 打开时默认全选，想排除谁就在窗口里去掉勾
                    checked: vec![true; self.cfg.dirs.len()],
                    detail: None,
                });
            }
            // 子布局从右往左排，切换器才能顶在这一行的最右侧
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                self.compact_switch(ui);
            });
        });
        if self.cfg.dirs.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("还没有目录").size(18.0).weak());
                ui.label(RichText::new("在上方输入或选择 SVN 工作副本目录，然后回车添加").weak());
            });
            return;
        }
        if self.cfg.home_compact {
            self.compact_editors(ui);
            self.dir_cards(ui);
            return;
        }
        ScrollArea::vertical()
            .id_salt("dir_list")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 5.0;
                // 行内「移除」会当场修改 cfg.dirs，因此每轮都重新取长度，避免越界
                let mut index = 0;
                while index < self.cfg.dirs.len() {
                    // 整行可点选中：scope 的点击感应区注册在行内所有控件之下，
                    // 因此行内按钮、菜单依旧优先命中，不会被抢走；
                    // 用固定 id 保证这一行的感应区每帧都是同一个控件（目录顺序变化也不影响）
                    let row_hit = ui
                        .scope_builder(
                            egui::UiBuilder::new()
                                .id(egui::Id::new(("dir_row", index)))
                                .sense(egui::Sense::click()),
                            |ui| self.dir_row(ui, index),
                        )
                        .response;
                    if row_hit.clicked() {
                        self.selected = Some(index);
                        self.confirm_remove = None;
                    }
                    if row_hit.double_clicked() {
                        self.open_folder(index);
                    }
                    index += 1;
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::jobs::{Data, Kind};
    use crate::testbed::{base_input, Stage};
    use egui::{Pos2, Rect, ThemePreference};

    #[test]
    fn switch_toggles_between_detailed_and_compact() {
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("svn_manager_home_style_test"),
        );
        let mut stage = Stage::new(false);
        let texts = stage.step(None);
        // 详细模式：整行的按钮都在，卡片的关键字一个都没有
        assert!(Stage::rect_of(&texts, "↓ 更新").is_some(), "默认应该是详细样式");
        assert!(Stage::rect_of(&texts, "+").is_none());

        // 切换器自带两格，点哪一格就切到哪一档
        let to_compact = Stage::rect_of(&texts, "简约").expect("切换器里应有「简约」那格");
        stage.step(Some(to_compact.center()));
        assert!(stage.app.cfg.home_compact, "点「简约」应该切成卡片样式");
        let texts = stage.step(None);
        assert!(Stage::rect_of(&texts, "↓ 更新").is_none(), "简约模式下六个按钮都收起来了");
        assert_eq!(Stage::rect_of(&texts, "+").map(|_| 1).unwrap_or(0), 1, "每张卡片一个「+」");

        let to_detail = Stage::rect_of(&texts, "详细").expect("切换器里应有「详细」那格");
        stage.step(Some(to_detail.center()));
        assert!(!stage.app.cfg.home_compact, "点「详细」应该切回整行样式");
        let saved = std::fs::read_to_string(config::config_file()).unwrap_or_default();
        assert!(saved.contains("home_compact"), "样式选择要写进配置文件");
    }

    /// 切换器是可点的，鼠标停上去要换成手型指针
    #[test]
    fn style_switch_points_with_a_hand_cursor() {
        let mut stage = Stage::new(false);
        let texts = stage.step(None);
        let cell = Stage::rect_of(&texts, "详细").expect("切换器的「详细」那一格");
        let Stage { ctx, app } = &mut stage;
        let mut input = base_input();
        input.events = vec![egui::Event::PointerMoved(cell.center())];
        let mut out = ctx.run_ui(input, |ui| app.main_page(ui));
        let cursor = out.platform_output.cursor_icon;
        out.textures_delta.clear();
        assert_eq!(cursor, egui::CursorIcon::PointingHand, "停在切换器上应该是手型指针");
    }

    /// 卡片是手摆的四角布局，只有跑一帧才知道文字有没有叠在一起、计数有没有真画出来
    #[test]
    fn compact_card_paints_versions_and_counts() {
        let mut stage = Stage::new(true);
        let texts = stage.step(None);
        for label in [
            "HRP",
            "本地 r120",
            "远端 r128",
            "↓5",
            "↑3",
            // 无待更新项时本地版本号按远端显示
            "本地 r200",
            "远端 r200",
            "↓0",
            "↑0",
            // 没检测到信息时的占位；1287 项要压成 999+ 才不会把标记撑出卡片
            "本地 —",
            "远端 —",
            "↑999+",
            "empty",
        ] {
            assert!(
                Stage::rect_of(&texts, label).is_some(),
                "卡片上应该画得出「{label}」，实际有：{:?}",
                texts.iter().map(|(t, _)| t).collect::<Vec<_>>()
            );
        }
        // 名称超长时按矩形宽度截断。注意 galley 里存的仍是原始整串文字，
        // 只能量画出来的宽度：居中的名称区宽 = 148 - 左右各留 4 = 140
        let long = "这是一个故意写得很长的目录别名用来验证截断";
        let wide = texts
            .iter()
            .find(|(text, _)| text == long)
            .map(|(_, rect)| rect.width())
            .expect("超长别名那一行总得画出来");
        assert!(
            wide <= 142.0,
            "超长别名没被截进名称区，实际画了 {wide:.0} 宽"
        );

        // 任何两段文字都不许互相压字（正方形卡片里手摆的块最容易挤在名称和标记之间）
        let mut collided: Vec<(&str, &str)> = Vec::new();
        for (i, (left_text, left_rect)) in texts.iter().enumerate() {
            for (right_text, right_rect) in texts.iter().skip(i + 1) {
                if left_rect.intersects(*right_rect) {
                    collided.push((left_text, right_text));
                }
            }
        }
        assert!(collided.is_empty(), "文字重叠：{collided:?}");

        // 卡片必须是正方形：名称、版本、标记、+ 全按边长排，画出来的块歪了说明算错
        let squares: Vec<f32> = texts
            .iter()
            .filter(|(text, _)| text == "+")
            .map(|(_, rect)| rect.height())
            .collect();
        assert_eq!(squares.len(), 4, "四张卡片四个「+」");
    }

    /// 形态要求逐项落地：名称居中、状态灯在左上、两枚淡绿标记一上一下右对齐、「+」是个圆
    #[test]
    fn compact_card_shape_and_corner_layout() {
        let mut stage = Stage::new(true);
        let Stage { ctx, app } = &mut stage;
        let mut out = ctx.run_ui(base_input(), |ui| app.main_page(ui));
        // 没有渲染器，字体增量不应用就得显式丢弃，否则 epaint 在 Drop 时 panic
        out.textures_delta.clear();
        let rects: Vec<(egui::Rect, u8, Color32, Color32)> = out
            .shapes
            .iter()
            .filter_map(|item| match &item.shape {
                egui::Shape::Rect(rect) => Some((
                    rect.rect,
                    rect.corner_radius.nw,
                    rect.fill,
                    rect.stroke.color,
                )),
                _ => None,
            })
            .collect();
        // 文字要连排版原点和落笔颜色一起收：`visual_bounding_rect` 量的是字形实际占的位子，
        // 不同字号的左右侧相机不一样，拿它比边线必然差出几像素
        let painted: Vec<(String, egui::Pos2, Color32)> = out
            .shapes
            .iter()
            .filter_map(|item| match &item.shape {
                egui::Shape::Text(text) => Some((
                    text.galley.text().to_string(),
                    text.pos,
                    text.galley
                        .job
                        .sections
                        .first()
                        .map(|section| section.format.color)
                        .unwrap_or(Color32::PLACEHOLDER),
                )),
                _ => None,
            })
            .collect();
        let found = |label: &str| {
            painted
                .iter()
                .find(|(text, _, _)| text == label)
                .unwrap_or_else(|| panic!("没画出「{label}」"))
        };
        // 落笔前 egui 会取整，位置比较一律留 0.6 的容差
        let near = |a: f32, b: f32| (a - b).abs() < 0.6;
        let warn = Color32::from_rgb(240, 190, 70);

        let cards: Vec<egui::Rect> = rects
            .iter()
            .filter(|(rect, radius, ..)| {
                (rect.width() - CARD_SIZE).abs() < 0.6
                    && (rect.height() - CARD_SIZE).abs() < 0.6
                    && *radius == 10
            })
            .map(|(rect, _, _, _)| *rect)
            .collect();
        assert_eq!(cards.len(), 8, "四张卡片各一个填充 + 一个描边");
        let mut squares: Vec<egui::Rect> = cards.clone();
        squares.sort_by_key(|rect| rect.left() as i32);
        squares.dedup();
        assert_eq!(squares.len(), 4, "四张卡片");
        assert!(
            squares.windows(2).all(|pair| near(pair[0].top(), pair[1].top())),
            "卡片要排成同一横行，不能歪到下一行：{squares:?}"
        );
        // 「+」：30 见方、圆角 15 → 画出来就是个圆（填充和圆环在同一个 RectShape 里）
        assert_eq!(
            rects
                .iter()
                .filter(|(rect, radius, ..)| {
                    (rect.width() - 30.0).abs() < 0.6
                        && (rect.height() - 30.0).abs() < 0.6
                        && *radius == 15
                })
                .count(),
            4,
            "四张卡片四个圆的「+」"
        );

        // 第一张卡片：服务器有 5 项待更新、本地有 3 项待提交
        let card = squares[0];
        let pills: Vec<(egui::Rect, Color32)> = rects
            .iter()
            .filter(|(rect, radius, ..)| {
                *radius == 8
                    && (rect.height() - MARK_H).abs() < 1.0
                    && card.contains_rect(*rect)
                    && rect.center().y < card.center().y
            })
            .map(|(rect, _, fill, _)| (*rect, *fill))
            .collect();
        assert_eq!(pills.len(), 2, "卡片右上角应该有两枚标记");
        let (top, bottom) = if pills[0].0.top() <= pills[1].0.top() {
            (pills[0], pills[1])
        } else {
            (pills[1], pills[0])
        };
        assert!(top.0.bottom() <= bottom.0.top(), "两枚标记要一上一下，不能叠");
        assert!(
            near(top.0.right(), bottom.0.right()),
            "两枚标记右边缘要对齐：{} vs {}",
            top.0.right(),
            bottom.0.right()
        );
        assert!(
            near(card.max.x - top.0.right(), 10.0),
            "标记要贴着卡片内边"
        );
        // 两枚都记着待办 → 都该是那个更深的黄
        assert_eq!(
            pills.iter().map(|(_, fill)| *fill).collect::<Vec<_>>(),
            vec![MARK_BUSY, MARK_BUSY],
            "有待办的标记要填深黄"
        );
        // 第二张卡片两个方向都是 0 → 实心绿
        let quiet_pills: Vec<Color32> = rects
            .iter()
            .filter(|(rect, radius, ..)| {
                *radius == 8
                    && (rect.height() - MARK_H).abs() < 1.0
                    && squares[1].contains_rect(*rect)
            })
            .map(|(_, _, fill, _)| *fill)
            .collect();
        assert_eq!(
            quiet_pills,
            vec![MARK_QUIET, MARK_QUIET],
            "没有待办的标记要填绿色"
        );

        let plus = rects
            .iter()
            .find(|(rect, radius, ..)| *radius == 15 && card.contains_rect(*rect))
            .map(|(rect, _, _, _)| *rect)
            .expect("卡片里要有「+」");
        assert!(near(plus.max.x, card.max.x - 10.0), "「+」贴右内边");
        assert!(near(plus.max.y, card.max.y - 10.0), "「+」贴下内边");

        // 名称居中、状态灯在左上、两行版本在左下
        let texts = stage.step(None);
        let name = Stage::rect_of(&texts, "HRP").expect("名称");
        // 名称用的是字形紧贴的可见矩形，左右侧相机天然不完全对称，容差放宽到 2 像素
        let centered = |a: f32, b: f32| (a - b).abs() < 2.0;
        assert!(
            centered(name.center().x, card.center().x),
            "名称要横向摆在卡片正中，实际文字中心 {}，卡片中心 {}",
            name.center().x,
            card.center().x
        );
        assert!(
            centered(name.center().y, card.center().y),
            "名称要纵向摆在卡片正中，实际文字中心 {}，卡片中心 {}",
            name.center().y,
            card.center().y
        );
        // 状态灯：place 进左上角一个 20 见方的格子，排版原点被居中推到格子中间，
        // 所以这里量可见矩形落在左上角那一块里就行
        let lamp = Stage::rect_of(&texts, "●").expect("状态灯");
        assert!(
            lamp.max.x <= card.min.x + 40.0 && lamp.max.y <= card.min.y + 40.0,
            "状态灯要在卡片左上角，实际 {lamp:?}，卡片 {card:?}"
        );
        let local = found("本地 r120").1;
        let remote = found("远端 r128").1;
        assert!(near(local.x, remote.x), "两行版本要左对齐");
        assert!(near(local.x, card.min.x + 10.0), "版本行贴左内边");
        assert!(local.y < remote.y, "远端版本在本地版本下面");
        // 远端有更新 → 用详细模式里「远端 rX（可更新 N 项）」的那个黄色；没更新的卡片保持弱色
        assert_eq!(found("远端 r128").2, warn, "有可更新项时远端行要走警告色");
        assert_eq!(found("本地 r120").2, found("远端 r44").2, "没得更新时两行同为弱色");
        assert_ne!(found("远端 r44").2, warn, "另一张卡片没待更新项，不该跟着变黄");
    }

    /// 计数标记是实心色块，深浅两套主题都得是同一块色，而且数字不能和底色撞在一起
    #[test]
    fn badge_colors_stay_readable_in_both_themes() {
        for theme in [ThemePreference::Dark, ThemePreference::Light] {
            let mut stage = Stage::new(true);
            stage.ctx.set_theme(theme);
            let Stage { ctx, app } = &mut stage;
            let mut out = ctx.run_ui(base_input(), |ui| app.main_page(ui));
            out.textures_delta.clear();
            let chips: Vec<(egui::Rect, Color32)> = out
                .shapes
                .iter()
                .filter_map(|item| match &item.shape {
                    egui::Shape::Rect(rect)
                        if rect.corner_radius.nw == 8
                            && (rect.rect.height() - MARK_H).abs() < 1.0 =>
                    {
                        Some((rect.rect, rect.fill))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(chips.len(), 8, "{theme:?}：四张卡片八枚标记");
            assert!(
                chips.iter().all(|(_, fill)| { *fill == MARK_QUIET || *fill == MARK_BUSY }),
                "{theme:?}：标记底色只该是那两种实心色，实际 {chips:?}"
            );
            // 每张卡片右上角两枚：有以待办的第一张走深黄，两个方向都干净的第二张走绿
            assert_eq!(chips[0].1, MARK_BUSY, "{theme:?}：有待办的标记该填深黄");
            assert_eq!(chips[2].1, MARK_QUIET, "{theme:?}：没待办的标记该填实心绿");
            // 数字压在底色上，两者相同就直接糊了
            let labels: Vec<(egui::Rect, Color32)> = out
                .shapes
                .iter()
                .filter_map(|item| match &item.shape {
                    egui::Shape::Text(shape) => Some((
                        item.shape.visual_bounding_rect(),
                        shape.galley.job.sections.first()?.format.color,
                    )),
                    _ => None,
                })
                .collect();
            for (rect, fill) in &chips {
                let digits: Vec<Color32> = labels
                    .iter()
                    .filter(|(bounds, _)| rect.contains_rect(*bounds))
                    .map(|(_, color)| *color)
                    .collect();
                assert!(!digits.is_empty(), "{theme:?}：标记 {rect:?} 里没有数字");
                assert!(
                    digits.iter().all(|color| color != fill),
                    "{theme:?}：数字颜色和底色相同，读不出来"
                );
            }
        }
    }

    /// 简约卡片跑任务时只多一枚转圈：地方小，再挤一行任务文字就把名称压掉了
    #[test]
    fn busy_card_shows_only_a_spinner() {
        let mut stage = Stage::new(true);
        let quiet = stage.step(None);
        stage.app.pool.spawn(Kind::Refresh, 0, "正在检测 …".to_owned(), |_| {
            Data::Run {
                dir: 0,
                ok: true,
                message: String::new(),
                reload: false,
            }
        });
        let texts = stage.step(None);
        assert!(
            Stage::rect_of(&texts, "正在检测 …").is_none(),
            "忙碌时不该再显示任务文字"
        );
        for label in ["HRP", "本地 r120", "远端 r128", "↓5", "↑3"] {
            assert!(
                Stage::rect_of(&texts, label).is_some(),
                "转圈不该挤掉卡片原有的「{label}」"
            );
        }
        let _ = quiet;
    }

    /// 「+」点开是一级菜单，悬停「更多」接出二级菜单：两级都要真能出来
    #[test]
    fn plus_menu_expands_two_levels() {
        let mut stage = Stage::new(true);
        let texts = stage.step(None);
        let plus = Stage::rect_of(&texts, "+").expect("卡片右下角要有「+」");
        stage.step(Some(plus.center()));
        let texts = stage.step(None);
        for label in ["↓ 更新", "↑ 上传", "历史", "打开目录", "移除"] {
            assert!(Stage::rect_of(&texts, label).is_some(), "「+」菜单里应有「{label}」");
        }
        // 二级菜单靠悬停展开（点「更多」反而会把整层菜单收掉）；
        // 子菜单按钮的文字里带着右箭头，只能按前缀找
        let more = texts
            .iter()
            .find(|(text, _)| text.starts_with("更多"))
            .map(|(_, rect)| *rect)
            .expect("一级菜单里应有「更多」");
        stage.hover(more.center());
        let texts = stage.step(None);
        assert!(
            Stage::rect_of(&texts, "▲ 上移").is_some(),
            "悬停「更多」应展开二级菜单，实际：{:?}",
            texts.iter().map(|(t, _)| t).collect::<Vec<_>>()
        );
    }

    /// 收一帧里所有图元的包围盒，用来比对悬停前后有没有东西变形
    fn painted_bounds(stage: &mut Stage, hover: Option<Pos2>) -> Vec<Rect> {
        let mut input = base_input();
        if let Some(pos) = hover {
            input.events = vec![egui::Event::PointerMoved(pos)];
        }
        let Stage { ctx, app } = &mut *stage;
        let mut out = ctx.run_ui(input, |ui| app.main_page(ui));
        out.textures_delta.clear();
        out.shapes
            .iter()
            .map(|item| item.shape.visual_bounding_rect())
            .collect()
    }

    /// 鼠标移进移出都不许让卡片上的任何东西改尺寸：控件的三态 visuals（`Button` 的
    /// hovered / active 会换一套底色、描边、圆角）最容易干这件事，所以计数标记改成自画。
    #[test]
    fn hovering_changes_no_size() {
        let near = |a: f32, b: f32| (a - b).abs() < 0.6;
        for theme in [ThemePreference::Dark, ThemePreference::Light] {
            let mut stage = Stage::new(true);
            stage.ctx.set_theme(theme);
            let calm = painted_bounds(&mut stage, None);
            let chip = *calm
                .iter()
                .find(|rect| (rect.height() - MARK_H).abs() < 1.0 && rect.width() < 60.0)
                .expect("没画出计数标记");
            let plus = *calm
                .iter()
                .find(|rect| near(rect.width(), 30.0) && near(rect.height(), 30.0))
                .expect("没画出「+」");
            for (name, target) in [
                ("计数标记", chip.center()),
                ("「+」", plus.center()),
                ("卡片空白", chip.center() + egui::vec2(-40.0, 60.0)),
            ] {
                let hot = painted_bounds(&mut stage, Some(target));
                // 只认「左上角还是那个左上角、宽或高却变了」的图元；
                // 悬停新长出来的提示框是另一个位置，不会被算进来
                let jumps: Vec<(Rect, Rect)> = calm
                    .iter()
                    .flat_map(|before| {
                        hot.iter().filter_map(move |after| {
                            let same_place =
                                near(before.left(), after.left()) && near(before.top(), after.top());
                            let resized =
                                !near(before.width(), after.width())
                                    || !near(before.height(), after.height());
                            (same_place && resized).then(|| (*before, *after))
                        })
                    })
                    .collect();
                assert!(jumps.is_empty(), "{theme:?}：鼠标停在{name}上时尺寸变了 {jumps:?}");
            }
        }
    }
}

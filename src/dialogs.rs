//! 各个独立窗口：修改仓库地址、全部上传（含单目录待提交明细）、发现新版本时的升级确认。


use egui::{Align, Color32, Frame, Key, Layout, RichText, ScrollArea, TextEdit, Vec2};

use crate::jobs::Kind;
use crate::{update, APP_VERSION, Level, SvnApp, UploadAll, ink};

impl SvnApp {
    // ------------------------------------------------------------ 「修改仓库地址」窗口

    pub(crate) fn relocate_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.relocate.clone() else {
            return;
        };
        let dir = dialog.dir;
        let label = self.dir_label(dir);
        let current = self
            .dirs
            .get(dir)
            .and_then(|view| view.info.clone())
            .map(|info| info.url)
            .unwrap_or_default();
        let running = self.pool.has(Kind::Relocate, dir);
        // 关闭用标题栏的 ×，工具栏里不再放「关闭」按钮
        let mut open = true;
        egui::Window::new(format!("修改仓库地址 · {label}"))
            .open(&mut open)
            .default_pos(egui::pos2(400.0, 220.0))
            .show(ctx, |ui| {
                ui.label(
                    RichText::new("只改写工作副本记录的仓库地址，不会更新、也不会改动任何本地文件。")
                        .weak()
                        .size(11.5),
                );
                ui.label(RichText::new(format!("当前地址：{current}")).size(12.0).monospace());
                ui.add_space(5.0);
                ui.label(RichText::new("原地址前缀").strong().size(12.5));
                ui.add_sized(
                    Vec2::new(470.0, 22.0),
                    TextEdit::singleline(&mut dialog.from),
                );
                ui.label(RichText::new("新地址前缀").strong().size(12.5));
                let field = ui.add_sized(
                    Vec2::new(470.0, 22.0),
                    TextEdit::singleline(&mut dialog.to).hint_text("例如 https://ypcloud/svn/java"),
                );
                if dialog.focus {
                    field.request_focus();
                    dialog.focus = false;
                }
                let from = dialog.from.trim().trim_end_matches('/');
                let to = dialog.to.trim().trim_end_matches('/');
                let new_url = if from.is_empty() || to.is_empty() || !current.starts_with(from) {
                    String::new()
                } else {
                    current.replacen(from, to, 1)
                };
                ui.add_space(5.0);
                if new_url.is_empty() {
                    let tip = if to.is_empty() {
                        "填好新地址前缀后，这里会显示改写后的地址".to_owned()
                    } else {
                        "原地址前缀必须是当前地址的开头，请核对".to_owned()
                    };
                    ui.label(RichText::new(tip).weak().size(11.5));
                } else {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("改写后：").weak().size(12.0));
                        ui.label(
                            RichText::new(&new_url)
                                .strong()
                                .monospace()
                                .size(12.0)
                                .color(ink(ui, Color32::from_rgb(80, 200, 120))),
                        );
                    });
                }
                if !dialog.error.is_empty() {
                    ui.label(
                        RichText::new(dialog.error.clone())
                            .size(12.0)
                            .color(ink(ui, Color32::from_rgb(240, 100, 100))),
                    );
                }
                ui.horizontal(|ui| {
                    let ready = !running && !new_url.is_empty() && new_url != current;
                    if ui.add_enabled_ui(ready, |ui| ui.button("✓ 执行 relocate")).inner.clicked() {
                        dialog.error.clear();
                        self.spawn_relocate(dir, from.to_owned(), to.to_owned(), new_url.clone());
                    }
                    if running {
                        ui.spinner();
                        ui.label(RichText::new("正在修改 …").weak().size(11.5));
                    } else if !new_url.is_empty() && new_url == current {
                        ui.label(RichText::new("新旧地址一样").weak().size(11.5));
                    }
                });
                if field.has_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    if new_url.is_empty() || new_url == current {
                        dialog.error = "请填写与当前地址不同的新地址前缀".to_owned();
                    } else if !running {
                        dialog.error.clear();
                        self.spawn_relocate(dir, from.to_owned(), to.to_owned(), new_url.clone());
                    }
                }
                ui.label(
                    RichText::new("提示：svn 会连新地址校验仓库标识，地址写错只是执行失败，不会弄坏工作副本。")
                        .weak()
                        .size(11.5),
                );
            });
        if open {
            self.relocate = Some(dialog);
        } else {
            self.relocate = None;
        }
    }

    // ------------------------------------------------------------ 「全部上传」窗口

    /// 所有目录用同一条说明各起一个提交任务。会改动服务器，所以第一下只亮出确认按钮。
    /// 每个目录前有勾选框决定参不参与本次上传；点「待提交 X 项」可弹出明细核对。
    pub(crate) fn upload_all_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.upload_all.clone() else {
            return;
        };
        // 窗口开着时目录列表可能增删：勾选状态跟着对齐，新出现的目录默认勾上
        dialog.checked.resize(self.cfg.dirs.len(), true);
        dialog.checked.truncate(self.cfg.dirs.len());
        if dialog.detail.is_some_and(|index| index >= self.cfg.dirs.len()) {
            dialog.detail = None;
        }
        // 行上显示的改动数只作参考（没检测过的写「未检测」），真正的清单由任务里现读 svn status
        let rows: Vec<(String, Option<usize>, bool)> = (0..self.cfg.dirs.len())
            .map(|index| {
                (
                    self.dir_label(index),
                    self.dirs.get(index).and_then(|view| view.changed),
                    self.pool.has(Kind::Commit, index),
                )
            })
            .collect();
        let total = rows.len();
        let picked = dialog.checked.iter().filter(|checked| **checked).count();
        let busy = rows.iter().any(|(_, _, running)| *running);
        let mut open = true;
        let mut close = false;
        egui::Window::new("全部上传")
            .open(&mut open)
            .default_pos(egui::pos2(430.0, 180.0))
            .show(ctx, |ui| {
                ui.label(
                    RichText::new("风险：这一步会直接改动服务器上的仓库")
                        .strong()
                        .size(13.5)
                        .color(ink(ui, Color32::from_rgb(240, 100, 100))),
                );
                ui.label(
                    RichText::new(
                        "· 提交完别人立刻就能看到，本地撤不回来（要退回只能再提交一次）\n\
                         · 清单 = 各目录全部可提交项：未版本化(?) 提交时自动 svn add、已丢失(!) 自动 svn delete，\n\
                         与修改项一起一次提交，不需要再手动标记删除和添加\n\
                         · 冲突 / 不完整的条目不会被带上；没有可提交改动的目录自动跳过\n\
                         · 勾选参与本次上传的目录（默认全选）；点「待提交 X 项」可先核对明细\n\
                         · 想逐条挑文件、自己核对差异，请用目录行里的「上传」",
                    )
                    .weak()
                    .size(11.5),
                );
                ui.separator();
                ui.label(RichText::new("提交说明（所有目录共用这一条）").strong().size(12.5));
                let field = ui.add_sized(
                    Vec2::new(500.0, 54.0),
                    TextEdit::multiline(&mut dialog.message).hint_text("填写本次提交说明"),
                );
                if dialog.focus {
                    field.request_focus();
                    dialog.focus = false;
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(RichText::new("参与上传的目录").strong().size(12.5));
                    if ui.button("全选").clicked() {
                        dialog.checked.iter_mut().for_each(|checked| *checked = true);
                    }
                    if ui.button("全不选").clicked() {
                        dialog.checked.iter_mut().for_each(|checked| *checked = false);
                    }
                    if picked == 0 {
                        ui.label(RichText::new("一个目录都没勾").weak().size(11.5));
                    }
                });
                ScrollArea::vertical()
                    .id_salt("upload_all_dirs")
                    .max_height(150.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (index, (label, changed, running)) in rows.iter().enumerate() {
                            ui.horizontal(|ui| {
                                let mut checked =
                                    dialog.checked.get(index).copied().unwrap_or(true);
                                if ui
                                    .checkbox(&mut checked, "")
                                    .on_hover_text("勾掉就不参与本次批量上传")
                                    .changed()
                                {
                                    dialog.checked[index] = checked;
                                }
                                ui.label(RichText::new(label).size(12.5).strong());
                                let note = match *changed {
                                    Some(0) => "无改动，自动跳过".to_owned(),
                                    Some(count) => format!("待提交 {count} 项"),
                                    None => "未检测，点击读取".to_owned(),
                                };
                                if matches!(*changed, None | Some(1..)) {
                                    // 点击「待提交 X 项」弹出明细窗口，核对具体哪些文件变了
                                    let hit = ui.add(
                                        egui::Label::new(
                                            RichText::new(note)
                                                .size(11.5)
                                                .underline()
                                                .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                                        )
                                        .sense(egui::Sense::click()),
                                    );
                                    if hit.clicked() {
                                        dialog.detail = Some(index);
                                        // 打开明细时现读一遍 svn status，看到的清单是新鲜的
                                        self.spawn_status(index, false);
                                    }
                                    hit.on_hover_text("点击查看该目录的变动文件明细");
                                } else {
                                    ui.label(
                                        RichText::new(note)
                                            .size(11.5)
                                            .color(ink(ui, Color32::from_gray(130))),
                                    );
                                }
                                if *running {
                                    ui.spinner();
                                    ui.label(RichText::new("该目录正在提交").weak().size(11.5));
                                }
                            });
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    let ready = total > 0 && picked > 0 && !busy;
                    if dialog.confirm {
                        let button = egui::Button::new(
                            RichText::new(format!(
                                "确认上传？{picked} 个目录的改动会立即进服务器"
                            ))
                            .strong()
                            .color(Color32::from_rgb(255, 240, 240)),
                        )
                        .fill(Color32::from_rgb(150, 60, 60));
                        if ui.add_enabled(ready, button).clicked() {
                            let message = dialog.message.clone();
                            for index in 0..total {
                                if dialog.checked.get(index).copied().unwrap_or(false) {
                                    self.spawn_upload_all(index, message.clone());
                                }
                            }
                            self.push(
                                Level::Info,
                                format!(
                                    "全部上传：已给 {picked} 个目录排上提交任务（没有改动的会自动跳过）"
                                ),
                            );
                            close = true;
                        }
                        if ui.button("先不上传").clicked() {
                            dialog.confirm = false;
                        }
                    } else if ui
                        .add_enabled(
                            ready,
                            egui::Button::new(
                                RichText::new(format!(
                                    "全部上传（{}）",
                                    if picked == total {
                                        format!("{total} 个目录")
                                    } else {
                                        format!("勾选 {picked} / {total} 个目录")
                                    }
                                ))
                                .strong(),
                            ),
                        )
                        .clicked()
                    {
                        dialog.confirm = true;
                        self.hint("这一步会改动服务器：核对上面的目录后，再点一次「确认上传」");
                    }
                    if total == 0 {
                        ui.label(RichText::new("目录列表是空的").weak().size(11.5));
                    } else if busy {
                        ui.label(
                            RichText::new("有目录正在提交，等它结束后才能批量上传")
                                .weak()
                                .size(11.5),
                        );
                    } else if picked == 0 {
                        ui.label(RichText::new("先勾选至少一个目录").weak().size(11.5));
                    }
                });
            });
        // 「待提交明细」子弹窗：叠在「全部上传」窗口之上
        if let Some(index) = dialog.detail {
            self.upload_detail_window(ctx, index, &mut dialog);
        }
        if open && !close {
            self.upload_all = Some(dialog);
        } else {
            self.upload_all = None;
        }
    }

    /// 「全部上传」窗口里点「待提交 X 项」弹出的明细窗口：
    /// 列出该目录会进本次提交的全部条目（? / ! 也在内，提交时自动补 add / delete）。
    fn upload_detail_window(&mut self, ctx: &egui::Context, index: usize, dialog: &mut UploadAll) {
        let label = self.dir_label(index);
        let path = self
            .dir_path(index)
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let reading = self.pool.has(Kind::Status, index);
        // 先把行状态克隆下来，闭包里还要借用 self 去发起「重新读取」
        let view = self.dirs.get(index).cloned().unwrap_or_default();
        let mut open = true;
        egui::Window::new(format!("待提交明细 · {label}"))
            .open(&mut open)
            .default_pos(egui::pos2(520.0, 240.0))
            .default_size(Vec2::new(560.0, 430.0))
            .show(ctx, |ui| {
                if !path.is_empty() {
                    ui.label(RichText::new(path.as_str()).size(11.5).weak());
                }
                // 计数口径与提交页一致：? 折进新增、! 折进删除
                let mut added = 0;
                let mut modified = 0;
                let mut deleted = 0;
                for entry in &view.changes {
                    match entry.item {
                        crate::svn::Item::Added | crate::svn::Item::Unversioned => added += 1,
                        crate::svn::Item::Modified | crate::svn::Item::Replaced => modified += 1,
                        crate::svn::Item::Deleted | crate::svn::Item::Missing => deleted += 1,
                        _ => {}
                    }
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("共 {} 项", view.changes.len()))
                        .strong()
                        .size(12.5));
                    ui.label(
                        RichText::new(format!("新增 {added}"))
                            .size(12.0)
                            .color(ink(ui, Color32::from_rgb(60, 190, 110))),
                    );
                    ui.label(
                        RichText::new(format!("修改 {modified}"))
                            .size(12.0)
                            .color(ink(ui, Color32::from_rgb(90, 160, 240))),
                    );
                    ui.label(
                        RichText::new(format!("删除 {deleted}"))
                            .size(12.0)
                            .color(ink(ui, Color32::from_rgb(235, 90, 90))),
                    );
                    if ui.button("重新读取").clicked() {
                        self.spawn_status(index, false);
                    }
                    if reading {
                        ui.spinner();
                        ui.label(RichText::new("正在读取 svn status …").weak().size(11.5));
                    }
                });
                ui.label(
                    RichText::new(
                        "这些就是「全部上传」会提交的条目：新增(?)、已丢失(!) 提交时自动补 svn add / svn delete，无需手动标记",
                    )
                    .weak()
                    .size(11.0),
                );
                ui.separator();
                ScrollArea::vertical()
                    .id_salt(("upload_detail", index))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        if view.changes.is_empty() && !reading {
                            ui.label(RichText::new(if view.changed.is_some() {
                                "该目录没有会进本次上传的改动"
                            } else {
                                "尚未读到该目录的修改清单，点上方「重新读取」"
                            })
                            .weak());
                        }
                        for entry in &view.changes {
                            let rgb = entry.item.color();
                            let color = ink(ui, Color32::from_rgb(rgb.0, rgb.1, rgb.2));
                            // 显示相对目录的路径（能看出子目录里的变动），取不到就退回文件名
                            let shown = entry
                                .path
                                .strip_prefix(&path)
                                .map(|rel| rel.trim_start_matches(['\\', '/']))
                                .filter(|rel| !rel.is_empty())
                                .unwrap_or(&entry.name);
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(entry.item.mark().to_string())
                                        .strong()
                                        .monospace()
                                        .size(12.5)
                                        .color(color),
                                )
                                .on_hover_text(entry.item.text());
                                ui.label(RichText::new(shown).size(12.0).color(color))
                                    .on_hover_text(format!(
                                        "{}\n状态：{}",
                                        entry.path, entry.item.text()
                                    ));
                                if entry.item.needs_add() {
                                    ui.label(
                                        RichText::new("提交时自动 add")
                                            .size(10.5)
                                            .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                                    );
                                } else if entry.item.needs_delete() {
                                    ui.label(
                                        RichText::new("提交时自动 delete")
                                            .size(10.5)
                                            .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                                    );
                                }
                            });
                        }
                    });
            });
        if !open {
            dialog.detail = None;
        }
    }

    /// 「发现新版本」确认对话框：顶部「↑ 新版本」按钮与设置里的「立即更新」
    /// 都打开它，点「开始更新」后直接进入下载，不再绕道设置页。
    pub(crate) fn update_confirm_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_update_confirm {
            return;
        }
        let Some(manifest) = self.update_info.clone() else {
            // 没有可用的更新信息（理论上只有 update_ready 才打开）就顺手关掉
            self.show_update_confirm = false;
            return;
        };
        let downloading = self.pool.has(Kind::DownloadUpdate, usize::MAX);
        let mut open = self.show_update_confirm;
        // 点了「稍后再说」要到窗口收尾时才写回，见下面那句
        let mut later = false;
        egui::Window::new("发现新版本")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(440.0)
            .default_pos(egui::pos2(430.0, 200.0))
            .show(ctx, |ui| {
                let latest = manifest.version.trim();
                // 版本号没变、只是更新源重新发布过构建时不做「旧 → 新」的对比：
                // 同一个号画两遍看着像没更新，这里只把要装上去的版本说清楚
                let replaced = update::same_version(latest, APP_VERSION);
                ui.horizontal(|ui| {
                    ui.add_space(2.0);
                    if !replaced {
                        ui.label(RichText::new(format!("V{APP_VERSION}")).size(17.0).weak());
                        ui.label(RichText::new("→").size(17.0).weak());
                    }
                    // 新旧版本号同字号、同字重：字大一号会把两个号看成两个量级。
                    // 新版号走金色（与日志区 Warning 同一档），在深浅两套主题下都压得住
                    ui.label(
                        RichText::new(format!("V{latest}"))
                            .size(17.0)
                            .strong()
                            .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                    );
                    if replaced {
                        ui.label(RichText::new("内容有更新（版本号没变）").size(11.5).weak());
                    }
                    if !manifest.published_at.trim().is_empty() {
                        ui.label(
                            RichText::new(format!("发布于 {}", manifest.published_at.trim()))
                                .weak()
                                .size(11.5),
                        );
                    }
                });
                // 更新说明放在与明细窗口同风格的灰边卡片里；支持多行（发布端用 \n 或 notes.txt）
                if !manifest.notes.trim().is_empty() {
                    ui.add_space(6.0);
                    let (card_fill, card_stroke) = if ui.visuals().dark_mode {
                        (Color32::from_gray(40), Color32::from_gray(65))
                    } else {
                        (Color32::WHITE, Color32::from_gray(200))
                    };
                    Frame::new()
                        .inner_margin(8.0)
                        .corner_radius(5.0)
                        .fill(card_fill)
                        .stroke(egui::Stroke::new(1.0, card_stroke))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(RichText::new("更新说明").strong().size(12.0));
                            ScrollArea::vertical()
                                .max_height(130.0)
                                .id_salt("update_notes")
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    // egui Label 按原文渲染换行，发布时写的多行说明原样展示
                                    ui.label(RichText::new(manifest.notes.trim()).size(12.0));
                                });
                        });
                }
                ui.add_space(6.0);
                if downloading {
                    // 下载中：状态直接在对话框里，不用翻日志区
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("正在下载新版本并校验…").size(12.5));
                    });
                    ui.label(
                        RichText::new("完成后程序会自动覆盖重启；进度详情见底部输出区。")
                            .weak()
                            .size(11.5),
                    );
                } else if let Some(error) = &self.update_error {
                    ui.label(
                        RichText::new(format!("上次下载失败：{error}"))
                            .size(11.5)
                            .color(ink(ui, Color32::from_rgb(240, 100, 100))),
                    );
                } else {
                    ui.label(
                        RichText::new(
                            "点「开始更新」后下载新版本并自动校验，然后程序自动退出完成覆盖并重新启动。",
                        )
                        .weak()
                        .size(11.5),
                    );
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    // 动作行整组靠右；add 顺序反过来，「开始更新」才仍然排在「稍后再说」左边
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add_enabled(!downloading, egui::Button::new("稍后再说")).clicked() {
                            later = true;
                        }
                        let start = ui.add_enabled(
                            !downloading,
                            egui::Button::new(
                                RichText::new(if downloading { "下载中…" } else { "开始更新" })
                                    .strong(),
                            ),
                        );
                        if start.clicked() {
                            // 保持在对话框里看下载状态，不再一按就关
                            self.update_error = None;
                            self.begin_update();
                        }
                    });
                });
            });
        // 关窗口这件事统一在这里收口：在按钮里改 self.show_update_confirm 会被下面这行
        // 按旧的 open 覆盖回去，窗口关不掉（点「稍后再说」等于没点）
        self.show_update_confirm = open && !later;
    }
}

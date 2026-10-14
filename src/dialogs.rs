//! 各个独立窗口：修改仓库地址、全部上传（含单目录待提交明细）、发现新版本时的升级确认。


use egui::{Align, Color32, Frame, Key, Layout, RichText, ScrollArea, TextEdit, Vec2};

use crate::jobs::Kind;
use crate::repo::fmt_rev;
use crate::ui::WindowCentered;
use crate::{pane_hover, update, APP_VERSION, Level, RepoApp, UploadAll, ink};
use crate::ui::{AlignedRow, field_margin};

/// 清单行：勾选中的行淡染主色、没勾的行悬停给一档灰底，长列表里指到哪行看得清哪行。
/// 行内容照旧由调用方摆；悬停读的是上一帧的命中结果——逐帧跟着指针走，滞后一帧看不出来。
fn checked_row(ui: &mut egui::Ui, id: egui::Id, checked: bool, add_contents: impl FnOnce(&mut egui::Ui)) {
    let hovered = ui
        .ctx()
        .read_response(id)
        .is_some_and(|resp| resp.hovered());
    let fill = if checked {
        ui.visuals().selection.bg_fill
    } else if hovered {
        pane_hover(ui)
    } else {
        Color32::TRANSPARENT
    };
    let row = Frame::new()
        .inner_margin(egui::Margin::symmetric(6, 2))
        .corner_radius(5.0)
        .fill(fill)
        .show(ui, |ui| {
            ui.horizontal(add_contents);
        });
    ui.interact(row.response.rect, id, egui::Sense::hover());
}

/// 地址前缀改写：把 `current` 开头的 `from` 换成 `to`。两边都先掐掉尾部斜杠；
/// from 是 current 的开头且 to 非空才改写，否则给 None（没有可显示的新地址）。
/// 单目录与批量两个改地址窗口共用这一份口径
pub(crate) fn rewrite_url(current: &str, from: &str, to: &str) -> Option<String> {
    let from = from.trim().trim_end_matches('/');
    let to = to.trim().trim_end_matches('/');
    if from.is_empty() || to.is_empty() || !current.starts_with(from) {
        return None;
    }
    Some(current.replacen(from, to, 1))
}

impl RepoApp {
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
            .centered(ctx, "relocate")
            .show(ctx, |ui| {
                ui.label(
                    RichText::new("只改写工作副本记录的仓库地址，不会更新、也不会改动任何本地文件。")
                        .weak()
                        .size(11.5),
                );
                ui.label(RichText::new(format!("当前地址：{current}")).size(12.0).monospace());
                ui.add_space(5.0);
                ui.label(RichText::new("原地址前缀").strong().size(12.5));
                ui.add(
                    TextEdit::singleline(&mut dialog.from)
                        .margin(field_margin(ui))
                        .desired_width(470.0),
                );
                ui.label(RichText::new("新地址前缀").strong().size(12.5));
                let field = ui.add(
                    TextEdit::singleline(&mut dialog.to)
                        .hint_text("例如 https://ypcloud/svn/java")
                        .margin(field_margin(ui))
                        .desired_width(470.0),
                );
                if dialog.focus {
                    field.request_focus();
                    dialog.focus = false;
                }
                let from = dialog.from.trim().trim_end_matches('/');
                let to = dialog.to.trim().trim_end_matches('/');
                let new_url = rewrite_url(&current, &dialog.from, &dialog.to).unwrap_or_default();
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
                ui.aligned_row(|ui| {
                    let ready = !running && !new_url.is_empty() && new_url != current;
                    if ui.add_enabled_ui(ready, |ui| ui.button("✓ 执行 relocate")).inner.clicked() {
                        dialog.error.clear();
                        self.spawn_relocate(dir, from.to_owned(), to.to_owned(), new_url.clone());
                    }
                    if running {
                        ui.spinner();
                        ui.label(RichText::new("正在修改 …").weak().size(11.5));
                    } else if !new_url.is_empty() && new_url == current {
                        ui.label(RichText::new("新地址与原地址相同").weak().size(11.5));
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
                    RichText::new("提示：svn 将连接新地址校验仓库标识；地址有误仅会导致执行失败，不会损坏工作副本。")
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

    // ------------------------------------------------------------ 「批量修改仓库」窗口

    /// 服务器搬家时一次给多个目录改地址：共用一对地址前缀，每个目录各起一个
    /// relocate 任务（同目录的并发约束由 spawn_relocate 把关，有任务在跑的跳过并计数）。
    /// 会改写工作副本记录，所以第一下只把按钮换成确认
    pub(crate) fn batch_relocate_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.batch_relocate.clone() else {
            return;
        };
        // 窗口开着时目录列表可能增删：勾选状态跟着对齐，新出现的目录默认不勾
        dialog.checked.resize(self.cfg.dirs.len(), false);
        dialog.checked.truncate(self.cfg.dirs.len());
        // from 变了就按新前缀重算一遍默认勾选：地址能对上的才勾
        let from_now = dialog.from.trim().trim_end_matches('/').to_owned();
        if from_now != dialog.last_from {
            for (index, checked) in dialog.checked.iter_mut().enumerate() {
                let url = self
                    .dirs
                    .get(index)
                    .and_then(|view| view.info.as_ref())
                    .map(|info| info.url.clone())
                    .unwrap_or_default();
                *checked = !from_now.is_empty() && url.starts_with(&from_now);
            }
            dialog.last_from = from_now;
        }
        // 每行一份快照：目录名、当前地址、改写结果（None = 改不动），Git 目录不参与
        let rows: Vec<(String, String, Option<String>, bool)> = (0..self.cfg.dirs.len())
            .map(|index| {
                let url = self
                    .dirs
                    .get(index)
                    .and_then(|view| view.info.clone())
                    .map(|info| info.url)
                    .unwrap_or_default();
                let git = self.dir_kind(index) == crate::repo::RepoKind::Git;
                let new_url = rewrite_url(&url, &dialog.from, &dialog.to);
                (
                    self.dir_label(index),
                    url,
                    new_url,
                    git,
                )
            })
            .collect();
        let picked = rows
            .iter()
            .zip(dialog.checked.iter())
            .filter(|((_, _, new_url, git), checked)| !*git && new_url.is_some() && **checked)
            .count();
        let mut open = true;
        let mut close = false;
        egui::Window::new("批量修改仓库")
            .open(&mut open)
            .default_width(760.0)
            .centered(ctx, "batch_relocate")
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "给勾选的目录逐个执行「修改仓库地址」：只改写工作副本记录的仓库地址，不会更新、也不会改动任何本地文件。",
                    )
                    .weak()
                    .size(11.5),
                );
                ui.add_space(5.0);
                ui.label(RichText::new("原地址前缀").strong().size(12.5));
                let from_field = ui.add(
                    TextEdit::singleline(&mut dialog.from)
                        .hint_text("要替换掉的地址开头，例如 http://192.168.1.251:8080/svn")
                        .margin(field_margin(ui))
                        .desired_width(560.0),
                );
                if dialog.focus {
                    from_field.request_focus();
                    dialog.focus = false;
                }
                ui.label(RichText::new("新地址前缀").strong().size(12.5));
                ui.add(
                    TextEdit::singleline(&mut dialog.to)
                        .hint_text("新前缀，例如 https://svn.example.com/repo")
                        .margin(field_margin(ui))
                        .desired_width(560.0),
                );
                ui.add_space(5.0);
                ui.aligned_row(|ui| {
                    ui.label(RichText::new("参与改地址的目录").strong().size(12.5));
                    if ui.button("全选").clicked() {
                        for (index, checked) in dialog.checked.iter_mut().enumerate() {
                            *checked = rows
                                .get(index)
                                .is_some_and(|(_, _, new_url, git)| new_url.is_some() && !*git);
                        }
                    }
                    if ui.button("全不选").clicked() {
                        dialog.checked.iter_mut().for_each(|checked| *checked = false);
                    }
                    if picked == 0 {
                        ui.label(RichText::new("没有可改写的目录").weak().size(11.5));
                    }
                });
                ScrollArea::vertical()
                    .id_salt("batch_relocate_dirs")
                    .max_height(220.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (index, (label, url, new_url, git)) in rows.iter().enumerate() {
                            let picked_row = dialog.checked.get(index).copied().unwrap_or(false);
                            checked_row(
                                ui,
                                egui::Id::new(("relocate_row", index)),
                                picked_row,
                                |ui| {
                                    let can = !git && new_url.is_some();
                                    let mut checked =
                                        dialog.checked.get(index).copied().unwrap_or(false);
                                    if ui
                                        .add_enabled(can, egui::Checkbox::new(&mut checked, ""))
                                        .on_hover_text("勾选参与本次批量修改仓库")
                                        .changed()
                                    {
                                        dialog.checked[index] = checked;
                                    }
                                    ui.label(RichText::new(label).size(12.5).strong());
                                    match (*git, new_url) {
                                        (true, _) => {
                                            ui.label(
                                                RichText::new("Git 目录不支持")
                                                    .size(11.5)
                                                    .color(ink(ui, Color32::from_gray(130))),
                                            );
                                        }
                                        (false, Some(new)) => {
                                            ui.label(
                                                RichText::new(url).size(11.5).monospace().weak(),
                                            );
                                            ui.label(RichText::new("→").size(11.5).weak());
                                            ui.label(
                                                RichText::new(new)
                                                    .size(11.5)
                                                    .monospace()
                                                    .color(ink(ui, Color32::from_rgb(
                                                        80, 200, 120,
                                                    ))),
                                            );
                                        }
                                        (false, None) if url.is_empty() => {
                                            ui.label(
                                                RichText::new("未检测，没有地址")
                                                    .size(11.5)
                                                    .color(ink(ui, Color32::from_gray(130))),
                                            );
                                        }
                                        (false, None) => {
                                            ui.label(
                                                RichText::new(url).size(11.5).monospace().weak(),
                                            );
                                            ui.label(
                                                RichText::new("前缀不匹配")
                                                    .size(11.5)
                                                    .color(ink(ui, Color32::from_rgb(
                                                        240, 100, 100,
                                                    ))),
                                            );
                                        }
                                    }
                                    if self.pool.is_busy(index) {
                                        ui.spinner();
                                        ui.label(
                                            RichText::new("有任务在跑，本次跳过")
                                                .weak()
                                                .size(11.5),
                                        );
                                    }
                                },
                            );
                        }
                    });
                ui.separator();
                ui.aligned_row(|ui| {
                    let ready = picked > 0;
                    if dialog.confirm {
                        let button = egui::Button::new(
                            RichText::new(format!("确认改地址？{picked} 个目录"))
                                .strong()
                                .color(crate::DANGER_TEXT),
                        )
                        .fill(crate::DANGER_FILL);
                        if ui.add_enabled(ready, button).clicked() {
                            let from = dialog.from.trim().trim_end_matches('/').to_owned();
                            let to = dialog.to.trim().trim_end_matches('/').to_owned();
                            let mut done = 0;
                            let mut skipped = 0;
                            for (index, (_, _, new_url, git)) in rows.iter().enumerate() {
                                let Some(new_url) = new_url else {
                                    continue;
                                };
                                if *git || !dialog.checked.get(index).copied().unwrap_or(false) {
                                    continue;
                                }
                                if self.pool.is_busy(index) {
                                    skipped += 1;
                                    continue;
                                }
                                self.spawn_relocate(index, from.clone(), to.clone(), new_url.clone());
                                done += 1;
                            }
                            let mut text = format!("批量修改仓库：已为 {done} 个目录发起改地址");
                            if skipped > 0 {
                                text.push_str(&format!("，{skipped} 个因有任务在跑被跳过"));
                            }
                            self.push(Level::Info, text);
                            close = true;
                        }
                        if ui.button("先不改").clicked() {
                            dialog.confirm = false;
                        }
                    } else if ui
                        .add_enabled(
                            ready,
                            egui::Button::new(RichText::new(format!("改地址（{picked} 个目录）")).strong()),
                        )
                        .clicked()
                    {
                        dialog.confirm = true;
                        self.hint("每个目录将单独执行一次改地址：核对上方预览后，再次点击「确认改地址」");
                    }
                    if picked == 0 && !rows.is_empty() {
                        ui.label(
                            RichText::new("填写一对前缀，勾选地址以「原地址前缀」开头的目录")
                                .weak()
                                .size(11.5),
                        );
                    }
                });
            });
        if open && !close {
            self.batch_relocate = Some(dialog);
        } else {
            self.batch_relocate = None;
        }
    }

    // ------------------------------------------------------------ 「批量分组 / 标记」窗口

    /// 给勾选的目录统一写分组：空串即清除。只动本地配置，persist 一次，返回真正改动的个数
    pub(crate) fn apply_batch_group(&mut self, checked: &[bool], group: &str) -> usize {
        let group = group.trim();
        let mut done = 0;
        for (index, dir) in self.cfg.dirs.iter_mut().enumerate() {
            if !checked.get(index).copied().unwrap_or(false) || dir.group == group {
                continue;
            }
            dir.group = group.to_owned();
            done += 1;
        }
        if done > 0 {
            self.persist();
        }
        done
    }

    /// 给勾选的目录统一写标记：tag 非空才存颜色（与单目录编辑器同一口径），tag 为空连颜色一起清。
    /// 只动本地配置，persist 一次，返回真正改动的个数
    pub(crate) fn apply_batch_tag(&mut self, checked: &[bool], tag: &str, hex: &str) -> usize {
        let tag = tag.trim();
        let hex = crate::home::tag_color_hex(crate::home::tag_color_rgb(hex));
        let mut done = 0;
        for (index, dir) in self.cfg.dirs.iter_mut().enumerate() {
            if !checked.get(index).copied().unwrap_or(false) {
                continue;
            }
            if tag.is_empty() {
                if dir.tag.is_empty() && dir.tag_color.is_empty() {
                    continue;
                }
                dir.tag.clear();
                dir.tag_color.clear();
            } else {
                if dir.tag == tag && dir.tag_color == hex {
                    continue;
                }
                dir.tag = tag.to_owned();
                dir.tag_color = hex.clone();
            }
            done += 1;
        }
        if done > 0 {
            self.persist();
        }
        done
    }

    /// 批量分组 / 标记：勾选若干目录，统一设置（或清除）分组、标记。改的是本地配置、
    /// 随时能逐个改回来，所以不做两步确认；分组与标记两个按钮互不牵连，各写各的字段。
    pub(crate) fn batch_label_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.batch_label.clone() else {
            return;
        };
        // 窗口开着时目录列表可能增删：勾选状态跟着对齐，新出现的目录默认不勾
        dialog.checked.resize(self.cfg.dirs.len(), false);
        dialog.checked.truncate(self.cfg.dirs.len());
        // 每行一份快照：目录名、当前分组、当前标记——勾了才知道会覆盖谁
        let rows: Vec<(String, String, String)> = (0..self.cfg.dirs.len())
            .map(|index| {
                let dir = &self.cfg.dirs[index];
                (self.dir_label(index), dir.group.clone(), dir.tag.clone())
            })
            .collect();
        let picked = dialog.checked.iter().filter(|checked| **checked).count();
        let mut open = true;
        let mut close = false;
        egui::Window::new("批量分组 / 标记")
            .open(&mut open)
            .default_width(560.0)
            .centered(ctx, "batch_tag")
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "给勾选的目录统一设置分组、标记：只改本地配置，随时能逐个改回来。两个按钮互不影响，各写各的字段。",
                    )
                    .weak()
                    .size(11.5),
                );
                ui.add_space(5.0);
                ui.label(RichText::new("分组").strong().size(12.5));
                let group_field = ui.add(
                    TextEdit::singleline(&mut dialog.group)
                        .hint_text("留空 = 清除所选目录的分组")
                        .margin(field_margin(ui))
                        .desired_width(520.0),
                );
                if dialog.focus {
                    group_field.request_focus();
                    dialog.focus = false;
                }
                ui.add_space(3.0);
                ui.label(RichText::new("标记").strong().size(12.5));
                ui.aligned_row(|ui| {
                    ui.add(
                        TextEdit::singleline(&mut dialog.tag)
                            .hint_text("留空 = 清除所选目录的标记")
                            .margin(field_margin(ui))
                            .desired_width(490.0),
                    );
                    let mut rgb = crate::home::tag_color_rgb(&dialog.tag_hex);
                    if crate::ui::color_swatch(ui, &mut rgb)
                        .on_hover_text("标记颜色")
                        .changed()
                    {
                        dialog.tag_hex = crate::home::tag_color_hex(rgb);
                    }
                });
                ui.add_space(5.0);
                ui.aligned_row(|ui| {
                    ui.label(RichText::new("要修改的目录").strong().size(12.5));
                    if ui.button("全选").clicked() {
                        dialog.checked.iter_mut().for_each(|checked| *checked = true);
                    }
                    if ui.button("全不选").clicked() {
                        dialog.checked.iter_mut().for_each(|checked| *checked = false);
                    }
                    if picked == 0 {
                        ui.label(RichText::new("请先勾选要修改的目录").weak().size(11.5));
                    }
                });
                ScrollArea::vertical()
                    .id_salt("batch_label_dirs")
                    .max_height(220.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (index, (label, group, tag)) in rows.iter().enumerate() {
                            let picked_row = dialog.checked.get(index).copied().unwrap_or(false);
                            checked_row(
                                ui,
                                egui::Id::new(("label_row", index)),
                                picked_row,
                                |ui| {
                                    let mut checked =
                                        dialog.checked.get(index).copied().unwrap_or(false);
                                    if ui
                                        .checkbox(&mut checked, "")
                                        .on_hover_text("勾选参与本次批量修改")
                                        .changed()
                                    {
                                        dialog.checked[index] = checked;
                                    }
                                    ui.label(RichText::new(label).size(12.5).strong());
                                    if !group.trim().is_empty() {
                                        ui.label(
                                            RichText::new(format!("分组：{group}"))
                                                .size(11.5)
                                                .weak(),
                                        );
                                    }
                                    if !tag.is_empty() {
                                        ui.label(
                                            RichText::new(format!("标记：{tag}")).size(11.5).weak(),
                                        );
                                    }
                                    if group.trim().is_empty() && tag.is_empty() {
                                        ui.label(
                                            RichText::new("未分组、无标记")
                                                .size(11.5)
                                                .color(ink(ui, Color32::from_gray(130))),
                                        );
                                    }
                                },
                            );
                        }
                    });
                ui.separator();
                ui.aligned_row(|ui| {
                    let ready = picked > 0;
                    if ui
                        .add_enabled(
                            ready,
                            egui::Button::new(RichText::new(format!("设置分组（{picked}）")).strong()),
                        )
                        .clicked()
                    {
                        let n = self.apply_batch_group(&dialog.checked, &dialog.group);
                        self.push(Level::Info, format!("批量分组：已修改 {n} 个目录的分组"));
                        close = true;
                    }
                    if ui
                        .add_enabled(
                            ready,
                            egui::Button::new(RichText::new(format!("设置标记（{picked}）")).strong()),
                        )
                        .clicked()
                    {
                        let n = self.apply_batch_tag(&dialog.checked, &dialog.tag, &dialog.tag_hex);
                        self.push(Level::Info, format!("批量标记：已修改 {n} 个目录的标记"));
                        close = true;
                    }
                });
            });
        if open && !close {
            self.batch_label = Some(dialog);
        } else {
            self.batch_label = None;
        }
    }

    // ------------------------------------------------------------ 「回退到某个版本」窗口

    /// 回退确认窗口：这一步会改本地工作副本里的文件，所以第一下只把按钮换成「确认回退」，
    /// 再点一次才真的执行。窗口里不写操作手册，只问一句从哪版回退到哪版、目标是哪个路径。
    pub(crate) fn rollback_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.rollback.clone() else {
            return;
        };
        let dir = dialog.dir;
        let kind = self.dir_kind(dir);
        let revision = dialog.revision.clone();
        let name = dialog.name.clone();
        let target_path = dialog.target.clone();
        let is_dir = dialog.is_dir;
        let shown = target_path.display().to_string();
        let running = self.pool.has(Kind::Rollback, dir);
        let label = self.dir_label(dir);
        let wc_rev = self
            .dirs
            .get(dir)
            .and_then(|view| view.info.clone())
            .map(|info| info.revision)
            .unwrap_or_default();
        // 回退是在现有本地改动之上做合并，先报个已有的待提交数，别让人以为回退会把它们冲掉
        let pending = self.dirs.get(dir).and_then(|view| view.changed).unwrap_or(0);
        let scope = if is_dir { "整个目录" } else { "这个文件" };
        let mut open = true;
        egui::Window::new(format!("回退到 {} · {label}", fmt_rev(kind, &revision)))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .centered(ctx, "rollback")
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(format!(
                        "确定要把{scope}从 {} 回退到 {}？",
                        fmt_rev(kind, &wc_rev),
                        fmt_rev(kind, &revision)
                    ))
                        .strong()
                        .size(13.0),
                );
                ui.label(RichText::new(shown.as_str()).size(12.0).monospace());
                if pending > 0 {
                    ui.label(
                        RichText::new(format!("该目录已有 {pending} 项待提交改动，回退会和它们合并"))
                            .size(11.5)
                            .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                    );
                }
                if !dialog.error.is_empty() {
                    ui.label(
                        RichText::new(dialog.error.clone())
                            .size(12.0)
                            .color(ink(ui, Color32::from_rgb(240, 100, 100))),
                    );
                }
                ui.add_space(4.0);
                ui.aligned_row(|ui| {
                    if dialog.confirm {
                        let button = egui::Button::new(
                            RichText::new(format!("确认回退到 {}", fmt_rev(kind, &revision)))
                                .strong()
                                .color(crate::DANGER_TEXT),
                        )
                        .fill(crate::DANGER_FILL);
                        if ui.add_enabled(!running, button).clicked() {
                            self.spawn_rollback(
                                dir,
                                revision.clone(),
                                target_path.clone(),
                                name.clone(),
                                is_dir,
                            );
                        }
                        if ui.button("先不回退").clicked() {
                            dialog.confirm = false;
                        }
                    } else if ui
                        .add_enabled(
                            !running,
                            egui::Button::new(
                                RichText::new(format!("回退到 {}", fmt_rev(kind, &revision)))
                                    .strong()
                                    .color(crate::DANGER_TEXT),
                            )
                            .fill(crate::DANGER_FILL),
                        )
                        .clicked()
                    {
                        dialog.confirm = true;
                        self.hint(format!(
                            "回退将改动{scope}的本地文件：核对上方路径后，再次点击「确认回退」"
                        ));
                    }
                    if running {
                        ui.spinner();
                        ui.label(RichText::new("正在回退 …").weak().size(11.5));
                    }
                });
            });
        if open {
            self.rollback = Some(dialog);
        } else {
            self.rollback = None;
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
        // 每个目录「还没更新到最新」的提醒：行上标一下，确认上传时再写进输出记录
        let outdated: Vec<Option<String>> =
            (0..total).map(|index| self.not_up_to_date_hint(index)).collect();
        let picked = dialog.checked.iter().filter(|checked| **checked).count();
        // 参与上传、又没更新到最新的目录：确认那一步要点名，别让人蒙着按下去
        let outdated_checked: Vec<usize> = (0..total)
            .filter(|index| dialog.checked.get(*index).copied().unwrap_or(false))
            .filter(|index| outdated.get(*index).and_then(|item| item.as_ref()).is_some())
            .collect();
        let busy = rows.iter().any(|(_, _, running)| *running);
        let mut open = true;
        let mut close = false;
        egui::Window::new("全部上传")
            .open(&mut open)
            .centered(ctx, "upload_all")
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
                ui.aligned_row(|ui| {
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
                            ui.aligned_row(|ui| {
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
                                if outdated.get(index).and_then(|item| item.as_ref()).is_some() {
                                    ui.label(
                                        RichText::new("未更新")
                                            .size(11.0)
                                            .strong()
                                            .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                                    )
                                    .on_hover_text(
                                        "该目录还没更新到最新版本：服务器上还有没拉到本地的新改动，\
                                         这次上传是在旧基线上做的；可以勾掉它先回主页点「更新」",
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
                    if !outdated_checked.is_empty() {
                        // 拦截的第二下：点名哪几个目录没更新到最新，让人知道自己是拿旧基线在上传
                        let names = outdated_checked
                            .iter()
                            .map(|index| rows[*index].0.clone())
                            .collect::<Vec<_>>()
                            .join("、");
                        ui.label(
                            RichText::new(format!("未更新到最新：{names}"))
                                .strong()
                                .size(12.0)
                                .color(ink(ui, Color32::from_rgb(240, 100, 100))),
                        )
                        .on_hover_text(
                            "这些目录在服务器上存在尚未同步到本地的新改动，本次上传将基于旧基线执行；\
                             若他人也修改了同一处，将产生冲突。如需避免，请取消后先更新这些目录。",
                        );
                    }
                });
                ui.aligned_row(|ui| {
                    let ready = total > 0 && picked > 0 && !busy;
                    if dialog.confirm {
                        let caption = if outdated_checked.is_empty() {
                            format!("确认上传？{picked} 个目录的改动会立即进服务器")
                        } else {
                            format!(
                                "确认上传？{picked} 个目录（{} 个未更新到最新）",
                                outdated_checked.len()
                            )
                        };
                        let button = egui::Button::new(
                            RichText::new(caption)
                                .strong()
                                .color(crate::DANGER_TEXT),
                        )
                        .fill(crate::DANGER_FILL);
                        if ui.add_enabled(ready, button).clicked() {
                            let message = dialog.message.clone();
                            for index in 0..total {
                                if dialog.checked.get(index).copied().unwrap_or(false) {
                                    // 没更新到最新也照样上传，但先往输出记录写一条：
                                    // 窗口一关提示就没了，这条留在记录里能对上往后查
                                    if let Some(warning) =
                                        outdated.get(index).and_then(|item| item.clone())
                                    {
                                        let label = self.dir_label(index);
                                        self.push(
                                            Level::Warning,
                                            format!("{label}：{warning}，建议先「更新」再上传"),
                                        );
                                    }
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
                        self.hint("此操作将改动服务器：核对上方目录后，再次点击「确认上传」");
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
            .centered(ctx, "pending_detail")
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
                ui.aligned_row(|ui| {
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
            .centered(ctx, "update_available")
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
                    if manifest.prerelease {
                        ui.label(RichText::new("测试版").size(11.5).weak());
                    }
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
                    ui.aligned_row(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("正在下载新版本并校验…").size(12.5));
                    });
                    ui.label(
                        RichText::new("完成后程序会自动换上并重启；进度详情见底部输出区。")
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
                            "点「开始更新」后下载新版本并自动校验，然后程序自动退出、把新版本换上并重新启动。",
                        )
                        .weak()
                        .size(11.5),
                    );
                }
                ui.add_space(8.0);
                ui.aligned_row(|ui| {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 正常改写：只有以 from 开头的地址才换前缀，剩下的尾巴原样保留
    #[test]
    fn rewrite_url_swaps_only_the_prefix() {
        assert_eq!(
            rewrite_url(
                "http://192.168.1.251:8080/svn/hrp",
                "http://192.168.1.251:8080/svn",
                "https://svn.example.com/repo",
            )
            .as_deref(),
            Some("https://svn.example.com/repo/hrp")
        );
    }

    /// 两头多打的斜杠不该算进前缀
    #[test]
    fn rewrite_url_trims_trailing_slashes() {
        assert_eq!(
            rewrite_url("http://old/svn/hrp", "http://old/svn/", "https://new/repo/").as_deref(),
            Some("https://new/repo/hrp")
        );
    }

    /// 改不动的三种情况：前缀不是开头、新前缀为空、旧前缀为空；
    /// 中段出现同样不算前缀
    #[test]
    fn rewrite_url_rejects_bad_input() {
        assert_eq!(
            rewrite_url("http://old/hrp", "http://other", "https://new"),
            None,
            "前缀不是当前地址的开头"
        );
        assert_eq!(rewrite_url("http://old/hrp", "http://old", ""), None, "新前缀为空");
        assert_eq!(rewrite_url("http://old/hrp", "", "https://new"), None, "旧前缀为空");
        assert_eq!(rewrite_url("http://a/b", "b", "c"), None, "中段出现不算前缀");
    }

    /// 批量分组只动勾选的目录：前后空白掐掉，空串就是清除
    #[test]
    fn apply_batch_group_writes_only_checked_dirs() {
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_batch_group_test"),
        );
        let mut app = crate::testbed::stub_app(false);
        let n = app.apply_batch_group(&[true, false, true, false], "  生产  ");
        assert_eq!(n, 2, "勾了两个该改两个");
        assert_eq!(app.cfg.dirs[0].group, "生产");
        assert_eq!(app.cfg.dirs[1].group, "", "没勾的不动");
        assert_eq!(app.cfg.dirs[2].group, "生产");

        let n = app.apply_batch_group(&[true, false, false, false], "");
        assert_eq!(n, 1, "本来就没分组的目录不算改动");
        assert_eq!(app.cfg.dirs[0].group, "", "空串即清除");
    }

    /// 批量标记：tag 非空连颜色一起写，tag 为空连颜色一起清；没勾的不受牵连
    #[test]
    fn apply_batch_tag_sets_and_clears_with_color() {
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_batch_tag_test"),
        );
        let mut app = crate::testbed::stub_app(false);
        let n = app.apply_batch_tag(&[true, true, false, false], " 冻结 ", "#FF8040");
        assert_eq!(n, 2);
        assert_eq!(app.cfg.dirs[0].tag, "冻结");
        assert_eq!(app.cfg.dirs[0].tag_color, "#FF8040");
        assert_eq!(app.cfg.dirs[1].tag_color, "#FF8040");

        let n = app.apply_batch_tag(&[true, false, false, false], "  ", "");
        assert_eq!(n, 1);
        assert_eq!(app.cfg.dirs[0].tag, "", "tag 为空连颜色一起清");
        assert_eq!(app.cfg.dirs[0].tag_color, "");
        assert_eq!(app.cfg.dirs[1].tag, "冻结", "没勾的不受牵连");
        assert_eq!(app.cfg.dirs[1].tag_color, "#FF8040");
    }
}

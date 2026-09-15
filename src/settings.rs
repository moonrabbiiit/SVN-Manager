//! 设置窗口：svn.exe / Beyond Compare / 主题与自动刷新 / 提交记录 / 版本更新 / AI 日志。

use std::path::PathBuf;

use egui::{RichText, ScrollArea, TextEdit, Vec2};

use crate::{autostart, bcompare, config};
use crate::jobs::Kind;
use crate::update;
use crate::{APP_VERSION, Level, SvnApp};

impl SvnApp {
    // ------------------------------------------------------------ 设置窗口

    pub(crate) fn settings(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut open = self.show_settings;
        let candidates = self.candidates.clone();
        let version = self.version.clone().unwrap_or_else(|| "不可用".into());
        egui::Window::new("设置")
            .open(&mut open)
            .default_pos(egui::pos2(240.0, 120.0))
            .default_size(Vec2::new(660.0, 430.0))
            .show(ctx, |ui| {
                ScrollArea::vertical().id_salt("settings").show(ui, |ui| {
                    ui.label(RichText::new("svn.exe 路径").strong());
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            Vec2::new((ui.available_width() - 150.0).max(180.0), 22.0),
                            TextEdit::singleline(&mut self.cfg.svn_exe).hint_text("留空则自动寻找"),
                        );
                        if ui.button("浏览…").clicked() {
                            if let Some(file) = rfd::FileDialog::new()
                                .set_title("选择 svn.exe")
                                .add_filter("可执行文件", &["exe"])
                                .pick_file()
                            {
                                let text = file.to_string_lossy().into_owned();
                                self.apply_svn_exe(&text);
                                self.spawn_all_refresh();
                            }
                        }
                    });
                    ui.label(RichText::new("如果显示“无法添加工作副本”，请重新安装svn，并且勾选“command line client tools”").weak());
                    ui.horizontal(|ui| {
                        if ui.button("应用此路径").clicked() {
                            let exe = self.cfg.svn_exe.clone();
                            self.apply_svn_exe(&exe);
                            self.hint(match self.version.clone() {
                                Some(v) => format!("svn.exe 可用，版本 {v}"),
                                None => "该路径无法执行，请确认是 svn.exe 命令行客户端".to_owned(),
                            });
                            self.spawn_all_refresh();
                        }
                        if ui.button("自动寻找").clicked() {
                            self.spawn_detect();
                        }
                        if ui.button("前往下载svn").clicked() {
                            ctx.open_url(egui::OpenUrl::same_tab(
                                "https://sourceforge.net/projects/tortoisesvn/",
                            ));
                            self.hint("已在默认浏览器打开 TortoiseSVN 下载页");
                        }
                        ui.label(RichText::new(format!("当前SVN版本：{version}")).weak());
                    });
                    if !candidates.is_empty() {
                        ui.collapsing(format!("检测到的候选（{} 个）", candidates.len()), |ui| {
                            for item in candidates {
                                let mark = if item == self.cfg.svn_exe { "✓" } else { " " };
                                let text = format!("{mark}  {item}");
                                if ui.add(egui::Label::new(RichText::new(text).size(12.0).monospace())).clicked() {
                                    self.apply_svn_exe(&item);
                                    self.spawn_all_refresh();
                                }
                            }
                        });
                    }
                    ui.separator();
                    ui.collapsing("Beyond Compare（外部对比工具）", |ui| {
                        ui.horizontal(|ui| {
                            ui.add_sized(
                                Vec2::new((ui.available_width() - 150.0).max(180.0), 22.0),
                                TextEdit::singleline(&mut self.cfg.bc_exe).hint_text("留空则自动寻找"),
                            );
                            if ui.button("浏览…").clicked() {
                                if let Some(file) = rfd::FileDialog::new()
                                    .set_title("选择 BCompare.exe")
                                    .add_filter("可执行文件", &["exe"])
                                    .pick_file()
                                {
                                    self.bc = file.clone();
                                    self.cfg.bc_exe = file.to_string_lossy().into_owned();
                                    self.persist();
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            if ui.button("应用此路径").clicked() {
                                self.bc = PathBuf::from(self.cfg.bc_exe.trim_matches('"'));
                                self.persist();
                            }
                            if ui.button("自动寻找").clicked() {
                                match bcompare::detect() {
                                    Some(found) => {
                                        self.bc = found.clone();
                                        self.cfg.bc_exe = found.to_string_lossy().into_owned();
                                        self.hint(format!("已找到 {}", found.display()));
                                    }
                                    None => self.hint("未找到 BCompare.exe，请手工填写路径"),
                                }
                                self.persist();
                            }
                            let note = if self.bc.is_file() {
                                self.bc.display().to_string()
                            } else {
                                "未找到".to_owned()
                            };
                            ui.label(RichText::new(format!("当前：{note}")).weak());
                        });
                        // 调用对比时的服务器端所在侧：文件对比的 BASE 版本、
                        // 文件夹对比里所选的工作副本目录都算服务器端
                        ui.horizontal(|ui| {
                            ui.label("调用对比时，服务器端放在");
                            for (value, text) in [("left", "左侧"), ("right", "右侧")] {
                                if ui
                                    .selectable_label(self.cfg.bc_server_side == value, text)
                                    .on_hover_text(
                                        "服务器端指：文件对比时的 BASE 版本；文件夹对比时列表里选中的目录（本地端是「绑定对比对象」）。\n\
                                         没绑定对比对象、且记录两侧路径都认不出服务器端（按路径名含 server 判断）时，沿用记录原有的左右顺序。",
                                    )
                                    .clicked()
                                {
                                    self.cfg.bc_server_side = value.to_owned();
                                    self.persist();
                                    self.hint(format!(
                                        "已设置：调用 Beyond Compare 对比时，服务器端放在{text}侧"
                                    ));
                                }
                            }
                        });
                        ui.label(
                            RichText::new(
                                "服务器端 = 文件对比的 BASE 版本、文件夹对比里所选的目录；本地端 = 工作副本文件、绑定的对比对象",
                            )
                            .weak()
                            .size(11.0),
                        );
                        // BC 命令行只有 /filters= 一个开关能带设置，比较内容和规则传不过去，
                        // 只能靠 BC 自己的「所有文件夹比较视图」默认值兜底，这里把话说在前面
                        ui.label(
                            RichText::new(
                                "对比筛选条件按记录里的值用 /filters= 带过去；「比较内容」BC 没有命令行开关，\
                                 只能落到 BC 自己的会话默认值上——用下面那个开关，等价于在 BC 会话设置底部下拉里选「更新会话默认值」。",
                            )
                            .weak()
                            .size(11.0),
                        );
                        // 「比较内容」「比较文件名大小写」BC 没有命令行开关，只能写进它的会话默认值节点
                        match bcompare::default_rules() {
                            Some(saved) => {
                                let mut next = saved;
                                if ui
                                    .checkbox(&mut next.content, "新建文件夹比较默认开启「比较内容」")
                                    .on_hover_text(
                                        "写的是 BCSessions.xml 里「新建文件夹比较」那个默认值节点，\
                                         之后所有新建的文件夹比较都按它来（包括本程序打开的）。\n\
                                         Beyond Compare 退出时会整个覆盖会话存储，所以改之前要先全部关掉它；\
                                         写入前会自动备份成 BCSessions.xml.svnmanager.bak。",
                                    )
                                    .clicked()
                                {
                                    match bcompare::set_default_rules(&next) {
                                        Ok(note) => self.push(Level::Info, note),
                                        Err(error) => self.push(Level::Error, format!("设置失败：{error}")),
                                    }
                                }
                                if ui
                                    .checkbox(&mut next.filename_case, "默认「比较文件名大小写」")
                                    .on_hover_text(
                                        "勾上之后 Foo.java 和 foo.java 不再算同一个文件，会各算一条只在单侧存在的记录。\n\
                                         同样写进 BC 的会话默认值，改之前要先全部关掉 Beyond Compare。",
                                    )
                                    .clicked()
                                {
                                    match bcompare::set_default_rules(&next) {
                                        Ok(note) => self.push(Level::Info, note),
                                        Err(error) => self.push(Level::Error, format!("设置失败：{error}")),
                                    }
                                }
                            }
                            None => {
                                ui.label(
                                    RichText::new("（没找到 BC 的会话默认值节点，先在 Beyond Compare 里做一次文件夹对比）")
                                        .weak()
                                        .size(11.0),
                                );
                            }
                        }                        ui.horizontal(|ui| {
                            if ui
                                .button("重置试用（删除注册表 CacheID）")
                                .on_hover_text(
                                    "reg delete \"HKEY_CURRENT_USER\\Software\\Scooter Software\\Beyond Compare 4\" /v CacheID /f\nBeyond Compare 4 / 5 两个版本的键都会处理，改完需重启 Beyond Compare。",
                                )
                                .clicked()
                            {
                                self.reset_bc();
                            }
                        });
                        ui.label(
                            RichText::new(
                                "顶部「Beyond Compare」按钮优先用「更多 → 绑定对比对象」的目录，其次匹配对比记录（含记录里的对比筛选条件）；提交页选中文件后可用 BASE 版本与本地版本对比。",
                            )
                            .weak()
                            .size(11.5),
                        );
                    });
                    ui.separator();
                    ui.collapsing("仓库账号（留空则使用 svn 已缓存的凭据）", |ui| {
                        ui.horizontal(|ui| {
                            ui.label("用户名");
                            ui.add(TextEdit::singleline(&mut self.cfg.auth_user).desired_width(180.0));
                        });
                        ui.horizontal(|ui| {
                            ui.label("密码    ");
                            ui.add(TextEdit::singleline(&mut self.cfg.auth_pass).password(true).desired_width(180.0));
                        });
                        if ui.button("保存账号信息").clicked() {
                            self.svn.username = self.cfg.auth_user.clone();
                            self.svn.password = self.cfg.auth_pass.clone();
                            self.persist();
                            self.hint("账号设置已保存");
                            self.spawn_all_refresh();
                        }
                        ui.label(RichText::new("注意：密码以明文保存于配置文件中，仅在仓库没有缓存凭据时填写。").weak().size(11.5));
                    });
                    ui.collapsing("常规", |ui| {
                        ui.horizontal(|ui| {
                            ui.label("提交记录读取条数");
                            ui.add(egui::DragValue::new(&mut self.cfg.log_limit).range(5..=500).speed(5));
                        });
                        ui.horizontal(|ui| {
                            ui.label("自动刷新间隔（秒，0 = 关闭）");
                            ui.add(egui::DragValue::new(&mut self.cfg.auto_refresh).range(0..=3600).speed(10));
                        });
                        // 立即保存：这个开关只影响之后新打开的历史页，不用重新读取记录
                        let mut only_mine = self.cfg.history_only_mine;
                        if ui
                            .checkbox(&mut only_mine, "打开提交记录时默认只看本人记录")
                            .on_hover_text(
                                "开：进入「提交记录」页默认按本机 svn 登录人过滤，只列自己的提交。\n\
                                 关：默认列出所有人的提交。\n\
                                 只决定进入页面时的初始状态，页面里的「只看 xxx 的提交」随时可以切。",
                            )
                            .changed()
                        {
                            self.cfg.history_only_mine = only_mine;
                            self.persist();
                        }
                        // 立即保存：只影响之后的提交，不用重新检测
                        let mut update_after = self.cfg.update_after_commit;
                        if ui
                            .checkbox(&mut update_after, "提交成功后自动更新（svn update）")
                            .on_hover_text(
                                "开：提交完成后自动补跑一次 svn update，把整棵工作副本树的版本推到最新，\n\
                                 列表里的「本地 r」会立刻跟上新提交的版本（这一步不往输出区刷日志）。\n\
                                 关：只提交，不动版本号（根目录会一直停在提交前的版本）。\n\
                                 注意：update 会把别人已提交的改动一起拉到本地，可能带来合并甚至冲突。",
                            )
                            .changed()
                        {
                            self.cfg.update_after_commit = update_after;
                            self.persist();
                        }
                        // 开机自启：状态以注册表为准（用户手动删了 Run 值时这里如实显示），
                        // 配置里的字段只做记录；开关本身就持久化，不用再点「保存常规设置」
                        let mut auto_start = autostart::is_enabled();
                        if ui
                            .checkbox(&mut auto_start, "开机自动启动")
                            .on_hover_text(
                                "开：登录 Windows 后自动启动本程序（写入注册表 HKCU\\Software\\Microsoft\\\
                                 Windows\\CurrentVersion\\Run，指向当前 exe，不需要管理员权限）。\n\
                                 关：删除该注册表值。\n\
                                 注意：记录的是当前 exe 的完整路径，如果之后换了目录放新版，需要重新勾一次。",
                            )
                            .changed()
                        {
                            match autostart::set_enabled(auto_start) {
                                Ok(()) => {
                                    self.cfg.auto_start = auto_start;
                                    self.persist();
                                    self.hint(if auto_start {
                                        "已开启开机自启（下次登录 Windows 生效）"
                                    } else {
                                        "已关闭开机自启"
                                    });
                                }
                                Err(error) => {
                                    self.push(Level::Error, format!("设置开机自启失败：{error}"));
                                }
                            }
                        }
                        ui.horizontal(|ui| {
                            ui.label("主题");
                            for (value, text) in
                                [("system", "跟随系统"), ("light", "浅色"), ("dark", "深色")]
                            {
                                if ui.selectable_label(self.cfg.theme == value, text).clicked() {
                                    self.cfg.theme = value.to_owned();
                                    self.persist();
                                }
                            }
                        });
                        if ui.button("保存常规设置").clicked() {
                            self.persist();
                            self.hint("设置已保存");
                        }
                        ui.label(RichText::new(format!("配置文件：{}", config::config_file().display())).size(11.5).weak());
                        ui.label(RichText::new(self.font_note.clone()).size(11.5).weak());
                    });
                    ui.collapsing("AI 日志（调用 AI 生成工作日志）", |ui| {
                        ui.label(
                            RichText::new(
                                "在历史页勾选本人提交，把「文件名 + 修改内容」发给 AI 整理成工作日志。\n\
                                 接口需兼容 OpenAI /chat/completions 格式（DeepSeek、通义、Kimi 等均支持）；\
                                 只填 base 地址也行，程序会自动补全 /chat/completions。\
                                 密钥保存在本机配置文件，请求经系统 curl 发送。",
                            )
                            .size(11.5)
                            .weak(),
                        );
                        ui.horizontal(|ui| {
                            ui.label("服务地址");
                            ui.add_sized(
                                Vec2::new((ui.available_width() - 90.0).max(180.0), 22.0),
                                TextEdit::singleline(&mut self.cfg.ai_url).hint_text(
                                    "如 https://api.deepseek.com（通义：https://dashscope.aliyuncs.com/compatible-mode/v1）",
                                ),
                            );
                        });
                        ui.horizontal(|ui| {
                            ui.label("API Key");
                            ui.add_sized(
                                Vec2::new((ui.available_width() - 90.0).max(180.0), 22.0),
                                TextEdit::singleline(&mut self.cfg.ai_key)
                                    .password(true)
                                    .hint_text("sk-…"),
                            );
                        });
                        ui.horizontal(|ui| {
                            ui.label("模型名  ");
                            ui.add_sized(
                                Vec2::new((ui.available_width() - 90.0).max(180.0), 22.0),
                                TextEdit::singleline(&mut self.cfg.ai_model)
                                    .hint_text("如 deepseek-chat / gpt-4o-mini / qwen-plus"),
                            );
                        });
                        ui.label(RichText::new("口吻（生成窗口打开时预填，长期保留）").size(11.5).weak());
                        ui.add(
                            TextEdit::multiline(&mut self.cfg.ai_tone)
                                .desired_rows(2)
                                .desired_width(f32::INFINITY)
                                .hint_text("例：我是后端组的张三，日志写给部门周报，用第一人称、简洁正式"),
                        );
                        if ui.button("保存 AI 设置").clicked() {
                            self.persist();
                            self.hint("AI 日志设置已保存");
                        }
                        ui.label(
                            RichText::new("注意：密钥以明文保存于本机配置文件；生成时把勾选的提交说明与文件清单发给该服务，注意涉密内容。")
                                .weak()
                                .size(11.0),
                        );
                    });
                    ui.separator();
                    ui.collapsing("版本更新（检查并升级到新版本）", |ui| {
                        let official = update::is_official(&self.cfg.update_source);
                        ui.label(RichText::new("从哪里取最新版本").size(11.5).weak());
                        ui.horizontal(|ui| {
                            // 两个都先求值再判断：`||` 短路会让没求值的那个 radio 某帧消失
                            let pick_official = ui.radio_value(
                                &mut self.cfg.update_source,
                                update::SOURCE_OFFICIAL.to_owned(),
                                "官方源（GitHub 最新发布）",
                            );
                            let pick_custom = ui.radio_value(
                                &mut self.cfg.update_source,
                                update::SOURCE_CUSTOM.to_owned(),
                                "自定义源（服务端地址）",
                            );
                            // 仓库地址不单独占一行，收在悬停提示里
                            pick_official.clone().on_hover_text(format!(
                                "读取 {} 的最新 Release",
                                update::OFFICIAL_PAGE
                            ));
                            if pick_official.changed() || pick_custom.changed() {
                                self.persist();
                                self.hint(if update::is_official(&self.cfg.update_source) {
                                    "更新源已改为官方 GitHub 发布"
                                } else {
                                    "更新源已改为自建服务端，在下面填服务根目录地址"
                                });
                            }
                        });
                        if !official {
                            ui.label(
                                RichText::new(
                                    "在下方填写服务端托管地址，启动时会自动检测是否有新版本发布",
                                )
                                .size(11.5)
                                .weak(),
                            );
                            ui.horizontal(|ui| {
                                ui.add_sized(
                                    Vec2::new((ui.available_width() - 90.0).max(180.0), 22.0),
                                    TextEdit::singleline(&mut self.cfg.update_server)
                                        .hint_text("如 http://192.168.1.10:8666"),
                                );
                                if ui.button("保存地址").clicked() {
                                    self.persist();
                                    self.hint("更新服务端地址已保存");
                                }
                            });
                        }
                        ui.horizontal(|ui| {
                            let checking = self.pool.has(Kind::CheckUpdate, usize::MAX);
                            if ui
                                .button(if checking { "检查中…" } else { "检查更新" })
                                .clicked()
                                && !checking
                            {
                                self.spawn_update_check();
                            }
                            let has_new = self
                                .update_info
                                .as_ref()
                                .is_some_and(|m| update::is_newer(m.version.trim(), APP_VERSION));
                            if ui.button("立即更新").clicked() {
                                if has_new {
                                    // 与顶部「↑ 新版本」按钮同一个确认对话框
                                    self.show_update_confirm = true;
                                } else {
                                    self.hint(if self.update_info.is_some() {
                                        "当前已是最新版本"
                                    } else {
                                        "请先「检查更新」"
                                    });
                                }
                            }
                        });
                        ui.label(RichText::new(format!("当前版本：V{APP_VERSION}")).size(11.5).weak());
                        if let Some(m) = &self.update_info {
                            let mut line = format!("最新版本：V{}", m.version.trim());
                            if !m.published_at.trim().is_empty() {
                                line.push_str(&format!("（发布于 {}）", m.published_at.trim()));
                            }
                            ui.label(RichText::new(line).size(11.5).weak());
                            if !m.notes.trim().is_empty() {
                                // 多行更新说明按原文渲染：标题一行，说明整块跟随
                                ui.label(RichText::new("更新说明").strong().size(11.5));
                                ui.label(RichText::new(m.notes.trim()).size(11.5).weak());
                            }
                        } else if !official && self.cfg.update_server.trim().is_empty() {
                            ui.label(RichText::new("未配置服务端地址，不会检查更新").size(11.5).weak());
                        }
                    });
                });
            });
        self.show_settings = open;
    }

}

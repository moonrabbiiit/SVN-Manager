//! AI 生成工作日志。
//!
//! 用法：在各目录「提交记录」页里，本人提交的版本行左侧有复选框，勾选后收进
//! 「AI 工作日志」窗口；窗口里把这些提交整理成「文件名 + 修改内容」的文本发给
//! AI（OpenAI 兼容的 /chat/completions），一次性拿回整篇日志（不做流式传输）。
//!
//! 接口地址 / API Key / 模型由用户在设置里自己填；密钥通过 curl 的配置文件传给
//! curl（不出现在命令行参数里，任务管理器看不到）；请求用系统自带的 curl.exe，
//! 不给程序增加 HTTP 依赖。内网地址照 update.rs 的规则绕过系统代理。

use std::path::PathBuf;

use egui::{Align, Color32, FontId, Frame, RichText, ScrollArea, TextEdit, Ui, Vec2};

use crate::jobs::{Data, Kind};
use crate::svn::LogEntry;
use crate::{ink, SvnApp};

/// 一次 AI 生成时单文件条数超过这个值就提醒可能消耗较多 token
const FILE_WARN_LIMIT: usize = 20;

/// 勾选进「AI 工作日志」的一条提交（快照：跨目录、跨页面都要能用，
/// 所以把目录别名和整条 LogEntry 都拷进来，不依赖历史页还开着）。
#[derive(Clone, Debug)]
pub struct AiPick {
    /// 来源目录在目录列表里的编号（同一仓库在不同目录行下也不会混）
    pub dir: usize,
    /// 来源目录的别名（目录列表里显示的名字）
    pub dir_label: String,
    /// 那条提交记录（版本 / 作者 / 时间 / 说明 / 涉及文件）
    pub entry: LogEntry,
}

/// 勾选里一共涉及多少个文件（用于 >20 的 token 提醒）
pub fn file_count(picks: &[AiPick]) -> usize {
    picks.iter().map(|pick| pick.entry.paths.len()).sum()
}

/// 把勾选的提交整理成发给 AI 的「文件名 + 修改内容」文本。
/// 每条提交一块：目录 / 版本 / 时间 / 提交说明 / 涉及文件（A/M/D/R 标记 + 路径）。
pub fn build_content(picks: &[AiPick]) -> String {
    let mut text = String::new();
    for pick in picks {
        let entry = &pick.entry;
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!(
            "【{}】r{} {} 作者：{}\n",
            pick.dir_label, entry.revision, entry.date, entry.author
        ));
        let message = entry.message.trim();
        text.push_str(&format!(
            "修改内容：{}\n",
            if message.is_empty() { "（无提交说明）" } else { message }
        ));
        text.push_str(&format!("涉及文件（{}）：\n", entry.paths.len()));
        for path in &entry.paths {
            text.push_str(&format!("  {} {}\n", path.action, path.path));
        }
    }
    text
}

/// 组装请求体（OpenAI 兼容 /chat/completions，非流式）。
pub fn build_body(model: &str, system: &str, user: &str) -> String {
    serde_json::json!({
        "model": model,
        "stream": false,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user }
        ]
    })
    .to_string()
}

/// 组装发给 AI 的两段提示词。口吻为空时用中性的默认说法。
pub fn build_prompt(tone: &str, extra: &str, content: &str) -> (String, String) {
    let system = "你是一名认真负责的软件开发工程师，现在要根据 SVN 提交记录为提交人撰写工作日志。\
                  只依据提供的提交记录归纳，不要编造记录之外的内容。"
        .to_owned();
    let mut user = String::from("请根据下面的 SVN 提交记录，生成一份工作日志。\n\n要求：\n\
        - 把提交记录归纳成若干条工作事项，每条一到两句话，突出完成了什么、改动了什么、解决了什么问题。\n\
        - 同一文件的多次相关改动尽量合并描述，不要逐版本流水账。\n\
        - 直接输出日志正文，不要解释，不要用 Markdown 代码块包裹。\n");
    let tone = tone.trim();
    if tone.is_empty() {
        user.push_str("- 语言简洁、正式。\n");
    } else {
        user.push_str(&format!("- 口吻与身份：{tone}\n"));
    }
    let extra = extra.trim();
    if !extra.is_empty() {
        user.push_str(&format!("- 其他要求：{extra}\n"));
    }
    user.push_str("\n提交记录如下（每条包含目录、版本、修改内容与涉及文件）：\n\n");
    user.push_str(content);
    (system, user)
}

/// 服务地址自动补全：OpenAI 兼容接口的完整地址固定以 /chat/completions 结尾，
/// 但用户经常只填 base 地址（如 https://dashscope.aliyuncs.com/compatible-mode/v1
/// 或 https://api.deepseek.com），少一段路径服务端就会报 url error，这里统一补上。
pub fn normalize_url(url: &str) -> String {
    let mut url = url.trim().trim_end_matches('/').to_owned();
    if !url.ends_with("/chat/completions") {
        url.push_str("/chat/completions");
    }
    url
}

/// 调 AI 接口，返回日志正文。地址 / Key / 模型任一为空都直接报错，不发起请求。
pub fn request(
    url: &str,
    key: &str,
    model: &str,
    tone: &str,
    extra: &str,
    content: &str,
) -> Result<String, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("尚未配置 AI 服务地址（设置 → AI 日志）".to_owned());
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(format!("AI 服务地址要以 http:// 或 https:// 开头：{url}"));
    }
    let url = normalize_url(url);
    let key = key.trim();
    if key.is_empty() {
        return Err("尚未配置 API Key（设置 → AI 日志）".to_owned());
    }
    let model = model.trim();
    if model.is_empty() {
        return Err("尚未配置模型名（设置 → AI 日志）".to_owned());
    }
    let (system, user) = build_prompt(tone, extra, content);
    let body = build_body(model, &system, &user);
    let response = post_json(&url, key, &body)?;
    parse_response(&response)
}

/// 用系统 curl 发 POST。请求头（含 API Key）写进 curl 的 `--config` 配置文件，
/// 不走命令行参数——命令行在任务管理器里谁都能看到，配置文件用完即删。
fn post_json(url: &str, key: &str, body: &str) -> Result<String, String> {
    let curl = crate::update::find_curl()
        .ok_or("未找到系统 curl.exe（Windows 10 1803+ 自带），无法调用 AI 接口")?;
    let dir = std::env::temp_dir().join("SVNManager").join("ai");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败：{e}"))?;
    let body_path = dir.join("request.json");
    let cfg_path = dir.join("curl.cfg");
    let resp_path = dir.join("response.json");
    let to_curl = |path: &PathBuf| path.display().to_string().replace('\\', "/");
    std::fs::write(&body_path, body.as_bytes()).map_err(|e| format!("写请求体失败：{e}"))?;
    let cfg_text = format!(
        "request = \"POST\"\n\
         header = \"Content-Type: application/json\"\n\
         header = \"Authorization: Bearer {key}\"\n\
         data-binary = \"@{}\"\n\
         output = \"{}\"\n\
         silent\nshow-error\n\
         connect-timeout = \"20\"\nmax-time = \"300\"\n",
        to_curl(&body_path),
        to_curl(&resp_path)
    );
    std::fs::write(&cfg_path, cfg_text.as_bytes()).map_err(|e| format!("写 curl 配置失败：{e}"))?;

    let mut cmd = std::process::Command::new(&curl);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // 界面程序：不能让 curl 弹出黑框
        cmd.creation_flags(crate::svn::CREATE_NO_WINDOW);
    }
    cmd.arg("--config").arg(&cfg_path).arg(url);
    // 内网地址绕过系统代理（http_proxy 环境变量会把内网请求转发出去连不回来）
    if crate::update::is_private_host(url) {
        cmd.arg("--noproxy").arg("*");
    }
    let output = cmd.output();
    // 密钥已经用完了，请求体也发完了：不管成败都先删掉这两个文件
    let _ = std::fs::remove_file(&cfg_path);
    let _ = std::fs::remove_file(&body_path);
    let output = output.map_err(|e| format!("无法启动 {}：{e}", curl.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "curl 退出码 {}：{}",
            output.status.code().unwrap_or(-1),
            if stderr.trim().is_empty() { "无错误输出".to_owned() } else { stderr.trim().to_owned() }
        ));
    }
    let text = std::fs::read_to_string(&resp_path)
        .map_err(|e| format!("读取 AI 返回失败：{e}（服务端可能没有返回内容）"))?;
    let _ = std::fs::remove_file(&resp_path);
    Ok(text)
}

/// 解析 AI 返回：成功取 `choices[0].message.content`，失败把 `error.message`
/// 或原文前 300 字挑出来给人看。
fn parse_response(text: &str) -> Result<String, String> {
    let trimmed = text.trim();
    let value: serde_json::Value = serde_json::from_str(trimmed)
        .map_err(|_| format!("AI 返回的不是 JSON：{}", preview(trimmed)))?;
    if let Some(content) = value
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
    {
        if !content.trim().is_empty() {
            return Ok(content.to_owned());
        }
    }
    if let Some(message) = value.pointer("/error/message").and_then(|m| m.as_str()) {
        let mut text = format!("AI 接口报错：{message}");
        // 阿里云百炼对错误路径 / 端点不匹配统一报 url error（见其错误码文档）
        if message.to_ascii_lowercase().contains("url error") {
            text.push_str(
                "\n提示：一般是服务地址不对。OpenAI 兼容地址要以 /chat/completions 结尾，\n\
                 通义百炼填 https://dashscope.aliyuncs.com/compatible-mode/v1 即可（程序会自动补全）；\n\
                 若地址本身完整，请检查模型名与端点类型是否匹配（多模态模型不能用纯文本端点）。",
            );
        }
        return Err(text);
    }
    Err(format!("无法从 AI 返回里取到日志内容：{}", preview(trimmed)))
}

fn preview(text: &str) -> String {
    let taken: String = text.chars().take(300).collect();
    if text.chars().count() > 300 {
        format!("{taken}…")
    } else {
        taken
    }
}

impl SvnApp {
    /// 把勾选的提交发给 AI（后台任务，结果整块回传）
    pub fn spawn_ai_log(&mut self) {
        if self.pool.has(Kind::AiLog, usize::MAX) {
            return;
        }
        let picks = self.ai_picks.clone();
        if picks.is_empty() {
            return;
        }
        let url = self.cfg.ai_url.clone();
        let key = self.cfg.ai_key.clone();
        let model = self.cfg.ai_model.clone();
        let tone = self.cfg.ai_tone.clone();
        let extra = self.ai_extra.clone();
        self.pool
            .spawn(Kind::AiLog, usize::MAX, "AI 生成工作日志".into(), move |sink| {
                let files = file_count(&picks);
                sink.line(format!(
                    "$ AI 请求：{model} @ {}（{} 个版本 / {files} 个文件，非流式，等待整篇返回）",
                    url.trim(),
                    picks.len()
                ));
                match request(&url, &key, &model, &tone, &extra, &build_content(&picks)) {
                    Ok(content) => {
                        sink.line(format!("AI 日志已返回（{} 字）", content.chars().count()));
                        Data::AiLog { ok: true, content, message: String::new() }
                    }
                    Err(message) => Data::AiLog {
                        ok: false,
                        content: String::new(),
                        message,
                    },
                }
            });
    }

    /// 「AI 工作日志」窗口：汇总各目录勾选的提交，编辑口吻与额外要求，一键生成。
    pub fn worklog_window(&mut self, ctx: &egui::Context) {
        if !self.show_worklog {
            return;
        }
        let mut open = self.show_worklog;
        let generating = self.pool.has(Kind::AiLog, usize::MAX);
        let configured = !self.cfg.ai_url.trim().is_empty()
            && !self.cfg.ai_key.trim().is_empty()
            && !self.cfg.ai_model.trim().is_empty();
        let files = file_count(&self.ai_picks);
        let model_note = if configured {
            format!("模型：{}", self.cfg.ai_model.trim())
        } else {
            "未配置 AI 服务（设置 → AI 日志）".to_owned()
        };
        egui::Window::new("AI 工作日志")
            .open(&mut open)
            .collapsible(false)
            .default_size(Vec2::new(880.0, 800.0))
            .min_width(780.0)
            .min_height(620.0)
            .default_pos(egui::pos2(240.0, 50.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("把勾选的提交交给 AI，按你的口吻整理成工作日志").size(13.0));
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(model_note).weak().size(11.5));
                    });
                });
                if !configured {
                    note_card(
                        ui,
                        "还没有配置 AI 服务：请到「设置 → AI 日志」填写接口地址、API Key 和模型名。\n\
                         接口需兼容 OpenAI /chat/completions 格式（DeepSeek、通义、Kimi 等都支持）。",
                        false,
                    );
                }
                ui.add_space(2.0);

                // ---- 已选提交 ----
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("已选提交（{} 个版本 / {files} 个文件）", self.ai_picks.len()))
                            .strong()
                            .size(13.5),
                    );
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("清空").clicked() {
                            self.ai_picks.clear();
                        }
                        ui.label(
                            RichText::new(
                                "勾选来自各目录「提交记录」页里本人提交行左侧的复选框（日志模式开启后显示）",
                            )
                            .weak()
                            .size(11.5),
                        );
                    });
                });
                // ---- 从其他目录补充勾选：跨目录汇总的入口就放在窗口里 ----
                ui.horizontal(|ui| {
                    ui.label(RichText::new("还要加其他目录的提交：").size(12.5));
                    if self.cfg.dirs.is_empty() {
                        ui.label(RichText::new("目录列表为空").weak().size(12.0));
                    } else {
                        if self.ai_pick_dir >= self.cfg.dirs.len() {
                            self.ai_pick_dir = 0;
                        }
                        egui::ComboBox::from_id_salt("ai_pick_dir")
                            .selected_text(self.dir_label(self.ai_pick_dir))
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                for index in 0..self.cfg.dirs.len() {
                                    let label = self.dir_label(index);
                                    ui.selectable_value(
                                        &mut self.ai_pick_dir,
                                        index,
                                        label,
                                    );
                                }
                            });
                        if ui
                            .button("打开它的提交记录页 →")
                            .on_hover_text(
                                "打开所选目录的「提交记录」页，在本人提交行左侧勾选；\n\
                                 勾选完点右上角的「AI 日志（N）」回到本窗口，多个目录反复操作即可",
                            )
                            .clicked()
                        {
                            let dir = self.ai_pick_dir;
                            self.open_history(dir);
                            // 关掉本窗口腾出屏幕；勾选都在，回来时原样显示
                            self.show_worklog = false;
                        }
                    }
                });
                if self.ai_picks.is_empty() {
                    note_card(
                        ui,
                        "还没有勾选提交：在上面选一个目录、打开它的提交记录页，在本人提交的版本行左侧勾选。\n\
                         可以对多个目录反复操作（比如 hrp_server 和 vue_ss_server），勾选会全部汇总到这里；\n\
                         勾选框在日志模式开启时才显示（右上角「AI 日志」开关控制）；没看到勾选框说明该行不是本机登录人的提交。",
                        false,
                    );
                } else {
                    if files > FILE_WARN_LIMIT {
                        note_card(
                            ui,
                            &format!(
                                "注意：已选 {files} 个文件（超过 {FILE_WARN_LIMIT} 个）：一次性发送可能消耗较多 token，\
                                 且超出模型上下文时会被截断，建议分批生成。"
                            ),
                            true,
                        );
                    }
                    ScrollArea::vertical()
                        .id_salt("ai_picks")
                        .max_height(210.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 3.0;
                            let mut remove: Option<usize> = None;
                            for (index, pick) in self.ai_picks.iter().enumerate() {
                                let entry = &pick.entry;
                                let first = entry
                                    .message
                                    .lines()
                                    .next()
                                    .unwrap_or("(无提交说明)")
                                    .to_owned();
                                let (fill, stroke) = card_colors(ui);
                                Frame::new()
                                    .inner_margin(5.0)
                                    .corner_radius(5.0)
                                    .fill(fill)
                                    .stroke(egui::Stroke::new(1.0, stroke))
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new(format!("【{}】", pick.dir_label))
                                                    .strong()
                                                    .size(12.5),
                                            );
                                            ui.label(
                                                RichText::new(format!("r{}", entry.revision))
                                                    .monospace()
                                                    .size(12.5)
                                                    .color(ink(ui, Color32::from_rgb(120, 190, 240))),
                                            );
                                            ui.label(RichText::new(&entry.date).size(11.5).weak());
                                            ui.label(
                                                RichText::new(format!("{} 项", entry.paths.len()))
                                                    .size(11.0)
                                                    .weak(),
                                            );
                                            ui.with_layout(
                                                egui::Layout::right_to_left(Align::Center),
                                                |ui| {
                                                    if ui
                                                        .add(
                                                            // ✕(U+2715) 在中文字体和 egui 内置字体里都没有字形，会显示成方块；
                                                            // ×(U+00D7) 所有字体都有
                                                            egui::Button::new(
                                                                RichText::new("×").size(14.0).strong(),
                                                            )
                                                            .small(),
                                                        )
                                                        .on_hover_text("从本次生成里移除这条提交")
                                                        .clicked()
                                                    {
                                                        remove = Some(index);
                                                    }
                                                },
                                            );
                                        });
                                        // 说明一行截断，悬停看全文
                                        let full = entry.message.trim().to_owned();
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(first).size(12.0),
                                            )
                                            .truncate(),
                                        )
                                        .on_hover_text(full);
                                    });
                            }
                            if let Some(index) = remove {
                                self.ai_picks.remove(index);
                            }
                        });
                }
                ui.add_space(4.0);

                // ---- 口吻（永久保存）----
                ui.label(RichText::new("口吻（保存后长期有效，每次生成都带上）").strong().size(13.0));
                let tone_before = self.cfg.ai_tone.clone();
                ui.add(
                    TextEdit::multiline(&mut self.cfg.ai_tone)
                        .desired_rows(2)
                        // 比常规两行再加高 30px：口吻通常要写好几句话，太挤
                        .min_size(Vec2::new(0.0, 76.0))
                        .desired_width(f32::INFINITY)
                        .hint_text(
                            "例：我是后端组的张三，日志写给部门周报，用第一人称、简洁正式，不用表情符号",
                        ),
                );
                if self.cfg.ai_tone != tone_before {
                    self.persist();
                }
                ui.add_space(2.0);

                // ---- 额外提示词（本次填写）----
                ui.label(RichText::new("额外提示词（可选，只对本次生成生效）").strong().size(13.0));
                ui.add(
                    TextEdit::multiline(&mut self.ai_extra)
                        .desired_rows(2)
                        // 与口吻框一致，加高 30px
                        .min_size(Vec2::new(0.0, 76.0))
                        .desired_width(f32::INFINITY)
                        .hint_text("例：重点总结订单模块的改动；末尾补一段「明日计划」；字数控制在 300 字以内"),
                );
                ui.add_space(6.0);

                // ---- 生成 ----
                ui.horizontal(|ui| {
                    let (fill, text_color) = if ui.visuals().dark_mode {
                        (Color32::from_rgb(70, 130, 245), Color32::WHITE)
                    } else {
                        (Color32::from_rgb(36, 105, 230), Color32::WHITE)
                    };
                    let button = egui::Button::new(
                        RichText::new(if generating { "生成中…" } else { "生成日志" })
                            .strong()
                            .size(14.0)
                            .color(text_color),
                    )
                    .fill(fill)
                    .corner_radius(6.0)
                    .min_size(Vec2::new(110.0, 30.0));
                    let enabled = configured && !self.ai_picks.is_empty() && !generating;
                    if ui
                        .add_enabled(enabled, button)
                        .on_hover_text(if configured {
                            if self.ai_picks.is_empty() {
                                "请先勾选至少一条提交"
                            } else {
                                "把勾选的提交发给 AI，整篇日志返回后显示在下方（非流式，可能需要几十秒）"
                            }
                        } else {
                            "请先在「设置 → AI 日志」里配置接口地址、API Key 和模型名"
                        })
                        .clicked()
                    {
                        self.ai_error.clear();
                        self.ai_result.clear();
                        self.spawn_ai_log();
                    }
                    if generating {
                        ui.spinner();
                        ui.label(
                            RichText::new("正在生成日志…（已把提交记录发给 AI，返回后自动显示）")
                                .size(12.5),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        // 窗口只留这一个收尾按钮：点它退出日志模式并关窗（标题栏 × 也能关窗，
                        // 模式保持开启）；单独的「关闭」按钮已按需求移除
                        if ui
                            .button("取消日志模式")
                            .on_hover_text(
                                "退出日志模式并关闭本窗口：提交记录页里的勾选框会隐藏。\n\
                                 已勾选的提交保留，重新开启日志模式后继续显示；不想保留用上面的「清空」",
                            )
                            .clicked()
                        {
                            self.ai_mode = false;
                            self.show_worklog = false;
                        }
                    });
                });
                if !self.ai_error.is_empty() {
                    note_card(ui, &format!("生成失败：{}", self.ai_error), true);
                }

                // ---- 结果 ----
                if !self.ai_result.is_empty() || !generating {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("生成结果（可直接编辑）").strong().size(13.0));
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .add_enabled(!self.ai_result.is_empty(), egui::Button::new("复制全部"))
                                .clicked()
                            {
                                ui.ctx().copy_text(self.ai_result.clone());
                                ui.ctx().request_repaint();
                            }
                            if ui
                                .add_enabled(!generating, egui::Button::new("重新生成"))
                                .on_hover_text("用当前口吻、提示词与勾选内容再生成一次（覆盖现有结果）")
                                .clicked()
                            {
                                self.ai_error.clear();
                                self.ai_result.clear();
                                self.spawn_ai_log();
                            }
                        });
                    });
                    let (fill, stroke) = card_colors(ui);
                    Frame::new()
                        .inner_margin(6.0)
                        .corner_radius(5.0)
                        .fill(fill)
                        .stroke(egui::Stroke::new(1.0, stroke))
                        .show(ui, |ui| {
                            ScrollArea::vertical()
                                .id_salt("ai_result")
                                .max_height(350.0)
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    ui.add(
                                        TextEdit::multiline(&mut self.ai_result)
                                            .font(FontId::proportional(13.0))
                                            .desired_width(f32::INFINITY)
                                            .frame(egui::Frame::NONE),
                                    );
                                });
                        });
                }
            });
        self.show_worklog = open;
    }
}

/// 卡片配色：与提交记录明细窗口一致——浅色白底灰边，深色深底浅灰边
pub(crate) fn card_colors(ui: &Ui) -> (Color32, Color32) {
    if ui.visuals().dark_mode {
        (Color32::from_gray(40), Color32::from_gray(65))
    } else {
        (Color32::WHITE, Color32::from_gray(200))
    }
}

/// 提示 / 警告横幅卡片：`warn` 为真时用橙黄底
pub(crate) fn note_card(ui: &mut Ui, text: &str, warn: bool) {
    let (fill, stroke, color) = if ui.visuals().dark_mode {
        (
            Color32::from_rgb(58, 50, 22),
            Color32::from_rgb(120, 100, 40),
            Color32::from_rgb(240, 210, 140),
        )
    } else {
        (
            Color32::from_rgb(255, 249, 230),
            Color32::from_rgb(225, 190, 110),
            Color32::from_rgb(140, 100, 20),
        )
    };
    let _ = warn;
    Frame::new()
        .inner_margin(6.0)
        .corner_radius(5.0)
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, stroke))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(text).size(12.0).color(color));
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::svn::LogPath;

    fn pick(revision: &str, message: &str, paths: &[(&str, &str)]) -> AiPick {
        AiPick {
            dir: 0,
            dir_label: "订单服务".to_owned(),
            entry: LogEntry {
                revision: revision.to_owned(),
                author: "zhangsan".to_owned(),
                date: "2026-09-07 10:00:00".to_owned(),
                message: message.to_owned(),
                paths: paths
                    .iter()
                    .map(|(action, path)| LogPath {
                        action: action.chars().next().unwrap(),
                        kind: "file".to_owned(),
                        path: path.to_string(),
                    })
                    .collect(),
            },
        }
    }

    /// 文件名 + 修改内容必须都进文本：目录、版本、说明、动作与路径一项不能少
    #[test]
    fn content_keeps_files_and_messages() {
        let picks = vec![
            pick("101", "修复下单失败\n\n详情略", &[("M", "src/order.java"), ("A", "src/new.java")]),
            pick("102", "调整库存扣减", &[("D", "src/old.java")]),
        ];
        let text = build_content(&picks);
        assert!(text.contains("【订单服务】r101"));
        assert!(text.contains("修改内容：修复下单失败"));
        assert!(text.contains("M src/order.java"));
        assert!(text.contains("A src/new.java"));
        assert!(text.contains("【订单服务】r102"));
        assert!(text.contains("D src/old.java"));
        assert_eq!(file_count(&picks), 3);
    }

    /// 口吻与额外提示词都要进用户提示词；口吻为空时用默认说法兜底
    #[test]
    fn prompt_carries_tone_and_extra() {
        let content = build_content(&[]);
        let (system, user) = build_prompt("第一人称，简洁正式", "重点写订单模块", &content);
        assert!(system.contains("不要编造"));
        assert!(user.contains("口吻与身份：第一人称，简洁正式"));
        assert!(user.contains("其他要求：重点写订单模块"));
        let (_, plain) = build_prompt("  ", "", &content);
        assert!(plain.contains("语言简洁、正式"));
        assert!(!plain.contains("其他要求"));
    }

    /// 请求体必须是合法 JSON：模型、两条消息、非流式
    #[test]
    fn body_is_valid_openai_json() {
        let (system, user) = build_prompt("t", "", "c");
        let body = build_body("deepseek-chat", &system, &user);
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["model"], "deepseek-chat");
        assert_eq!(value["stream"], false);
        assert_eq!(value["messages"][0]["role"], "system");
        assert_eq!(value["messages"][1]["role"], "user");
    }

    /// 返回解析：标准 OpenAI 结构取 content；错误结构取 error.message；乱返回给原文预览
    #[test]
    fn response_parsing() {
        let ok = r#"{"choices":[{"message":{"role":"assistant","content":"日志正文\n第二条"}}]}"#;
        assert_eq!(parse_response(ok).unwrap(), "日志正文\n第二条");
        let err = r#"{"error":{"message":"Incorrect API key"}}"#;
        assert!(parse_response(err).unwrap_err().contains("Incorrect API key"));
        assert!(parse_response("not json").unwrap_err().contains("not json"));
        let empty = r#"{"choices":[]}"#;
        assert!(parse_response(empty).unwrap_err().contains("无法从 AI 返回里取到"));
        // url error 附带排查提示（阿里云百炼对错误路径统一这么报）
        let url_err = r#"{"error":{"message":"url error, please check url！"}}"#;
        let hint = parse_response(url_err).unwrap_err();
        assert!(hint.contains("chat/completions"));
    }

    /// 服务地址补全：base 地址补上 /chat/completions，完整地址原样保留
    #[test]
    fn url_normalization() {
        assert_eq!(
            normalize_url("https://dashscope.aliyuncs.com/compatible-mode/v1"),
            "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions"
        );
        assert_eq!(
            normalize_url("https://api.deepseek.com"),
            "https://api.deepseek.com/chat/completions"
        );
        assert_eq!(
            normalize_url("https://api.deepseek.com/v1/"),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(
            normalize_url("https://api.deepseek.com/v1/chat/completions"),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(normalize_url(" https://api.deepseek.com "), "https://api.deepseek.com/chat/completions");
    }
}

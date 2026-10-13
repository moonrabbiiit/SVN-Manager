//! 顶部状态栏：仓库连接状态、页面切换、主题与设置入口，以及有新版本时的那个按钮。

use crate::repo::RepoKind;
use crate::{APP_TITLE, DirView, RepoApp, ink, quiet};
use egui::{Align, Color32, Layout, RichText, Ui};
use crate::jobs::Kind;
use crate::ui::AlignedRow;

impl RepoApp {
    // ------------------------------------------------------------ 顶部状态栏

    fn connection(&self) -> (Color32, String, String) {
        let green = crate::ui::OK;
        let amber = crate::ui::AMBER;
        let red = crate::ui::ERR;
        let gray = Color32::from_gray(150);
        if self.probing || self.pool.has(Kind::DetectSvn, usize::MAX) {
            return (amber, "正在寻找 svn.exe".into(), "自动探测 SVN 命令行工具".into());
        }
        if !self.svn.available() {
            return (
                red,
                "未找到 svn.exe".into(),
                format!("路径无效：{}", self.svn.exe.display()),
            );
        }
        if self.cfg.dirs.is_empty() {
            return (
                gray,
                format!("svn {} 可用，尚未添加目录", self.version.clone().unwrap_or_default()),
                "添加工作副本目录后会自动检测连接".into(),
            );
        }
        let total = self.cfg.dirs.len();
        let checked: Vec<&DirView> = self.dirs.iter().filter(|d| d.remote.is_some()).collect();
        if checked.is_empty() {
            return (gray, "等待检测".into(), "尚未完成连接检测".into());
        }
        let ok = checked.iter().filter(|d| d.remote == Some(true)).count();
        // 后端字样：列表里只有一种仓库时带名字（SVN / Git），混着就不细分
        let git = checked.iter().filter(|d| d.kind == RepoKind::Git).count();
        let backend = match (git, checked.len() - git) {
            (0, _) => " SVN",
            (_, 0) => " Git",
            _ => "",
        };
        if ok == total {
            (green, format!("已连接{backend}（{ok}/{total}）"), "全部目录均可访问仓库".into())
        } else if ok == 0 {
            let reason = checked
                .iter()
                .map(|d| d.remote_msg.as_str())
                .find(|m| !m.is_empty())
                .unwrap_or("无法访问仓库")
                .to_owned();
            (red, format!("未连接{backend}（0/{total}）"), reason)
        } else {
            (amber, format!("部分连接（{ok}/{total}）"), "存在无法访问的目录".into())
        }
    }

    pub(crate) fn header(&mut self, ui: &mut Ui) {
        let (color, text, reason) = self.connection();
        let color = ink(ui, color);
        let running = self.pool.running();
        let busy_label = self
            .selected
            .and_then(|index| self.pool.busy_label(index))
            .or_else(|| if running > 0 { Some("任务执行中".into()) } else { None });
        ui.aligned_row(|ui| {
            ui.label(RichText::new(APP_TITLE).size(19.0).strong());
            ui.separator();
            if running > 0 {
                ui.spinner();
            }
            ui.label(
                RichText::new(format!("● {text}")).color(color).strong(),
            )
            .on_hover_text(reason);
            if let Some(label) = busy_label {
                ui.label(RichText::new(format!("{running} 个任务：{label}")).weak().size(12.0));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // 检查到新版本时这里常驻入口，点击直接弹确认框，确认后即开始更新。
                // 文字七彩流动：每个字错开一点色相（LayoutJob 逐字上色），整行铺满一道
                // 彩虹并随时间向前流动，约 4 秒转一圈；按钮在屏幕上就要持续重绘。
                // 有没有新版由 update_ready 定（后台检查那趟按文件 sha256 算好的结论）：
                // 版本号没动但换了构建时，这里照样亮起
                if self.update_ready {
                    let latest = self
                        .update_info
                        .as_ref()
                        .map(|m| m.version.trim().to_owned())
                        .unwrap_or_default();
                    let text = format!("↑ 新版本 V{latest}");
                    let base = (ui.input(|i| i.time) / 4.0) % 1.0;
                    // 深色主题用亮色，浅色主题（白底）压暗到一半亮度，否则会看不清
                    let (sat, val) = if ui.visuals().dark_mode { (0.92, 0.85) } else { (0.95, 0.55) };
                    let font = egui::TextStyle::Button.resolve(ui.style());
                    let mut job = egui::text::LayoutJob::default();
                    for (index, ch) in text.chars().enumerate() {
                        let hue = ((base + index as f64 / 12.0) % 1.0) as f32;
                        let color = egui::Color32::from(egui::ecolor::Hsva::new(hue, sat, val, 1.0));
                        job.append(
                            &ch.to_string(),
                            0.0,
                            egui::TextFormat::simple(font.clone(), color),
                        );
                    }
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(66));
                    if ui
                        .button(job)
                        .on_hover_text("有可用更新，点击确认后直接开始（下载新版本并覆盖重启）")
                        .clicked()
                    {
                        self.show_update_confirm = true;
                    }
                }
                if quiet(ui, |ui| ui.button("设置")).clicked() {
                    self.show_settings = !self.show_settings;
                }
                let theme_name = match self.cfg.theme.as_str() {
                    "light" => "浅色",
                    "dark" => "深色",
                    _ => "跟随系统",
                };
                if quiet(ui, |ui| ui.button(format!("主题：{theme_name}")))
                    .on_hover_text("点击在 跟随系统 / 浅色 / 深色 之间循环切换")
                    .clicked()
                {
                    self.cfg.theme = match self.cfg.theme.as_str() {
                        "system" => "light",
                        "light" => "dark",
                        _ => "system",
                    }
                    .to_owned();
                    self.persist();
                }
                if quiet(ui, |ui| ui.button("Beyond Compare"))
                    .on_hover_text("启动Beyond Compare")
                    .clicked()
                {
                    self.open_bcompare_matched();
                }
                // AI 日志：排在 Beyond Compare 之后、与其他按钮同款配色（不加粗）。
                // 开关式入口：第一次点开启「日志模式」（各目录提交记录页里本人提交的
                // 版本行出现勾选框）；开着但没勾选时显示「取消日志模式」，再点退出；
                // 有勾选后显示数量，点击弹出「AI 工作日志」窗口（模式保持开启，
                // 方便关掉窗口后去别的目录继续勾）。
                let picks = self.ai_picks.len();
                let (ai_label, ai_hover) = if !self.ai_mode {
                    (
                        "AI 日志".to_owned(),
                        "开启日志模式：各目录「提交记录」页里本人提交的版本行会出现勾选框，\n\
                         勾选后本按钮变成「AI 日志（N）」，点击弹出「AI 工作日志」窗口".to_owned(),
                    )
                } else if picks == 0 {
                    (
                        "取消日志模式".to_owned(),
                        "退出日志模式：隐藏提交记录页里的勾选框（已勾选的提交保留，重新开启后继续显示）".to_owned(),
                    )
                } else {
                    (
                        format!("AI 日志（{picks}）"),
                        "打开「AI 工作日志」窗口：把勾选的提交交给 AI，按「口吻」整理成工作日志\n\
                         （接口在 设置 → AI 日志 里配置；模式保持开启，可到其他目录继续勾选）".to_owned(),
                    )
                };
                if quiet(ui, |ui| ui.button(ai_label))
                    .on_hover_text(ai_hover)
                    .clicked()
                {
                    if !self.ai_mode {
                        self.ai_mode = true;
                    } else if picks == 0 {
                        self.ai_mode = false;
                    } else {
                        self.show_worklog = true;
                    }
                }
                if quiet(ui, |ui| ui.button("提意见"))
                    .on_hover_text("在 GitHub 上给本项目提 issue：报问题、提需求都可以")
                    .clicked()
                {
                    ui.ctx().open_url(egui::OpenUrl::same_tab(format!(
                        "{}/issues",
                        crate::update::OFFICIAL_PAGE
                    )));
                    self.hint("已在默认浏览器打开 GitHub 的 issue 页面");
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 顶栏单独跑一帧，收下这一帧画出来的全部文字。
    fn header_labels() -> Vec<String> {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = crate::testbed::stub_app(false);
        let mut out = ctx.run_ui(crate::testbed::base_input(), |ui| app.header(ui));
        // 测试里没有渲染器，字体增量不应用就得显式丢弃，否则 epaint 在 Drop 时 panic
        out.textures_delta.clear();
        out.shapes
            .iter()
            .filter_map(|item| match &item.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                _ => None,
            })
            .collect()
    }

    /// 右上角换了人：「提意见」进来顶替「重新寻找svn.exe」，找 svn 的活归设置页那颗「自动寻找」。
    #[test]
    fn feedback_entry_takes_over_the_svn_rescan_slot() {
        let labels = header_labels();
        assert!(labels.iter().any(|text| text == "提意见"), "顶栏没有「提意见」：{labels:?}");
        assert!(
            !labels.iter().any(|text| text.contains("重新寻找")),
            "顶栏不该再有重新寻找 svn.exe：{labels:?}"
        );
    }
}

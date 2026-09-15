//! 无头跑帧用的测试台：一个不启动任何 svn 任务的 `SvnApp`，外加把真实页面跑一帧、
//! 把画出来的文字与矩形收下来的小驱动器。只有测试用得到，整个模块挂在 cfg(test) 下。

use std::path::PathBuf;
use std::time::Instant;

use egui::{Pos2, RawInput, Rect, ThemePreference, Vec2};

use crate::config::{Config, DirConfig};
use crate::jobs::Pool;
use crate::svn::{Svn, WcInfo};
use crate::{DirView, Page, SvnApp};

/// 只填字段、不起任何 svn 任务的 app。卡片渲染和点击链要在无头里跑，就得先有个能用的实例。
pub fn stub_app(compact: bool) -> SvnApp {
    let info = |url: &str, revision: &str| WcInfo {
        is_wc: true,
        url: url.to_owned(),
        repos_root: url.to_owned(),
        revision: revision.to_owned(),
        node_kind: "dir".to_owned(),
        last_rev: revision.to_owned(),
        last_author: "dev".to_owned(),
        last_date: "2026-09-10 09:12".to_owned(),
    };
    let cfg = Config {
        dirs: vec![
            DirConfig { path: r"C:\wc\hrp".to_owned(), label: "HRP".to_owned(), ..Default::default() },
            DirConfig { path: r"C:\wc\crm".to_owned(), label: "CRM".to_owned(), ..Default::default() },
            DirConfig { path: r"C:\wc\empty".to_owned(), label: String::new(), ..Default::default() },
            DirConfig {
                path: r"C:\wc\long".to_owned(),
                label: "这是一个故意写得很长的目录别名用来验证截断".to_owned(),
                ..Default::default()
            },
        ],
        home_compact: compact,
        ..Default::default()
    };
    let dirs = vec![
        // 服务器上有 5 项没更新、本地有 3 项没提交：两枚标记都该亮
        DirView {
            info: Some(info("https://192.168.1.251/svn/hrp", "120")),
            remote: Some(true),
            remote_rev: "128".to_owned(),
            out_of_date: Some(5),
            changed: Some(3),
            checked_at: "09:30".to_owned(),
            ..Default::default()
        },
        // 没有待更新项：本地版本号跟远端一致，标记都是 0
        DirView {
            info: Some(info("https://192.168.1.251/svn/crm", "198")),
            remote: Some(true),
            remote_rev: "200".to_owned(),
            out_of_date: Some(0),
            changed: Some(0),
            ..Default::default()
        },
        // 没检测到信息：两行版本都该是占位的破折号，而不是 r
        DirView {
            changed: Some(1287),
            ..Default::default()
        },
        // 有冲突：边框走冲突色，同时验证超长别名不会把标记挤走
        DirView {
            info: Some(info("https://192.168.1.251/svn/long", "44")),
            remote: Some(true),
            remote_rev: "44".to_owned(),
            out_of_date: Some(0),
            conflicts: Some(2),
            ..Default::default()
        },
    ];
    SvnApp {
        bc: PathBuf::new(),
        version: None,
        candidates: Vec::new(),
        probing: false,
        refresh_after_detect: false,
        svn_user: "dev".to_owned(),
        user_probed: true,
        pool: Pool::default(),
        output: Vec::new(),
        page: Page::Main,
        commit: None,
        history: None,
        stats: None,
        file_diff: None,
        file_log: None,
        selected: None,
        new_path: String::new(),
        new_label: String::new(),
        edit_label: None,
        label_buf: String::new(),
        label_focus: false,
        edit_filter: None,
        filter_buf: String::new(),
        filter_focus: false,
        relocate: None,
        upload_all: None,
        show_settings: false,
        hint: String::new(),
        confirm_remove: None,
        auto_scroll: true,
        next_refresh: Instant::now(),
        pending_refresh: Vec::new(),
        font_note: String::new(),
        update_info: None,
        update_check_at: None,
        show_update_confirm: false,
        update_error: None,
        ai_picks: Vec::new(),
        ai_pick_dir: 0,
        show_worklog: false,
        ai_mode: false,
        ai_extra: String::new(),
        ai_result: String::new(),
        ai_error: String::new(),
        cfg,
        svn: Svn::default(),
        dirs,
    }
}

/// 无头跑帧用的基础输入：给个够放卡片网格的窗口尺寸，并让窗口始终有焦点
pub fn base_input() -> RawInput {
    RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1400.0, 800.0))),
        focused: true,
        ..Default::default()
    }
}

/// 把主页跑一帧，返回这一帧画出来的「文字 + 矩形」。弹层的开关状态存在内存里，
/// 点下去那一帧还画不出弹层，要多跑一帧稳定的空帧才有。
pub struct Stage {
    pub ctx: egui::Context,
    pub app: SvnApp,
}

impl Stage {
    pub fn new(compact: bool) -> Self {
        let ctx = egui::Context::default();
        // 颜色断言要确定：ink() 在浅色主题下会把强调色压暗，所以固定深色
        ctx.set_theme(ThemePreference::Dark);
        crate::fonts::install_cjk(&ctx);
        Self { ctx, app: stub_app(compact) }
    }

    pub fn step(&mut self, click: Option<Pos2>) -> Vec<(String, Rect)> {
        let mut events = Vec::new();
        if let Some(pos) = click {
            let press = |pressed: bool| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            events.push(egui::Event::PointerMoved(pos));
            events.push(press(true));
            events.push(press(false));
        }
        self.run(events)
    }

    /// 只把指针挪过去、不按：二级菜单靠悬停展开，发点击反而会先把整层菜单收掉
    pub fn hover(&mut self, pos: Pos2) -> Vec<(String, Rect)> {
        self.run(vec![egui::Event::PointerMoved(pos)])
    }

    pub fn run(&mut self, events: Vec<egui::Event>) -> Vec<(String, Rect)> {
        let Stage { ctx, app } = self;
        let mut input = base_input();
        input.events = events;
        let mut out = ctx.run_ui(input, |ui| app.main_page(ui));
        // 测试里没有渲染器，字体增量不应用就得显式丢弃，否则 epaint 在 Drop 时 panic
        out.textures_delta.clear();
        out.shapes
            .iter()
            .filter_map(|item| match &item.shape {
                egui::Shape::Text(text) => {
                    Some((text.galley.text().to_string(), item.shape.visual_bounding_rect()))
                }
                _ => None,
            })
            .filter(|(text, _)| !text.trim().is_empty())
            .collect()
    }

    pub fn rect_of(texts: &[(String, Rect)], label: &str) -> Option<Rect> {
        texts
            .iter()
            .find(|(text, _)| text == label)
            .map(|(_, rect)| *rect)
    }
}

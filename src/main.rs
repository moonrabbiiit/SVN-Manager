#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autostart;
mod bcompare;
mod commit;
mod compare;
mod config;
mod dialogs;
mod fonts;
mod header;
mod history;
mod home;
mod jobs;
mod settings;
mod stats;
mod svn;
mod tasks;
mod ui;
mod update;
mod worklog;
#[cfg(test)]
mod testbed;

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use egui::{Color32, Ui};

use crate::commit::CommitPage;
use crate::config::Config;
use crate::history::{FileDiff, FileLog, HistoryPage};
use crate::jobs::Pool;
use crate::stats::StatsPage;
use crate::svn::{Svn, WcInfo};

// 通用绘制工具搬到 ui.rs，这里转出一层，history / commit / stats 里的 crate::ink 照旧可用
pub use ui::{highlight, highlight_with, ink, Search};

pub const APP_TITLE: &str = "SVN 管理器";
/// 程序版本号，取自 Cargo.toml；发布新版本时只改那里
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Command,
    Success,
    Warning,
    Error,
}

impl Level {
    pub(crate) fn color(self, ui: &Ui) -> Color32 {
        ink(
            ui,
            match self {
                Self::Info => Color32::from_gray(200),
                Self::Command => Color32::from_rgb(120, 190, 240),
                Self::Success => Color32::from_rgb(80, 200, 120),
                Self::Warning => Color32::from_rgb(240, 190, 70),
                Self::Error => Color32::from_rgb(240, 100, 100),
            },
        )
    }
}

/// 「修改仓库地址」窗口状态：把工作副本 URL 里的 `from` 前缀换成 `to` 前缀。
#[derive(Clone)]
pub struct Relocate {
    pub dir: usize,
    /// 原地址前缀，默认填仓库根地址
    pub from: String,
    /// 新地址前缀
    pub to: String,
    pub focus: bool,
    /// 上一次执行的失败原因
    pub error: String,
}

/// 「全部上传」窗口状态：所有目录共用一条说明，各自起一个提交任务。
#[derive(Clone, Default)]
pub struct UploadAll {
    pub message: String,
    /// 第一下只是把按钮变成「确认」，再点一次才真的提交（这一步会改动服务器）
    pub confirm: bool,
    /// 打开时抢一下输入框焦点
    pub focus: bool,
    /// 每个目录是否参与本次批量上传（与 cfg.dirs 对齐，新勾进的默认选中）
    pub checked: Vec<bool>,
    /// 正在查看「待提交明细」的目录（None = 明细窗口关着）
    pub detail: Option<usize>,
}

#[derive(Clone)]
pub struct OutLine {
    pub level: Level,
    pub text: String,
}

/// 目录的运行期状态，与 `cfg.dirs` 一一对应。
#[derive(Clone, Default)]
pub struct DirView {
    pub info: Option<WcInfo>,
    pub remote: Option<bool>,
    pub remote_msg: String,
    pub remote_rev: String,
    /// 服务器上该地址的最近一次提交（来自 `svn info <url>`）
    pub last_rev: String,
    pub last_author: String,
    pub last_date: String,
    /// 待提交条目数（口径同「全部上传」：修改 / 新增 / 删除，以及 ? 与 !）
    pub changed: Option<usize>,
    /// 待提交条目明细，来自最近一次检测或「待提交 X 项」点开的现读
    pub changes: Vec<crate::svn::StatusEntry>,
    /// 服务器上已变、本地还没更新的文件数（`svn status -u`，比本地/远端版本号可靠）
    pub out_of_date: Option<usize>,
    /// 需要人工处理的条目数（冲突 / 不完整）；`None` = 还没读到过，不显示
    pub conflicts: Option<usize>,
    pub checked_at: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Main,
    Commit,
    History,
    /// 个人提交文件数量统计（整页，图表要地方铺）
    Stats,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Maintain {
    Add,
    Delete,
    Cleanup,
    Resolve,
    Revert,
}

impl Maintain {
    pub fn label(self) -> &'static str {
        match self {
            Self::Add => "加入版本控制",
            Self::Delete => "标记删除",
            Self::Cleanup => "清理工作副本",
            Self::Resolve => "解决冲突",
            Self::Revert => "撤销修改",
        }
    }
}

pub struct SvnApp {
    pub cfg: Config,
    pub svn: Svn,
    /// Beyond Compare（BCompare.exe）路径，未找到时为空
    pub bc: PathBuf,
    pub version: Option<String>,
    pub candidates: Vec<String>,
    pub probing: bool,
    pub refresh_after_detect: bool,
    /// 本机 svn 登录人（取自 `svn auth`，取不到时退回 Windows 账户名）
    pub svn_user: String,
    pub user_probed: bool,
    pub dirs: Vec<DirView>,
    pub pool: Pool,
    pub output: Vec<OutLine>,
    pub page: Page,
    pub commit: Option<CommitPage>,
    pub history: Option<HistoryPage>,
    /// 个人提交统计页的状态（None = 没开过；退出页面时清掉，下次进来回到默认条件）
    pub stats: Option<StatsPage>,
    /// 双击历史页「涉及文件」后打开的逐行改动窗口（左右分栏）
    pub file_diff: Option<FileDiff>,
    /// 点「查看提交记录」后打开的单个文件提交记录窗口
    pub file_log: Option<FileLog>,
    pub selected: Option<usize>,
    pub new_path: String,
    pub new_label: String,
    /// 正在修改别名的目录行
    pub edit_label: Option<usize>,
    pub label_buf: String,
    pub label_focus: bool,
    /// 正在修改 BC 名称筛选的目录行（编辑器放在目录行里，
    /// 不能塞进「更多」菜单——menu_button 里点输入框本身就会把菜单关掉）
    pub edit_filter: Option<usize>,
    pub filter_buf: String,
    pub filter_focus: bool,
    /// 正在修改仓库地址的目录（None = 窗口关闭）
    pub relocate: Option<Relocate>,
    /// 正在批量上传的目录清单（None = 窗口关闭）
    pub upload_all: Option<UploadAll>,
    pub show_settings: bool,
    pub hint: String,
    pub confirm_remove: Option<usize>,
    pub auto_scroll: bool,
    pub next_refresh: Instant,
    /// 请求过但当时同目录已有一次检测在跑，等那次落地之后补跑一次
    pub pending_refresh: Vec<(usize, bool)>,
    pub font_note: String,
    /// 最新版本信息（官方 GitHub 发布或自建服务端的 latest.json，检查过更新才有）
    pub update_info: Option<update::UpdateManifest>,
    /// 更新源上确实有可装的新东西：按文件 sha256 判定（见 `update::has_update`，
    /// 同一个版本号重新发布的构建也算），检查那一趟的后台线程算好后放这里，
    /// 界面每帧只读它，不重算哈希
    pub update_ready: bool,
    /// 启动后第一次检查更新的时刻（None = 已触发过）
    pub update_check_at: Option<Instant>,
    /// 开了「自动检查更新」后的下一次检查时刻（每 5 分钟一轮）
    pub next_update_check: Instant,
    /// 「发现新版本」确认对话框开关（header 与设置里的更新入口都走它）
    pub show_update_confirm: bool,
    /// 最近一次下载新版本失败的原因（None = 没在失败状态）；对话框里内联显示
    pub update_error: Option<String>,
    /// AI 工作日志：从各目录历史页勾选的本人提交（快照，跨页面保留）
    pub ai_picks: Vec<worklog::AiPick>,
    /// AI 日志窗口里「补充其他目录」下拉框当前选中的目录编号
    pub ai_pick_dir: usize,
    /// AI 工作日志窗口开关
    pub show_worklog: bool,
    /// 日志模式开关：开启后各目录「提交记录」页里本人提交的版本行才出现勾选框
    ///（右上角「AI 日志」按钮以开关形式控制，见 header）
    pub ai_mode: bool,
    /// AI 工作日志：额外提示词（只在当前会话保留，不写配置）
    pub ai_extra: String,
    /// AI 工作日志：最近一次生成的日志正文（可直接编辑）
    pub ai_result: String,
    /// AI 工作日志：最近一次生成失败的原因
    pub ai_error: String,
}

/// 取 icon/svn_manager.ico 里 48px 那张当窗口图标（标题栏 + 任务栏）。
/// 直接读嵌进 exe 的同一个 ico，图标就只有 rabbit.png -> svn_manager.ico 这一条来源；
/// ico 里的图是 32bpp 未压缩位图，像素自下而上存、字节序 BGRA，所以要翻转并换成 RGBA。
/// 解析不出来就返回 None，让 eframe 用它的默认图标，不影响启动。
fn window_icon() -> Option<egui::IconData> {
    const EDGE: usize = 48;
    let ico = include_bytes!("../icon/svn_manager.ico");
    let count = u16::from_le_bytes(ico.get(4..6)?.try_into().ok()?) as usize;
    for index in 0..count {
        let entry = ico.get(6 + index * 16..22 + index * 16)?;
        if entry[0] as usize != EDGE {
            continue;
        }
        let offset = u32::from_le_bytes(entry[12..16].try_into().ok()?) as usize;
        let header = ico.get(offset..offset + 20)?;
        if u32::from_le_bytes(header[0..4].try_into().ok()?) != 40
            || u32::from_le_bytes(header[8..12].try_into().ok()?) != (EDGE * 2) as u32
            || u16::from_le_bytes(header[14..16].try_into().ok()?) != 32
            || u32::from_le_bytes(header[16..20].try_into().ok()?) != 0
        {
            continue;
        }
        let pixels = ico.get(offset + 40..offset + 40 + EDGE * EDGE * 4)?;
        let mut rgba = vec![0u8; EDGE * EDGE * 4];
        for row in 0..EDGE {
            for col in 0..EDGE {
                let from = (EDGE - 1 - row) * EDGE * 4 + col * 4;
                let to = (row * EDGE + col) * 4;
                rgba[to] = pixels[from + 2];
                rgba[to + 1] = pixels[from + 1];
                rgba[to + 2] = pixels[from];
                rgba[to + 3] = pixels[from + 3];
            }
        }
        return Some(egui::IconData {
            rgba,
            width: EDGE as u32,
            height: EDGE as u32,
        });
    }
    None
}

fn main() -> eframe::Result {
    // 界面程序没有控制台，异常信息写入配置文件同目录的 panic.log 便于排查
    let panic_log = config::config_file().with_file_name("panic.log");
    std::panic::set_hook(Box::new(move |info| {
        let text = format!(
            "[{}] {info}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&panic_log)
        {
            let _ = file.write_all(text.as_bytes());
        }
    }));
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1220.0, 820.0])
        .with_min_inner_size([900.0, 580.0])
        .with_title(APP_TITLE);
    // 不显式设置的话，标题栏和任务栏用的还是 eframe 自带的默认图标
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Glow,
        centered: true,
        ..Default::default()
    };
    eframe::run_native(APP_TITLE, options, Box::new(|cc| Ok(Box::new(SvnApp::new(cc)))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_icon_is_a_non_blank_rgba_48() {
        let icon = window_icon().expect("icon/svn_manager.ico 里应能解析出 48px 的图");
        assert_eq!((icon.width, icon.height), (48, 48));
        assert_eq!(icon.rgba.len(), 48 * 48 * 4);
        // 四角是透明的，中心是兔子身上的白色；行序翻转错了这两处就会反过来
        assert_eq!(&icon.rgba[0..4], &[0, 0, 0, 0]);
        let center = (24 * 48 + 24) * 4;
        assert_eq!(&icon.rgba[center..center + 4], &[255, 255, 255, 255]);
    }
}

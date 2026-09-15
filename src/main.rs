#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod bcompare;
mod commit;
mod config;
mod fonts;
mod history;
mod jobs;
mod stats;
mod svn;
mod update;
mod worklog;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::NaiveDate;
use egui::text::LayoutJob;
use egui::{Align, Color32, FontId, Frame, Layout, RichText, ScrollArea, TextEdit, TextFormat, Ui, Vec2};
use egui::Key;

use crate::commit::CommitPage;
use crate::config::{Config, DirConfig};
use crate::history::{FileDiff, FileLog, HistoryPage, Zoom};
use crate::jobs::{Data, Kind, Pool};
use crate::stats::StatsPage;
use crate::svn::{LogEntry, LogPath, Svn, WcInfo};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
use crate::svn::CREATE_NO_WINDOW;

pub const APP_TITLE: &str = "SVN 管理器";
/// 程序版本号，取自 Cargo.toml；发布新版本时只改那里
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Command,
    Success,
    Warning,
    Error,
}

impl Level {
    fn color(self, ui: &Ui) -> Color32 {
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

/// 强调色都是按深色背景调的，浅色主题下压暗一点，保证白底上也能看清。
pub fn ink(ui: &Ui, color: Color32) -> Color32 {
    if ui.visuals().dark_mode {
        color
    } else {
        // 只压暗 RGB，不能动 alpha（gamma_multiply 会连 alpha 一起乘，字会变成半透明）
        let [r, g, b, a] = color.to_array();
        Color32::from_rgba_unmultiplied(
            (r as f32 * 0.55 + 0.5) as u8,
            (g as f32 * 0.55 + 0.5) as u8,
            (b as f32 * 0.55 + 0.5) as u8,
            a,
        )
    }
}

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

/// 搜索高亮：把命中的 `needle` 片段用醒目底色标出来，其余文字保持原样。
/// 忽略 ASCII 大小写；按字节匹配是安全的——UTF-8 的后续字节都落在 0x80..=0xBF，
/// 可用作首字节的一定 >= 0xC2，所以命中位置必然在字符边界上，不会切坏中文。
pub fn highlight(ui: &Ui, text: &str, needle: &str, font: FontId, color: Color32) -> LayoutJob {
    let plain = TextFormat::simple(font, color);
    let mut job = LayoutJob::default();
    let key = needle.trim().as_bytes();
    if key.is_empty() {
        job.append(text, 0.0, plain);
        return job;
    }
    let dark = ui.visuals().dark_mode;
    let mark = TextFormat {
        color: if dark {
            Color32::from_rgb(255, 216, 100)
        } else {
            Color32::from_rgb(64, 44, 0)
        },
        background: if dark {
            Color32::from_rgb(96, 76, 18)
        } else {
            Color32::from_rgb(255, 234, 140)
        },
        ..plain.clone()
    };
    let bytes = text.as_bytes();
    let mut cursor = 0usize;
    while cursor + key.len() <= bytes.len() {
        let found = bytes[cursor..].windows(key.len()).position(|window| {
            window
                .iter()
                .zip(key)
                .all(|(left, right)| left.eq_ignore_ascii_case(right))
        });
        let Some(offset) = found else { break };
        let start = cursor + offset;
        job.append(&text[cursor..start], 0.0, plain.clone());
        job.append(&text[start..start + key.len()], 0.0, mark.clone());
        cursor = start + key.len();
    }
    if cursor < bytes.len() {
        job.append(&text[cursor..], 0.0, plain);
    }
    job
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
    /// 启动后第一次检查更新的时刻（None = 已触发过）
    pub update_check_at: Option<Instant>,
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

impl SvnApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let font_note = match fonts::install_cjk(&cc.egui_ctx) {
            Some(path) => format!(
                "中文字体：{}",
                Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or(path)
            ),
            None => "警告：未找到系统中文字体，中文可能显示为方块".to_owned(),
        };
        let cfg = Config::load();
        for style_theme in [egui::Theme::Dark, egui::Theme::Light] {
            cc.egui_ctx.style_mut_of(style_theme, |style| {
                style.spacing.item_spacing = Vec2::new(7.0, 5.0);
                style.spacing.button_padding = Vec2::new(8.0, 3.0);
            });
        }
        // 直接 with_maximized 会让首帧画在旧尺寸上且不重绘，改为第一帧后再最大化
        cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
        let version = {
            let svn = Svn {
                exe: PathBuf::from(&cfg.svn_exe),
                ..Default::default()
            };
            svn.version()
        };
        let svn = Svn {
            exe: PathBuf::from(&cfg.svn_exe),
            username: cfg.auth_user.clone(),
            password: cfg.auth_pass.clone(),
        };
        let saved_bc = PathBuf::from(&cfg.bc_exe);
        let bc = if saved_bc.is_file() {
            saved_bc
        } else {
            bcompare::detect().unwrap_or_default()
        };
        let mut app = Self {
            dirs: vec![DirView::default(); cfg.dirs.len()],
            probing: false,
            refresh_after_detect: true,
            version,
            candidates: Vec::new(),
            cfg,
            svn,
            bc,
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
            svn_user: String::new(),
            user_probed: false,
            show_settings: false,
            hint: String::new(),
            confirm_remove: None,
            auto_scroll: true,
            next_refresh: Instant::now() + Duration::from_secs(4),
            pending_refresh: Vec::new(),
            font_note,
            update_info: None,
            // 启动 3 秒后开始检查更新：避开启动瞬间的 svn 探测高峰
            update_check_at: Some(Instant::now() + Duration::from_secs(3)),
            show_update_confirm: false,
            update_error: None,
            ai_picks: Vec::new(),
            ai_pick_dir: 0,
            show_worklog: false,
            ai_mode: false,
            ai_extra: String::new(),
            ai_result: String::new(),
            ai_error: String::new(),
        };
        app.push(Level::Info, format!("{APP_TITLE} 已启动（配置：{}）", config::config_file().display()));
        if app.svn.available() {
            app.push(
                Level::Success,
                format!(
                    "svn.exe = {}（版本 {}）",
                    app.svn.exe.display(),
                    app.version.clone().unwrap_or_default()
                ),
            );
            app.spawn_all_refresh();
            app.probing = false;
        } else {
            app.spawn_detect();
        }
        app
    }

    // ------------------------------------------------------------ 小工具

    pub fn push(&mut self, level: Level, text: impl Into<String>) {
        for line in text.into().lines() {
            if !line.trim().is_empty() {
                self.output.push(OutLine {
                    level,
                    text: line.to_owned(),
                });
            }
        }
        if self.output.len() > 3000 {
            let cut = self.output.len() - 3000;
            self.output.drain(..cut);
        }
    }

    pub fn hint(&mut self, text: impl Into<String>) {
        self.hint = text.into();
    }

    pub fn persist(&mut self) {
        if let Err(e) = self.cfg.save() {
            self.hint(format!("保存配置失败：{e}"));
        }
    }

    pub fn dir_path(&self, index: usize) -> Option<PathBuf> {
        self.cfg.dirs.get(index).map(|d| PathBuf::from(&d.path))
    }

    pub fn dir_label(&self, index: usize) -> String {
        match self.cfg.dirs.get(index) {
            Some(dir) if !dir.label.trim().is_empty() => dir.label.trim().to_owned(),
            Some(dir) => Path::new(&dir.path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| dir.path.clone()),
            None => format!("#{index}"),
        }
    }

    fn sync_views(&mut self) {
        self.dirs.resize(self.cfg.dirs.len(), DirView::default());
        self.dirs.truncate(self.cfg.dirs.len());
        if self.selected.is_some_and(|i| i >= self.cfg.dirs.len()) {
            self.selected = None;
        }
    }

    pub fn apply_svn_exe(&mut self, exe: &str) {
        self.cfg.svn_exe = exe.trim().trim_matches('"').to_owned();
        self.svn.exe = PathBuf::from(&self.cfg.svn_exe);
        self.version = self.svn.version();
        self.svn_user.clear();
        self.user_probed = false;
        self.persist();
    }

    pub fn svn_or_none(&mut self) -> Option<Svn> {
        if self.svn.available() {
            Some(self.svn.clone())
        } else {
            self.hint("未找到可用的 svn.exe，请在「设置」中指定路径或重新自动寻找");
            self.show_settings = true;
            None
        }
    }

    // ------------------------------------------------------------ 后台任务

    pub fn spawn_detect(&mut self) {
        if self.pool.has(Kind::DetectSvn, usize::MAX) {
            return;
        }
        self.probing = true;
        self.refresh_after_detect = true;
        self.hint("正在自动寻找 svn.exe …");
        self.pool
            .spawn(Kind::DetectSvn, usize::MAX, "寻找 svn.exe".into(), move |sink| {
                sink.line("$ 自动寻找 svn.exe（环境变量 / PATH / 注册表 / 常见安装目录 / 磁盘浅层扫描）");
                let started = Instant::now();
                let found = svn::candidates();
                sink.line(format!(
                    "→ {} 个候选路径（扫描用时 {:.1} 秒）",
                    found.len(),
                    started.elapsed().as_secs_f32()
                ));
                let list: Vec<String> = found
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                match svn::detect(&found) {
                    Some((exe, version)) => Data::Svn {
                        exe: exe.to_string_lossy().into_owned(),
                        version,
                        candidates: list,
                    },
                    None => Data::Svn {
                        exe: String::new(),
                        version: String::new(),
                        candidates: list,
                    },
                }
            });
    }

    /// 后台检查一次版本更新（官方 GitHub 发布或自建服务端 latest.json，与目录无关的任务）。
    /// 地址有改动时会顺带保存配置。
    pub fn spawn_update_check(&mut self) {
        let official = update::is_official(&self.cfg.update_source);
        if !official && self.cfg.update_server.trim().is_empty() {
            self.hint("未配置更新服务端地址，请在「设置 → 版本更新」里填写");
            return;
        }
        self.persist();
        if self.pool.has(Kind::CheckUpdate, usize::MAX) {
            return;
        }
        let source = self.cfg.update_source.clone();
        let server = self.cfg.update_server.clone();
        let from = if official {
            update::OFFICIAL_REPO.to_owned()
        } else {
            server.clone()
        };
        self.pool
            .spawn(Kind::CheckUpdate, usize::MAX, "检查更新".into(), move |sink| {
                sink.line(format!("$ 检查版本更新：{from}"));
                match update::check_from(&source, &server) {
                    Ok(manifest) => {
                        sink.line(format!("→ 最新版本：V{}", manifest.version.trim()));
                        Data::UpdateCheck {
                            ok: true,
                            message: String::new(),
                            info: Some(manifest),
                        }
                    }
                    Err(e) => Data::UpdateCheck {
                        ok: false,
                        message: e,
                        info: None,
                    },
                }
            });
    }

    /// 确认对话框里点了「开始更新」：后台下载新版本并校验，结果回来后再覆盖。
    pub fn begin_update(&mut self) {
        let Some(manifest) = self.update_info.clone() else {
            self.hint("还没有检查到新版本，请先「检查更新」");
            return;
        };
        if self.pool.has(Kind::DownloadUpdate, usize::MAX) {
            return;
        }
        let url = manifest.url.clone();
        let sha = manifest.sha256.trim().to_lowercase();
        self.pool.spawn(
            Kind::DownloadUpdate,
            usize::MAX,
            "下载更新".into(),
            move |sink| {
                sink.line(format!("$ 正在下载新版本：{url}"));
                let dest = std::env::temp_dir().join("svn_manager_update.exe");
                let _ = std::fs::remove_file(&dest);
                match update::download(&url, &dest) {
                    Ok(bytes) => {
                        sink.line(format!("→ 下载完成（{:.1} MB）", bytes as f64 / 1_048_576.0));
                        // 更新源给了校验值就核对（自建服务端的 sha256 字段、GitHub 资产的 digest），不一致直接删掉重来
                        if !sha.is_empty() {
                            match update::sha256_of(&dest) {
                                Ok(hash) if hash == sha => {
                                    sink.line("→ SHA256 校验通过");
                                }
                                Ok(hash) => {
                                    let _ = std::fs::remove_file(&dest);
                                    return Data::UpdateDownloaded {
                                        ok: false,
                                        message: format!(
                                            "SHA256 校验不符（期望 {sha}，实际 {hash}）"
                                        ),
                                        bytes,
                                    };
                                }
                                Err(e) => {
                                    let _ = std::fs::remove_file(&dest);
                                    return Data::UpdateDownloaded {
                                        ok: false,
                                        message: e,
                                        bytes,
                                    };
                                }
                            }
                        }
                        // 防御无限更新循环：更新源版本号升了但 exe 没换（或换回了同一个文件）
                        // 时，下载结果与当前程序完全相同，覆盖只会让程序反复重启「更新」
                        if let Ok(self_exe) = std::env::current_exe() {
                            match (update::sha256_of(&dest), update::sha256_of(&self_exe)) {
                                (Ok(new_hash), Ok(cur_hash)) if new_hash == cur_hash => {
                                    let _ = std::fs::remove_file(&dest);
                                    sink.line("→ 下载的文件与当前程序完全相同");
                                    return Data::UpdateDownloaded {
                                        ok: false,
                                        message: "下载的文件与当前程序完全相同：那次发布传的 exe 可能没有换成新构建，已取消覆盖".to_owned(),
                                        bytes,
                                    };
                                }
                                _ => {}
                            }
                        }
                        Data::UpdateDownloaded {
                            ok: true,
                            message: String::new(),
                            bytes,
                        }
                    }
                    Err(e) => Data::UpdateDownloaded {
                        ok: false,
                        message: e,
                        bytes: 0,
                    },
                }
            },
        );
    }

    /// 下载校验完成后：生成收尾 bat（覆盖+重启+自删）并启动，随后退出本程序。
    fn launch_apply_bat(&mut self) {
        let exe = match std::env::current_exe() {
            Ok(path) => path,
            Err(e) => {
                self.hint(format!("无法确定程序自身路径：{e}"));
                self.push(Level::Error, format!("无法确定程序自身路径：{e}"));
                return;
            }
        };
        let bat_text = update::build_apply_bat(&exe);
        // bat 默认写到程序自身目录：杀软对 %TEMP% 里的 .bat 扫描最凶，
        // 脚本跑到一半被查删就会报「找不到批处理文件」。
        // 程序目录不可写（如 Program Files）时回退到 %TEMP%。
        let mut bat: Option<std::path::PathBuf> = None;
        let mut candidates: Vec<std::path::PathBuf> = Vec::new();
        if let Some(dir) = exe.parent() {
            if !dir.as_os_str().is_empty() {
                candidates.push(dir.join("svn_manager_apply.bat"));
            }
        }
        candidates.push(std::env::temp_dir().join("svn_manager_apply.bat"));
        for cand in &candidates {
            if std::fs::write(cand, bat_text.as_bytes()).is_ok() {
                bat = Some(cand.clone());
                break;
            }
        }
        let bat = match bat {
            Some(b) => b,
            None => {
                self.hint("生成更新脚本失败：程序目录与临时目录都不可写".to_owned());
                self.push(Level::Error, "生成更新脚本失败：程序目录与临时目录都不可写".to_owned());
                return;
            }
        };
        self.push(
            Level::Info,
            format!("更新脚本已生成：{}（覆盖 {} 并重启）", bat.display(), exe.display()),
        );
        // 启动器本身用 CREATE_NO_WINDOW：不闪 cmd 黑框；
        // `start` 会给 bat 另起一个控制台窗口，覆盖失败时的 pause 仍看得见。
        let mut launcher = std::process::Command::new("cmd");
        #[cfg(windows)]
        launcher.creation_flags(CREATE_NO_WINDOW);
        match launcher
            .args(["/C", "start", "", &bat.to_string_lossy()])
            .spawn()
        {
            Ok(_) => {
                self.push(Level::Success, "程序即将退出，由更新脚本完成覆盖并自动重启…");
                // 下载的临时 exe 只在本进程内持有路径，退出后 bat 直接接管
                std::process::exit(0);
            }
            Err(e) => {
                self.hint(format!("启动更新脚本失败：{e}"));
                self.push(Level::Error, format!("启动更新脚本失败：{e}"));
            }
        }
    }

    /// 读一次本机 svn 登录人（`svn auth`），提交记录默认用它只看自己的提交。
    pub fn spawn_auth_user(&mut self) {
        let svn = self.svn.clone();
        let url = self
            .dirs
            .iter()
            .find_map(|view| view.info.clone())
            .map(|info| info.repos_root)
            .unwrap_or_default();
        // 等第一次检测出结果再查：有仓库地址才能挑出这台服务器的账号，
        // 否则会拿到 svn auth 里第一个凭据（可能是另一台服务器的另一个账号）
        let checked = self.dirs.iter().any(|view| view.remote.is_some());
        if self.user_probed
            || !self.svn.available()
            || self.pool.has(Kind::AuthUser, usize::MAX)
            || (url.is_empty() && !checked)
        {
            return;
        }
        self.user_probed = true;
        self.pool
            .spawn(Kind::AuthUser, usize::MAX, "本机 svn 登录人".to_owned(), move |sink| {
                sink.line("$ svn auth");
                let user = svn.auth_user(&url).unwrap_or_else(|| {
                    std::env::var("USERNAME")
                        .or_else(|_| std::env::var("USER"))
                        .unwrap_or_default()
                });
                sink.line(format!("→ 本机 svn 登录人：{user}"));
                Data::User { user }
            });
    }

    /// `quiet` 为真时不在输出区打印执行日志（提交成功后的自动刷新用：那是程序
    /// 自己补的读操作，逐行 `$ svn …` 只会淹没提交结果；失败仍走结果消息报出来）。
    pub fn spawn_refresh(&mut self, index: usize, quiet: bool) {
        let Some(svn) = self.svn_or_none() else { return };
        let Some(path) = self.dir_path(index) else { return };
        if self.pool.has(Kind::Refresh, index) {
            // 直接返回的话，正在跑的那一次读的是提交前的旧数据，
            // 本地版本号就会一直停在旧值上，所以记一笔，等它落地后补跑
            if !self
                .pending_refresh
                .iter()
                .any(|(item, _)| *item == index)
            {
                self.pending_refresh.push((index, quiet));
            }
            return;
        }
        if !path.is_dir() {
            if let Some(view) = self.dirs.get_mut(index) {
                view.info = None;
                view.remote = Some(false);
                view.remote_msg = "目录不存在（可能已被移动或删除）".to_owned();
            }
            return;
        }
        let label = self.dir_label(index);
        self.pool
            .spawn(Kind::Refresh, index, format!("{label} 检测"), move |sink| {
                let sink = if quiet { sink.muted() } else { sink };
                let target = path.to_string_lossy().into_owned();
                let (info, run) = svn.info(&target);
                let Some(info) = info else {
                    let message = run.summary();
                    sink.line(format!("[{label}] 不是 SVN 工作副本：{message}"));
                    return Data::Wc {
                        dir: index,
                        info: None,
                        remote: Some(false),
                        remote_msg: "不是 SVN 工作副本".to_owned(),
                        remote_rev: String::new(),
                        last_rev: String::new(),
                        last_author: String::new(),
                        last_date: String::new(),
                        changed: None,
                        changes: Vec::new(),
                        conflicts: None,
                        out_of_date: None,
                    };
                };
                let mut remote = Some(true);
                let mut remote_msg = "已连接到仓库".to_owned();
                let mut remote_rev = String::new();
                let mut last_rev = String::new();
                let mut last_author = String::new();
                let mut last_date = String::new();
                if info.url.is_empty() {
                    remote = Some(false);
                    remote_msg = "无法取得仓库 URL".to_owned();
                } else {
                    sink.line(format!("$ svn info --xml {}", info.url));
                    let (head, probe) = svn.info(&info.url);
                    if probe.ok {
                        if let Some(head) = head {
                            remote_rev = head.revision;
                            last_rev = head.last_rev;
                            last_author = head.last_author;
                            last_date = head.last_date;
                        }
                    } else {
                        remote = Some(false);
                        remote_msg = probe.summary();
                    }
                }
                let (entries, status) = svn.status(&path);
                // 统计口径与「全部上传」一致：? 会在提交时自动 add、! 自动 delete，
                // 所以它们也算待提交项，用户看到的数量和真正提交上去的数量才对得上
                let changes: Vec<crate::svn::StatusEntry> = entries
                    .iter()
                    .filter(|entry| entry.item.uploadable())
                    .cloned()
                    .collect();
                let changed = status.ok.then(|| changes.len());
                // 冲突数在未过滤的 entries 上数：changes 里已经没有冲突条目了。
                // status 读失败只能给 None——给 0 会把「没读到」显示成「没有冲突」
                let conflicts = status.ok.then(|| crate::svn::blocked_count(&entries));
                // 「可更新」以服务器为准：提交只把被提交路径的版本推进，工作副本根目录
                // 还停在旧版本上，光比本地 r 和远端 HEAD 会把自己刚提交完的目录误报成可更新
                let out_of_date = if remote == Some(true) {
                    svn.out_of_date(&path)
                } else {
                    None
                };
                if let Some(pending) = out_of_date.filter(|count| *count > 0) {
                    sink.line(format!("[{label}] 服务器上有 {pending} 项本地还没更新"));
                }
                if remote != Some(true) {
                    sink.line(format!("[{label}] 连接异常：{remote_msg}"));
                }
                Data::Wc {
                    dir: index,
                    info: Some(info),
                    remote,
                    remote_msg,
                    remote_rev,
                    last_rev,
                    last_author,
                    last_date,
                    changed,
                    changes,
                    conflicts,
                    out_of_date,
                }
            });
    }

    pub fn spawn_all_refresh(&mut self) {
        for index in 0..self.cfg.dirs.len() {
            self.spawn_refresh(index, false);
        }
    }

    pub fn spawn_update(&mut self, index: usize) {
        let (Some(svn), Some(path)) = (self.svn_or_none(), self.dir_path(index)) else {
            return;
        };
        let label = self.dir_label(index);
        self.pool
            .spawn(Kind::Update, index, format!("{label} 更新"), move |sink| {
                sink.line(format!("$ svn update \"{}\"", path.display()));
                let run = svn.update(&path, &|line| sink.line(line));
                let message = if run.ok {
                    format!("{label}：更新完成")
                } else {
                    format!("{label}：更新失败——{}", run.summary())
                };
                // 收尾文案只交给任务回收端统一写进输出区，避免同一句显示两遍
                Data::Run {
                    dir: index,
                    ok: run.ok,
                    message,
                    reload: true,
                }
            });
    }

    pub fn spawn_status(&mut self, index: usize, quiet: bool) {
        let (Some(svn), Some(path)) = (self.svn_or_none(), self.dir_path(index)) else {
            return;
        };
        if self.pool.has(Kind::Status, index) {
            return;
        }
        let label = self.dir_label(index);
        self.pool
            .spawn(Kind::Status, index, format!("{label} 读取修改"), move |sink| {
                // 静音时丢弃执行日志，读失败仍通过 Data::Status 的 message 报给界面
                let sink = if quiet { sink.muted() } else { sink };
                sink.line(format!("$ svn status --xml \"{}\"", path.display()));
                let (entries, run) = svn.status(&path);
                sink.line(format!("→ 共 {} 项改动", entries.len()));
                Data::Status {
                    dir: index,
                    entries,
                    ok: run.ok,
                    message: if run.ok { String::new() } else { run.summary() },
                }
            });
    }

    /// 读一个目录的提交记录。`mine` 为真时只取本机 svn 登录人的记录（`svn log --search`）；
    /// `unlimited` 为真时临时不带 `-l`（不限条数拉全量），只影响这一次读取，不写设置。
    /// `range` 为 `Some` 时按日期区间读服务器（`svn log -r {止}:{起}`，新 → 旧）：
    /// 区间模式下条数没有意义（要的是那几天，不是最近 N 条），所以不带 `-l`；
    /// 目标串还要显式 peg 到 `@HEAD`：工作副本的隐式 peg 是 BASE，本地落后时按日期找版本会取不到新提交。
    pub fn spawn_log_in(
        &mut self,
        index: usize,
        mine: bool,
        unlimited: bool,
        range: Option<(NaiveDate, NaiveDate)>,
    ) {
        let (Some(svn), Some(path)) = (self.svn_or_none(), self.dir_path(index)) else {
            return;
        };
        if self.pool.has(Kind::Log, index) {
            return;
        }
        // limit 传 0 表示不限条数（svn.log_in 里据此省掉 -l）；按区间读时固定不限
        let limit = match range {
            Some(_) => 0,
            None if unlimited => 0,
            None => self.cfg.log_limit.clamp(1, 2000),
        };
        let spec = stats::revspec(range);
        let author = if mine { self.svn_user.clone() } else { String::new() };
        let label = self.dir_label(index);
        self.pool
            .spawn(Kind::Log, index, format!("{label} 提交记录"), move |sink| {
                let shown = if author.is_empty() {
                    String::new()
                } else {
                    format!(" --search {author}")
                };
                let limit_part = match range {
                    Some(_) => String::new(),
                    None if limit > 0 => format!(" -l {limit}"),
                    None => "（不限条数）".to_owned(),
                };
                let target = match range {
                    Some(_) => format!("{}@HEAD", path.to_string_lossy()),
                    None => path.to_string_lossy().into_owned(),
                };
                sink.line(format!(
                    "$ svn log -v -r {spec}{limit_part}{shown} --xml \"{target}\""
                ));
                let (entries, run) = svn.log_in(&target, &spec, limit, &author);
                sink.line(format!("→ 共 {} 条提交记录", entries.len()));
                Data::Log {
                    dir: index,
                    entries,
                    ok: run.ok,
                    message: if run.ok { String::new() } else { run.summary() },
                }
            });
    }

    /// 双击历史页的「涉及文件」：看这一次提交对这个文件 / 目录改动了哪几行（和上一个版本比）。
    /// `path.path` 是 svn log 返回的路径，相对仓库根（如 `/code/HRP.FA/...`），拼上 repos_root 就能查。
    pub fn open_file_diff(&mut self, index: usize, entry: &LogEntry, path: &LogPath) {
        // 一次只读一条：上一条还没回来就换路径，结果会串到新的路径上
        if self.pool.has(Kind::FileDiff, index) {
            self.hint("上一条差异还在读取，稍等再双击");
            return;
        }
        let root = self
            .dirs
            .get(index)
            .and_then(|view| view.info.clone())
            .map(|info| info.repos_root)
            .unwrap_or_default();
        let root = root.trim_end_matches('/');
        if root.is_empty() {
            self.hint("还没读到该目录的仓库根地址，无法查看逐行改动");
            return;
        }
        let mut url = root.to_owned();
        if !path.path.starts_with('/') {
            url.push('/');
        }
        url.push_str(&path.path);
        let revision = entry.revision.clone();
        // XML 整份被改缩进 / 换行符的情况最多，这类文件默认忽略空白；其余文件默认看原始差异
        let ignore_white = path.path.to_ascii_lowercase().ends_with(".xml");
        self.hint(format!("正在读取 r{revision} 对 {} 的逐行改动 …", path.path));
        self.file_diff = Some(FileDiff {
            dir: index,
            revision: revision.clone(),
            author: entry.author.clone(),
            date: entry.date.clone(),
            message: entry.message.clone(),
            path: path.path.clone(),
            action: path.action,
            url: url.clone(),
            diff: String::new(),
            error: String::new(),
            ignore_white,
            // 改动明细默认最大化：看差异要的是横向空间，点「还原」才回到原来的大小
            zoom: Zoom::maximized(),
        });
        self.spawn_file_diff(index, revision, url, ignore_white);
    }

    /// `svn diff -c <版本> <仓库URL>`：这一次提交相对上一个版本，对该路径改了哪几行。
    /// 用仓库 URL 查，本地没有该文件（或已被改名 / 删除）也能取到；本次新增的文件会整份显示为 +。
    pub fn spawn_file_diff(&mut self, index: usize, revision: String, url: String, ignore_white: bool) {
        let Some(svn) = self.svn_or_none() else {
            self.hint("没有可用的 svn.exe，无法读取差异");
            return;
        };
        if self.pool.has(Kind::FileDiff, index) {
            self.hint("上一条差异还在读取，读完再点「重新读取」");
            return;
        }
        let shown = url.rsplit('/').next().unwrap_or("").to_owned();
        self.pool.spawn(
            Kind::FileDiff,
            index,
            format!("{shown} r{revision} 差异"),
            move |sink| {
                let flags = crate::svn::diff_flags(ignore_white);
                sink.line(format!("$ svn diff -c {revision} --internal-diff{flags} \"{url}\""));
                let run = svn.diff_rev(&revision, &url, ignore_white);
                sink.line(format!(
                    "→ {}",
                    if run.ok { "已取到差异".to_owned() } else { run.summary() }
                ));
                Data::Run {
                    dir: index,
                    ok: run.ok,
                    message: if run.ok { run.out } else { run.summary() },
                    reload: false,
                }
            },
        );
    }
    /// 点涉及文件行右侧的「查看提交记录」：查这个文件 / 目录自己在服务器上的提交记录。
    /// 地址 = 仓库根 + log 返回的相对路径（相对仓库根，如 `/code/HRP.FA/...`）。
    pub fn open_file_log(&mut self, index: usize, path: &LogPath) {
        // 一次只读一条：上一条没回来就换路径，结果会串到别的路径上
        if self.pool.has(Kind::FileLog, index) {
            self.hint("上一个文件的提交记录还在读取，稍等再点");
            return;
        }
        let root = self
            .dirs
            .get(index)
            .and_then(|view| view.info.clone())
            .map(|info| info.repos_root)
            .unwrap_or_default();
        let root = root.trim_end_matches('/');
        if root.is_empty() {
            self.hint("还没读到该目录的仓库根地址，无法查询单个文件的提交记录");
            return;
        }
        let mut url = root.to_owned();
        if !path.path.starts_with('/') {
            url.push('/');
        }
        url.push_str(&path.path);
        let limit = self.cfg.log_limit.clamp(1, 2000);
        self.hint(format!("正在读取 {} 的提交记录 …", path.path));
        self.file_log = Some(FileLog {
            dir: index,
            url: url.clone(),
            name: path.path.clone(),
            limit,
            entries: Vec::new(),
            error: String::new(),
            zoom: Zoom::default(),
        });
        self.spawn_file_log(index, url, limit);
    }

    /// `svn log <仓库URL>`：只返回改动过这个路径的那些版本（同样一律读服务器）。
    pub fn spawn_file_log(&mut self, index: usize, url: String, limit: i64) {
        let Some(svn) = self.svn_or_none() else {
            self.hint("没有可用的 svn.exe，无法读取文件提交记录");
            return;
        };
        if self.pool.has(Kind::FileLog, index) {
            self.hint("上一个文件的提交记录还在读取，稍等再点");
            return;
        }
        let shown = url.rsplit('/').next().unwrap_or("").to_owned();
        self.pool.spawn(
            Kind::FileLog,
            index,
            format!("{shown} 提交记录"),
            move |sink| {
                sink.line(format!("$ svn log -v -r HEAD:1 -l {limit} --xml \"{url}\""));
                let (entries, run) = svn.log(&url, limit, "");
                sink.line(format!("→ 共 {} 条记录", entries.len()));
                Data::Log {
                    dir: index,
                    entries,
                    ok: run.ok,
                    message: if run.ok { String::new() } else { run.summary() },
                }
            },
        );
    }
    pub fn spawn_diff(&mut self, index: usize, path: String, ignore_white: bool) {
        let Some(svn) = self.svn_or_none() else {
            self.hint("没有可用的 svn.exe，无法读取差异");
            return;
        };
        let target = PathBuf::from(&path);
        self.pool
            .spawn(Kind::Diff, index, "查看差异".into(), move |sink| {
                let flags = crate::svn::diff_flags(ignore_white);
                sink.line(format!("$ svn diff --internal-diff{flags} \"{path}\""));
                let run = svn.diff(&target, ignore_white);
                Data::Run {
                    dir: index,
                    ok: run.ok,
                    message: if run.ok { run.out } else { run.summary() },
                    reload: false,
                }
            });
    }

    pub fn spawn_maintain(&mut self, index: usize, op: Maintain) {
        let (Some(svn), Some(path)) = (self.svn_or_none(), self.dir_path(index)) else {
            return;
        };
        let label = self.dir_label(index);
        let targets: Vec<String> = match op {
            Maintain::Add | Maintain::Delete => {
                let list = self
                    .commit
                    .as_ref()
                    .map(|commit| commit.selected_targets(op))
                    .unwrap_or_default();
                if list.is_empty() {
                    self.hint(format!("请先勾选需要「{}」的条目", op.label()));
                    return;
                }
                list
            }
            _ => Vec::new(),
        };
        if op == Maintain::Revert {
            self.hint("已撤销该目录下的全部本地修改");
        }
        let count = targets.len();
        let sub = match op {
            Maintain::Add => "add --force",
            Maintain::Delete => "delete",
            Maintain::Cleanup => "cleanup",
            Maintain::Resolve => "resolve --accept working --recursive",
            Maintain::Revert => "revert --recursive",
        };
        self.pool
            .spawn(Kind::Maintain, index, format!("{label} {sub}"), move |sink| {
                if count > 0 {
                    sink.line(format!("$ svn {sub} （{count} 项）"));
                } else {
                    sink.line(format!("$ svn {sub} \"{}\"", path.display()));
                }
                let run = match op {
                    Maintain::Add => svn.add(&path, &targets, &|line| sink.line(line)),
                    Maintain::Delete => svn.delete(&path, &targets, &|line| sink.line(line)),
                    Maintain::Cleanup => svn.cleanup(&path, &|line| sink.line(line)),
                    Maintain::Resolve => svn.resolve_working(&path, &|line| sink.line(line)),
                    Maintain::Revert => svn.revert_all(&path, &|line| sink.line(line)),
                };
                let message = if run.ok {
                    format!("{label}：{} 完成", op.label())
                } else {
                    format!("{label}：{} 失败——{}", op.label(), run.summary())
                };
                // 收尾文案只交给任务回收端统一写进输出区，避免同一句显示两遍
                Data::Run {
                    dir: index,
                    ok: run.ok,
                    message,
                    reload: op != Maintain::Cleanup,
                }
            });
    }

    /// 打开「修改仓库地址」窗口：原前缀默认填仓库根地址，只需填新的根地址。
    pub fn open_relocate(&mut self, index: usize) {
        let Some(info) = self.dirs.get(index).and_then(|view| view.info.clone()) else {
            self.hint("还没有读到该目录的仓库地址，请先「全部检测」后再试");
            return;
        };
        let from = if info.repos_root.is_empty() {
            info.url.clone()
        } else {
            info.repos_root.clone()
        };
        self.relocate = Some(Relocate {
            dir: index,
            from,
            to: String::new(),
            focus: true,
            error: String::new(),
        });
    }

    /// 执行 `svn relocate`：改写工作副本记录的仓库地址，不更新、不改动任何本地文件。
    pub fn spawn_relocate(&mut self, index: usize, from: String, to: String, new_url: String) {
        let (Some(svn), Some(path)) = (self.svn_or_none(), self.dir_path(index)) else {
            return;
        };
        if self.pool.is_busy(index) {
            self.hint("该目录还有任务在执行，请稍后再试");
            return;
        }
        let label = self.dir_label(index);
        self.pool
            .spawn(Kind::Relocate, index, format!("{label} 修改仓库地址"), move |sink| {
                sink.line(format!(
                    "$ svn relocate \"{from}\" \"{to}\" \"{}\"",
                    path.display()
                ));
                let run = svn.relocate(&path, &from, &to, &|line| sink.line(line));
                let message = if run.ok {
                    format!("{label}：仓库地址已改为 {new_url}")
                } else {
                    format!("{label}：修改仓库地址失败——{}", run.summary())
                };
                // 收尾文案只交给任务回收端统一写进输出区，避免同一句显示两遍
                Data::Run {
                    dir: index,
                    ok: run.ok,
                    message,
                    reload: false,
                }
            });
    }

    // ------------------------------------------------------------ 目录管理

    pub fn add_directory(&mut self, raw: impl AsRef<str>) {
        let text = raw.as_ref().trim().trim_matches('"').to_owned();
        if text.is_empty() {
            self.hint("请先输入目录路径，或点击「选择文件夹」");
            return;
        }
        let path = PathBuf::from(&text);
        if !path.is_dir() {
            self.hint(format!("目录不存在：{text}"));
            return;
        }
        let clean = path.display().to_string();
        let exists = self.cfg.dirs.iter().any(|d| {
            d.path.replace('/', "\\").eq_ignore_ascii_case(&clean.replace('/', "\\"))
        });
        if exists {
            self.hint("该目录已在列表中");
            return;
        }
        let label = std::mem::take(&mut self.new_label);
        self.cfg.dirs.push(DirConfig { path: clean.clone(), label, bc_target: String::new(), bc_filter: String::new() });
        self.sync_views();
        self.selected = Some(self.cfg.dirs.len() - 1);
        self.new_path.clear();
        self.persist();
        self.hint(format!("已添加目录：{clean}"));
        self.spawn_refresh(self.cfg.dirs.len() - 1, false);
    }

    pub fn remove_directory(&mut self, index: usize) {
        self.edit_label = None;
        self.edit_filter = None;
        if index >= self.cfg.dirs.len() {
            return;
        }
        let removed = self.cfg.dirs.remove(index);
        self.dirs.remove(index);
        if self.relocate.as_ref().is_some_and(|d| d.dir >= index) {
            // 行号变了，窗口里的地址可能已经对不上，直接关掉重新打开
            self.relocate = None;
        }
        self.sync_views();
        self.persist();
        self.confirm_remove = None;
        if self.commit.as_ref().is_some_and(|c| c.dir == index)
            || self.history.as_ref().is_some_and(|h| h.dir == index)
        {
            self.commit = None;
            self.history = None;
            self.page = Page::Main;
        }
        self.hint(format!("已从列表移除：{}（磁盘文件未删除）", removed.path));
    }

    pub fn move_directory(&mut self, index: usize, delta: isize) {
        let Some(target) = self.selected_target(index, delta) else {
            return;
        };
        self.cfg.dirs.swap(index, target);
        self.dirs.swap(index, target);
        if self.relocate.as_ref().is_some_and(|d| d.dir == index || d.dir == target) {
            self.relocate = None;
        }
        self.selected = Some(target);
        self.edit_label = None;
        self.edit_filter = None;
        self.persist();
    }

    fn selected_target(&self, index: usize, delta: isize) -> Option<usize> {
        let target = index as isize + delta;
        if target < 0 || target as usize >= self.cfg.dirs.len() {
            None
        } else {
            Some(target as usize)
        }
    }

    pub fn open_folder(&self, index: usize) {
        let Some(path) = self.dir_path(index) else {
            return;
        };
        let _ = std::process::Command::new("explorer").arg(&path).spawn();
    }

    /// 重置 Beyond Compare 的试用状态：删掉注册表里的 CacheID，结果逐条写进输出记录。
    pub fn reset_bc(&mut self) {
        for note in bcompare::reset_cache() {
            self.push(Level::Info, note);
        }
        self.hint("已重置 Beyond Compare（删除注册表 CacheID），重启 Beyond Compare 后生效，详情见输出记录");
    }

    /// 启动 Beyond Compare；targets 为空时只打开主窗口，传两个路径即为两路对比。
    /// 返回是否真的启动成功，方便调用方接着写自己的提示语。
    /// switches 是额外要交给 BC 的命令行开关，比如记录里的名称筛选 `/filters=...`。
    pub fn open_bcompare(&mut self, targets: &[PathBuf], switches: &[String]) -> bool {
        if !self.bc.is_file() {
            match bcompare::detect() {
                Some(found) => {
                    self.bc = found.clone();
                    self.cfg.bc_exe = found.to_string_lossy().into_owned();
                    self.persist();
                }
                None => {
                    self.hint("未找到 Beyond Compare（BCompare.exe），可在「设置 → Beyond Compare」中指定路径");
                    self.show_settings = true;
                    return false;
                }
            }
        }
        // 提前检查目标路径是否存在，避免 BC 打开后报「加载失败」
        for path in targets {
            if !path.exists() {
                let msg = format!("对比路径不存在或无法访问：{}", path.display());
                self.push(Level::Warning, msg.clone());
                self.hint(msg);
                return false;
            }
        }
        let paths: Vec<&Path> = targets.iter().map(|path| path.as_path()).collect();
        match bcompare::launch(&self.bc, &paths, switches) {
            Ok(()) => {
                // 日志按「开关在前、路径在后」显示，与真正传给 BC 的参数顺序一致
                let mut shown: Vec<String> = switches
                    .iter()
                    .map(|s| if s.contains(' ') { format!("\"{}\"", s) } else { s.clone() })
                    .collect();
                shown.extend(targets.iter().map(|path| format!("\"{}\"", path.display())));
                self.push(
                    Level::Command,
                    format!("$ \"{}\" {}", self.bc.display(), shown.join(" ")),
                );
                self.hint("已启动 Beyond Compare");
                true
            }
            Err(e) => {
                self.hint(e);
                false
            }
        }
    }

    /// 顶部「Beyond Compare」按钮：先拿「更多 → 绑定对比对象」里绑定的目录去 BC 的对比记录
    /// （BCSessions.xml）里找对应的那条，没绑定就按当前目录的文件夹名猜；命中就把记录的
    /// 名称筛选交给 BC——BC 收到路径只会按程序默认设置新建一次比较，不会自己去读记录。
    /// 服务器端（所选工作副本目录）放哪一侧由设置的 bc_server_side 决定；没绑定时按路径名
    /// 里的 server 关键字认记录两侧，认不出就沿用记录原有的左右顺序。
    /// 绑定了但记录里没有这条时，就直接按绑定的两个目录开一次比较；两者都没有则打开主窗口。
    pub fn open_bcompare_matched(&mut self) {
        let Some(index) = self.selected else {
            self.hint("尚未选中目录，已直接打开 Beyond Compare 主窗口");
            self.open_bcompare(&[], &[]);
            return;
        };
        let Some(dir) = self.dir_path(index) else {
            self.hint("选中目录的路径已失效，已直接打开 Beyond Compare 主窗口");
            self.open_bcompare(&[], &[]);
            return;
        };
        let folder = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let bound = self.cfg.dirs[index].bc_target.trim_matches('"').to_owned();
        let here = dir.to_string_lossy().trim_end_matches(['\\', '/']).to_owned();
        let there = bound.trim_end_matches(['\\', '/']).to_owned();
        let sessions = bcompare::sessions();
        let session = if bound.is_empty() {
            bcompare::match_session(&sessions, &dir)
        } else {
            // 绑定过就只认「左右正好是这两个目录」的那条记录：既能拿到记录里的名称筛选，
            // 也好对号入座认出哪一侧是服务器端。同一对路径 BC 常常自动存出好几条，
            // 优先挑带名称筛选的那条，别退回到命令行开出来的那条空记录
            let mut hit: Option<&bcompare::BcSession> = None;
            for record in &sessions {
                let same = (record.left.eq_ignore_ascii_case(&there) && record.right.eq_ignore_ascii_case(&here))
                    || (record.left.eq_ignore_ascii_case(&here) && record.right.eq_ignore_ascii_case(&there));
                if same && hit.map_or(true, |saved| saved.filter.is_empty() && !record.filter.is_empty()) {
                    hit = Some(record);
                }
            }
            hit
        };
        if session.is_none() && bound.is_empty() {
            self.hint(format!(
                "Beyond Compare 的 {} 条对比记录里没有匹配「{folder}」的，且没绑定对比对象；\
                 请在「更多 → 绑定对比对象」里指定另一侧，之后就不依赖本机的 BC 记录了",
                sessions.len()
            ));
            self.open_bcompare(&[], &[]);
            return;
        }
        let label = self.dir_label(index);
        // 四个值：左侧、右侧、记录里的名称筛选、记录里认出的对比对象（另一侧）。
        // 最后一项只在「没绑定对比对象」时才有，用来回填进配置，之后不依赖本机 BC 记录。
        let (left, right, record_filter, record_target) = match session {
            Some(session) => {
                self.push(
                    Level::Info,
                    format!(
                        "目录「{label}」匹配到对比记录：{}（{}），名称筛选：{}",
                        session.name,
                        if session.folder { "文件夹对比" } else { "文件对比" },
                        if session.filter.is_empty() { "无" } else { session.filter.as_str() }
                    ),
                );
                // 服务器端 = 所选的工作副本目录（xxx_server），本地端 = 绑定的对比对象。
                // 先从记录两侧认出服务器端：绑定时按 here/there 对号；
                // 没绑定就按路径名里的 server 关键字认，两侧都认不出时沿用记录原顺序
                let server = if !bound.is_empty() {
                    if session.left.eq_ignore_ascii_case(&here) {
                        Some(session.left.as_str())
                    } else if session.right.eq_ignore_ascii_case(&here) {
                        Some(session.right.as_str())
                    } else {
                        None
                    }
                } else {
                    let (a, b) = (
                        bcompare::looks_like_server(&session.left),
                        bcompare::looks_like_server(&session.right),
                    );
                    if a != b {
                        Some(if a { session.left.as_str() } else { session.right.as_str() })
                    } else {
                        None
                    }
                };
                let (lr, rr) = match server {
                    Some(server) => {
                        let local = if server == session.left.as_str() {
                            session.right.as_str()
                        } else {
                            session.left.as_str()
                        };
                        bcompare::order_sides(
                            Path::new(server),
                            Path::new(local),
                            &self.cfg.bc_server_side,
                        )
                    }
                    None => (
                        PathBuf::from(&session.left),
                        PathBuf::from(&session.right),
                    ),
                };
                // 只有当记录某一侧「正好就是」当前工作副本目录时，另一侧才是可靠的对比对象。
                // 记录可能是按文件夹名蒙中的（路径其实不一样），这时别回填，免得把目录绑成它自己
                let target = if session.left.eq_ignore_ascii_case(&here) {
                    Some(session.right.clone())
                } else if session.right.eq_ignore_ascii_case(&here) {
                    Some(session.left.clone())
                } else {
                    None
                };
                (
                    lr.display().to_string(),
                    rr.display().to_string(),
                    session.filter.clone(),
                    target,
                )
            }
            None => {
                // 所选工作副本目录是服务器端，绑定的对比对象是本地端，按设置排左右
                let (l, r) = bcompare::order_sides(&dir, Path::new(&bound), &self.cfg.bc_server_side);
                let (left, right) = (l.display().to_string(), r.display().to_string());
                self.push(
                    Level::Info,
                    format!("目录「{label}」按绑定的对比对象打开：{left} <--> {right}（BC 记录里没有这条，名称筛选走本程序配置）"),
                );
                (left, right, String::new(), None)
            }
        };
        // 把 BC 记录里认出来的东西回填进我们自己的配置：本机用户在 BC 里存过这条会话，
        // 记录里才有；其他用户机器上没有这条记录就读不到，于是既带不出筛选也认不出对比对象。
        // 回填后不再依赖本机 BC 记录，换机器、换人都能一致（配置可随项目分发）。
        let mut dirty = false;
        if self.cfg.dirs[index].bc_filter.is_empty() && !record_filter.is_empty() {
            self.cfg.dirs[index].bc_filter = record_filter.clone();
            dirty = true;
        }
        // 只在没手动绑定过时才回填对比对象，免得悄悄覆盖用户自己的选择
        if self.cfg.dirs[index].bc_target.is_empty() {
            if let Some(target) = record_target {
                if !target.is_empty() {
                    self.cfg.dirs[index].bc_target = target;
                    dirty = true;
                }
            }
        }
        if dirty {
            self.persist();
        }
        // 我们自己的配置优先（用户可在「更多」里直接填），没有时才退回 BC 记录的值
        let filter = if self.cfg.dirs[index].bc_filter.is_empty() {
            record_filter
        } else {
            self.cfg.dirs[index].bc_filter.clone()
        };
        let mut switches: Vec<String> = Vec::new();
        if !filter.is_empty() {
            switches.push(format!("/filters={filter}"));
        }
        self.open_bcompare(&[PathBuf::from(&left), PathBuf::from(&right)], &switches);
    }

    /// 用 Beyond Compare 对比单个文件的「BASE 版本 ↔ 本地版本」。
    pub fn open_bcompare_file(&mut self, index: usize, path: String) {
        let (Some(svn), Some(root)) = (self.svn_or_none(), self.dir_path(index)) else {
            return;
        };
        let target = PathBuf::from(&path);
        // 临时目录里保留工作副本内的相对路径，BC 两侧的标题更好对应；
        // 拿不到相对路径时直接放弃，避免误把 BASE 写回工作副本
        let Ok(relative) = target.strip_prefix(&root) else {
            self.hint("无法确定该文件在工作副本中的相对路径，已放弃对比");
            return;
        };
        let mirror = std::env::temp_dir()
            .join("SVNManager")
            .join("BASE")
            .join(relative);
        if let Some(parent) = mirror.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match svn.cat_base(&target, &mirror) {
            Ok(()) => {
                // 服务器端（BASE 版本）放哪一侧由设置决定，默认在左
                let (left, right) = bcompare::order_sides(&mirror, &target, &self.cfg.bc_server_side);
                self.open_bcompare(&[left, right], &[]);
            }
            Err(e) => {
                // 新增 / 未版本化的条目没有 BASE 版本，直接在 BC 里打开本地文件
                self.push(Level::Warning, format!("$ svn cat -r BASE \"{path}\" 失败：{e}"));
                self.open_bcompare(&[target], &[]);
            }
        }
    }

    pub fn open_commit(&mut self, index: usize) {
        if self.svn_or_none().is_none() {
            return;
        }
        let label = self.dir_label(index);
        self.commit = Some(CommitPage::new(index, label));
        self.page = Page::Commit;
        self.spawn_status(index, false);
    }

    pub fn open_history(&mut self, index: usize) {
        self.open_history_in(index, None);
    }

    /// 打开「提交记录」页并按日期区间读服务器（`None` = 照旧按条数读最近 N 条）。
    /// 统计图下钻走这里：点某天就直接落在那天的记录上。
    pub fn open_history_in(&mut self, index: usize, range: Option<(NaiveDate, NaiveDate)>) {
        if self.svn_or_none().is_none() {
            return;
        }
        let label = self.dir_label(index);
        let limit = self.cfg.log_limit;
        let user = self.svn_user.clone();
        let only_mine = self.cfg.history_only_mine;
        let mut page = HistoryPage::new(index, label, limit, user, only_mine);
        if let Some((from, to)) = range {
            // 下钻带进来的区间要在控件上看得见：落到「自定义」并把两个框填成那两天，
            // 用户一眼知道现在读的是哪段，也能就地改
            page.preset = Some(stats::RangePreset::Custom);
            page.custom_from = from.format("%Y-%m-%d").to_string();
            page.custom_to = to.format("%Y-%m-%d").to_string();
        }
        page.range = range;
        self.history = Some(page);
        self.page = Page::History;
        let mine = self.history.as_ref().is_some_and(|page| page.mine);
        self.spawn_log_in(index, mine, false, range);
    }

    pub fn back_to_main(&mut self) {
        self.page = Page::Main;
    }

    // ------------------------------------------------------------ 任务回收

    fn drain(&mut self, ctx: &egui::Context) {
        let polled = self.pool.poll();
        let mut busy = !polled.lines.is_empty() || !polled.done.is_empty() || !polled.stranded.is_empty();
        // 后台线程没回传结果就没了（svn 线程 panic、起线程失败）：必须说出来，否则界面就是「点了没反应」
        for (kind, dir, label) in polled.stranded {
            self.push(Level::Error, format!("任务「{label}」异常结束（{kind:?}，目录 #{dir}）"));
            self.hint(format!("「{label}」异常结束，没有取到结果，请重试"));
        }
        for line in polled.lines {
            let level = if line.starts_with('$') || line.starts_with('→') {
                Level::Command
            } else if line.contains("失败") || line.contains("Error") || line.contains("svn: E") {
                Level::Error
            } else if line.contains("Warning") || line.contains("警告") || line.contains("冲突") {
                Level::Warning
            } else if line.contains("完成") || line.starts_with('A') || line.starts_with('D') {
                Level::Success
            } else {
                Level::Info
            };
            self.push(level, line);
        }
        for (kind, _dir, data) in polled.done {
            match (kind, data) {
                (Kind::DetectSvn, Data::Svn { exe, version, candidates }) => {
                    self.probing = false;
                    self.candidates = candidates;
                    if exe.is_empty() {
                        self.hint("未能自动找到 svn.exe，请在「设置」中手动指定路径");
                        self.push(Level::Error, "未找到 svn.exe：请安装命令行版 Subversion（svn.exe），或在设置里手动指定");
                        self.show_settings = true;
                    } else {
                        self.push(Level::Success, format!("已找到 svn.exe：{exe}（版本 {version}）"));
                        self.apply_svn_exe(&exe);
                        self.hint(format!("已自动连接 svn.exe（版本 {version}）"));
                        if self.refresh_after_detect {
                            self.spawn_all_refresh();
                        }
                    }
                    self.refresh_after_detect = false;
                }
                (Kind::AuthUser, Data::User { user }) => {
                    self.svn_user = user.clone();
                    self.push(Level::Info, format!("本机 svn 登录人：{user}"));
                    // 启动时可能还没查到，查到后补一次「只看自己」的加载
                    let mut again = None;
                    if let Some(history) = self.history.as_mut() {
                        if history.author.is_empty() {
                            history.author = user;
                            history.mine = true;
                            again = Some(history.dir);
                        }
                    }
                    if let Some(dir) = again {
                        // 页面上已经按区间读的话，补读也要按同一条区间来，不能把用户筛出来的列表换成默认那批
                        let (unlimited, range) = self
                            .history
                            .as_ref()
                            .map(|page| (page.unlimited, page.range))
                            .unwrap_or((false, None));
                        self.spawn_log_in(dir, true, unlimited, range);
                    }
                    // 统计页可能正等这个登录人（打开时还没探测到），拿到人就补跑一次
                    self.stats_after_user_probe();
                }
                (Kind::CheckUpdate, Data::UpdateCheck { ok, message, info }) => {
                    if !ok {
                        // 启动时的自动检查失败只温和提醒（服务器没开是常态），不弹错误打断使用
                        self.push(Level::Warning, format!("检查更新失败：{message}"));
                    } else {
                        match info {
                            Some(manifest) => {
                                let latest = manifest.version.trim().to_owned();
                                let notes = manifest.notes.trim().to_owned();
                                let has_new = update::is_newer(&latest, APP_VERSION);
                                self.update_info = Some(manifest);
                                if has_new {
                                    self.push(
                                        Level::Warning,
                                        format!("发现新版本 V{latest}（当前 V{APP_VERSION}）"),
                                    );
                                    if !notes.is_empty() {
                                        self.push(Level::Info, format!("更新说明：\n{notes}"));
                                    }
                                    self.hint(format!(
                                        "有新版本 V{latest}，点击顶部「↑ 新版本」按钮即可更新"
                                    ));
                                } else {
                                    self.push(
                                        Level::Success,
                                        format!("已是最新版本（V{APP_VERSION}）"),
                                    );
                                }
                            }
                            None => self.push(Level::Warning, "更新源没有返回版本信息"),
                        }
                    }
                }
                (Kind::DownloadUpdate, Data::UpdateDownloaded { ok, message, bytes }) => {
                    if !ok {
                        // 失败原因也放进更新对话框里内联显示，不止在日志区
                        self.update_error = Some(message.clone());
                        self.hint(format!("下载新版本失败：{message}"));
                        self.push(Level::Error, format!("下载新版本失败：{message}"));
                    } else {
                        // 下载与校验都已就绪：交给收尾 bat 覆盖重启（会退出本程序）
                        self.launch_apply_bat();
                        let _ = bytes;
                    }
                }
                (Kind::AiLog, Data::AiLog { ok, content, message }) => {
                    // 结果整块回传（非流式）：成功直接放进生成窗口，失败原因也显示在窗口里
                    if ok {
                        self.ai_result = content.clone();
                        self.ai_error.clear();
                        self.push(
                            Level::Success,
                            format!("AI 日志已生成（{} 字），可在「AI 工作日志」窗口查看与复制", content.chars().count()),
                        );
                        self.hint("AI 日志已生成");
                    } else {
                        self.ai_error = message.clone();
                        self.push(Level::Error, format!("AI 生成日志失败：{message}"));
                    }
                }
                (
                    Kind::Refresh,
                    Data::Wc {
                        dir: index,
                        info,
                        remote,
                        remote_msg,
                        remote_rev,
                        last_rev,
                        last_author,
                        last_date,
                        changed,
                        changes,
                        conflicts,
                        out_of_date,
                    },
                ) => {
                    if let Some(view) = self.dirs.get_mut(index) {
                        view.info = info;
                        view.remote = remote;
                        view.remote_msg = remote_msg;
                        view.remote_rev = remote_rev;
                        view.last_rev = last_rev;
                        view.last_author = last_author;
                        view.last_date = last_date;
                        view.changed = changed;
                        view.changes = changes;
                        view.conflicts = conflicts;
                        view.out_of_date = out_of_date;
                        view.checked_at = chrono::Local::now().format("%H:%M:%S").to_string();
                    }
                    if let Some(quiet) = self
                        .pending_refresh
                        .iter()
                        .find(|(item, _)| *item == index)
                        .map(|&(_, quiet)| quiet)
                    {
                        self.pending_refresh.retain(|(item, _)| *item != index);
                        self.spawn_refresh(index, quiet);
                    }
                }
                (Kind::Status, Data::Status { dir: index, entries, ok, message }) => {
                    // 先数完再把 entries 交给提交页（它拿走所有权）
                    let blocked = ok.then(|| crate::svn::blocked_count(&entries));
                    if let Some(view) = self.dirs.get_mut(index) {
                        view.conflicts = blocked;
                        if ok {
                            // 口径与「全部上传」一致：? / ! 提交时会自动补 add / delete，一并算待提交
                            view.changes = entries
                                .iter()
                                .filter(|entry| entry.item.uploadable())
                                .cloned()
                                .collect();
                            view.changed = Some(view.changes.len());
                        } else {
                            // 读失败就清掉旧数：宁可显示「未检测」也不能拿旧清单冒充现状
                            view.changed = None;
                            view.changes.clear();
                        }
                    }
                    if let Some(commit) = self.commit.as_mut() {
                        if commit.dir == index {
                            if ok {
                                commit.set_entries(entries);
                            } else {
                                commit.error = message.clone();
                            }
                        }
                    }
                    if !ok {
                        self.push(Level::Error, format!("读取修改状态失败：{message}"));
                    }
                }
                (Kind::Log, Data::Log { dir: index, entries, ok, message }) => {
                    if let Some(history) = self.history.as_mut() {
                        if history.dir == index {
                            if ok {
                                history.entries = entries;
                            } else {
                                history.error = message.clone();
                            }
                        }
                    }
                    if !ok {
                        self.push(Level::Error, format!("读取提交记录失败：{message}"));
                    }
                }
                (Kind::Stats, Data::Stats { dir, epoch, entries, ok, message }) => {
                    // 一个目录的统计结果：epoch 对不上（用户中途换了条件）会在里面直接丢弃
                    self.apply_stats(dir, epoch, entries, ok, message.clone());
                    if !ok {
                        self.push(Level::Error, format!("读取提交统计失败：{message}"));
                    }
                }
                (Kind::Commit, Data::Run { dir, ok, message, reload }) => {
                    self.hint(message.clone());
                    self.push(if ok { Level::Success } else { Level::Error }, message);
                    if let Some(commit) = self.commit.as_mut() {
                        commit.done_ok = ok;
                        // 先清空避免重复提交已提交的条目，再重新读 status 取回真正剩下的改动
                        if ok {
                            commit.set_entries(Vec::new());
                        }
                    }
                    // 提交前的 svn add / svn delete 已经改过工作副本，提交没成功也要重读列表。
                    // 这两步都是程序自己补的自动刷新：静音执行，不在输出区刷 `$ svn …` 日志
                    if reload && self.commit.as_ref().is_some_and(|c| c.dir == dir) {
                        self.spawn_status(dir, true);
                    }
                    self.spawn_refresh(dir, true);
                }
                (Kind::Update, Data::Run { dir, ok, message, .. }) => {
                    self.hint(message.clone());
                    self.push(if ok { Level::Success } else { Level::Error }, message);
                    self.spawn_refresh(dir, false);
                    if self.commit.as_ref().is_some_and(|c| c.dir == dir) {
                        self.spawn_status(dir, false);
                    }
                    let again = self.history.as_mut().filter(|h| h.dir == dir).map(|h| {
                        h.entries.clear();
                        h.picked = None;
                        (h.mine, h.unlimited, h.range)
                    });
                    if let Some((mine, unlimited, range)) = again {
                        self.spawn_log_in(dir, mine, unlimited, range);
                    }
                }
                (Kind::Maintain, Data::Run { dir, ok, message, reload }) => {
                    self.hint(message.clone());
                    self.push(if ok { Level::Success } else { Level::Error }, message);
                    if reload {
                        self.spawn_status(dir, false);
                        self.spawn_refresh(dir, false);
                    }
                }
                (Kind::Relocate, Data::Run { dir, ok, message, .. }) => {
                    self.push(if ok { Level::Success } else { Level::Error }, message.clone());
                    if ok {
                        self.relocate = None;
                        self.hint(message);
                    } else if let Some(dialog) = self.relocate.as_mut().filter(|d| d.dir == dir) {
                        dialog.error = message;
                    }
                    self.spawn_refresh(dir, false);
                }
                (Kind::FileDiff, Data::Run { dir: index, ok, message, .. }) => {
                    if let Some(diff) = self.file_diff.as_mut().filter(|diff| diff.dir == index) {
                        diff.diff = message.clone();
                        diff.error = if ok { String::new() } else { message.clone() };
                    }
                    if !ok {
                        self.push(Level::Error, format!("读取该次提交的差异失败：{message}"));
                    }
                }
                (Kind::FileLog, Data::Log { dir: index, entries, ok, message }) => {
                    if let Some(log) = self.file_log.as_mut().filter(|log| log.dir == index) {
                        log.entries = entries;
                        log.error = if ok { String::new() } else { message.clone() };
                    }
                    if !ok {
                        self.push(Level::Error, format!("读取文件提交记录失败：{message}"));
                    }
                }
                (Kind::Diff, Data::Run { dir, ok, mut message, .. }) => {
                    if !ok && message.trim().is_empty() {
                        message = "无法取得差异（svn diff 执行失败）".to_owned();
                    }
                    // 忽略空白后为空，说明这一项只改了缩进 / 空格 / 换行符，不写清楚会以为读挂了
                    if ok && message.trim().is_empty() {
                        let ignored = self.commit.as_ref().is_some_and(|commit| commit.diff_ignore);
                        message = if ignored {
                            "忽略空白与换行后没有内容改动：这一项只是缩进、空格或换行符（CRLF↔LF）变了。\n取消勾选「忽略空白与换行」就能看到整份文件被逐行替换。".to_owned()
                        } else {
                            "没有差异：该文件内容与 BASE 版本一致。".to_owned()
                        };
                    }
                    if let Some(commit) = self.commit.as_mut().filter(|commit| commit.dir == dir) {
                        commit.diff = message;
                    }
                }
                (_, other) => {
                    if let Data::Run { dir, ok, message, .. } = other {
                        self.hint(message.clone());
                        self.push(if ok { Level::Success } else { Level::Error }, message);
                        self.spawn_refresh(dir, false);
                    }
                }
            }
            busy = true;
        }
        if busy {
            ctx.request_repaint();
        }
    }

    fn tick(&mut self, ctx: &egui::Context) {
        ctx.request_repaint_after(Duration::from_millis(150));
        // 主题随时可切换（顶部按钮 / 设置里选），这里统一同步给 egui；
        // 原生标题栏由 Options::sync_window_theme（默认开启）自动跟随
        let preference = match self.cfg.theme.as_str() {
            "light" => egui::ThemePreference::Light,
            "dark" => egui::ThemePreference::Dark,
            _ => egui::ThemePreference::System,
        };
        if ctx.options(|opt| opt.theme_preference) != preference {
            ctx.set_theme(preference);
        }
        self.drain(ctx);
        self.spawn_auth_user();
        // 启动后到点的第一次版本更新检查：官方源不用填地址就能查；
        // 自定义源要填了地址才查，地址为空就安静跳过（不催用户配置）
        if let Some(at) = self.update_check_at {
            if Instant::now() >= at {
                self.update_check_at = None;
                let ready = update::is_official(&self.cfg.update_source)
                    || !self.cfg.update_server.trim().is_empty();
                if self.cfg.check_update_on_start && ready {
                    self.spawn_update_check();
                }
            }
        }
        if self.cfg.auto_refresh > 0
            && !self.probing
            && self.pool.running() == 0
            && Instant::now() >= self.next_refresh
            && !self.cfg.dirs.is_empty()
        {
            self.next_refresh = Instant::now() + Duration::from_secs(self.cfg.auto_refresh);
            self.spawn_all_refresh();
        }
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|file| file.path().to_path_buf())
                .collect()
        });
        for path in dropped {
            if path.is_dir() {
                self.add_directory(path.display().to_string());
            }
        }
    }

    // ------------------------------------------------------------ 顶部状态栏

    fn connection(&self) -> (Color32, String, String) {
        let green = Color32::from_rgb(80, 200, 120);
        let amber = Color32::from_rgb(240, 190, 70);
        let red = Color32::from_rgb(240, 100, 100);
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
        if ok == total {
            (green, format!("已连接 SVN（{ok}/{total}）"), "全部目录均可访问仓库".into())
        } else if ok == 0 {
            let reason = checked
                .iter()
                .map(|d| d.remote_msg.as_str())
                .find(|m| !m.is_empty())
                .unwrap_or("无法访问仓库")
                .to_owned();
            (red, format!("未连接 SVN（0/{total}）"), reason)
        } else {
            (amber, format!("部分连接（{ok}/{total}）"), "存在无法访问的目录".into())
        }
    }

    fn header(&mut self, ui: &mut Ui) {
        let (color, text, reason) = self.connection();
        let color = ink(ui, color);
        let running = self.pool.running();
        let busy_label = self
            .selected
            .and_then(|index| self.pool.busy_label(index))
            .or_else(|| if running > 0 { Some("任务执行中".into()) } else { None });
        ui.horizontal(|ui| {
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
                if self
                    .update_info
                    .as_ref()
                    .is_some_and(|m| update::is_newer(m.version.trim(), APP_VERSION))
                {
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
                if ui.button("设置").clicked() {
                    self.show_settings = !self.show_settings;
                }
                let theme_name = match self.cfg.theme.as_str() {
                    "light" => "浅色",
                    "dark" => "深色",
                    _ => "跟随系统",
                };
                if ui
                    .button(format!("主题：{theme_name}"))
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
                if ui
                    .button(if self.page == Page::Stats { "提交统计 ●" } else { "提交统计" })
                    .on_hover_text(
                        "统计本人在指定区间里提交涉及的文件数量，整页显示。\n\
                         区间：当天 / 一周 / 一个月 / 一年 / 所有 / 自定义起止日期。\n\
                         图：折线图、直方图、火力图（日历格子，颜色越深当天提交越多）。\n\
                         可以只看某一个目录，也可以选「全部目录（合并）」一起看；\n\
                         合并时按「版本 + 仓库路径」去重，同一仓库挂在两个目录行下不会重复计。",
                    )
                    .clicked()
                {
                    self.open_stats();
                }
                if ui.button("重新寻找 svn").clicked() {
                    self.spawn_detect();
                }
                if ui.button("全部刷新").clicked() {
                    self.hint("正在刷新全部目录 …");
                    self.spawn_all_refresh();
                }
                if ui
                    .button("Beyond Compare")
                    .on_hover_text("优先用「更多 → 绑定对比对象」的目录，其次按文件夹名匹配 Beyond Compare 已保存的对比记录，并带上记录里的名称筛选打开；都没有时打开主窗口")
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
                if ui
                    .button(ai_label)
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
            });
        });
        ui.horizontal(|ui| {
            let exe = if self.cfg.svn_exe.is_empty() { "未设置".to_owned() } else { self.cfg.svn_exe.clone() };
            ui.label(RichText::new(format!("svn.exe：{exe}")).weak().size(12.0));
            ui.separator();
            ui.label(
                RichText::new("提示：可把文件夹直接拖入窗口添加；双击目录行打开文件夹")
                    .weak()
                    .size(12.0),
            );
        });
        if !self.hint.is_empty() {
            ui.label(
                RichText::new(self.hint.clone())
                    .size(12.5)
                    .color(ink(ui, Color32::from_rgb(140, 205, 255))),
            );
        }
    }

    // ------------------------------------------------------------ 底部输出面板

    fn output_panel(&mut self, ui: &mut Ui) {
        let count = self.output.len();
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("输出记录（{count} 行）")).strong().size(13.0));
            if ui.button("清空").clicked() {
                self.output.clear();
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut auto_scroll = self.auto_scroll;
                ui.checkbox(&mut auto_scroll, "自动滚动");
                self.auto_scroll = auto_scroll;
                if ui.button("打开配置目录").clicked() {
                    let dir = config::config_file()
                        .parent()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    let _ = std::process::Command::new("explorer").arg(dir).spawn();
                }
            });
        });
        ui.separator();
        let auto_scroll = self.auto_scroll;
        let output = self.output.clone();
        ScrollArea::vertical()
            .id_salt("output_area")
            .auto_shrink([false, false])
            .stick_to_bottom(auto_scroll)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                for line in output.iter().rev().take(600).rev() {
                    let color = if line.level == Level::Info {
                        ui.visuals().weak_text_color()
                    } else {
                        line.level.color(ui)
                    };
                    ui.monospace(RichText::new(&line.text).size(12.0).color(color));
                }
            });
    }

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

    // ------------------------------------------------------------ 「修改仓库地址」窗口

    fn relocate_dialog(&mut self, ctx: &egui::Context) {
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
    fn upload_all_dialog(&mut self, ctx: &egui::Context) {
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

    // ------------------------------------------------------------ 设置窗口

    fn settings(&mut self, ctx: &egui::Context) {
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

    /// 「发现新版本」确认对话框：顶部「↑ 新版本」按钮与设置里的「立即更新」
    /// 都打开它，点「开始更新」后直接进入下载，不再绕道设置页。
    fn update_confirm_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_update_confirm {
            return;
        }
        let Some(manifest) = self.update_info.clone() else {
            // 没有可用的更新信息（理论上只有 has_new 才打开）就顺手关掉
            self.show_update_confirm = false;
            return;
        };
        let downloading = self.pool.has(Kind::DownloadUpdate, usize::MAX);
        let mut open = self.show_update_confirm;
        egui::Window::new("发现新版本")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(440.0)
            .default_pos(egui::pos2(430.0, 200.0))
            .show(ctx, |ui| {
                // 版本对比一行看明白：当前（弱） → 新版本（大、主题蓝）
                ui.horizontal(|ui| {
                    ui.add_space(2.0);
                    ui.label(RichText::new(format!("V{APP_VERSION}")).size(17.0).weak());
                    ui.label(RichText::new("→").size(17.0).weak());
                    ui.label(
                        RichText::new(format!("V{}", manifest.version.trim()))
                            .size(20.0)
                            .strong()
                            .color(ink(ui, Color32::from_rgb(120, 190, 240))),
                    );
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
                    let start = ui.add_enabled(
                        !downloading,
                        egui::Button::new(
                            RichText::new(if downloading { "下载中…" } else { "开始更新" }).strong(),
                        ),
                    );
                    if start.clicked() {
                        // 保持在对话框里看下载状态，不再一按就关
                        self.update_error = None;
                        self.begin_update();
                    }
                    if ui.add_enabled(!downloading, egui::Button::new("稍后再说")).clicked() {
                        self.show_update_confirm = false;
                    }
                });
            });
        self.show_update_confirm = open;
    }
}

impl eframe::App for SvnApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.tick(ctx);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("header").show(ui, |ui| {
            self.header(ui);
        });
        egui::Panel::bottom("output")
            .default_size(190.0)
            .resizable(true)
            .show(ui, |ui| {
                self.output_panel(ui);
            });
        let ctx = ui.ctx().clone();
        egui::CentralPanel::default().show(ui, |ui| {
            let page = self.page;
            match page {
                Page::Main => self.main_page(ui),
                Page::Commit => self.commit_page(ui),
                Page::History => self.history_page(ui),
                Page::Stats => self.stats_page(ui),
            }
        });
        self.settings(&ctx);
        self.update_confirm_dialog(&ctx);
        self.relocate_dialog(&ctx);
        self.upload_all_dialog(&ctx);
        self.file_diff_window(&ctx);
        self.file_log_window(&ctx);
        self.worklog_window(&ctx);
    }
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
    use egui::{Pos2, RawInput, Rect, ThemePreference};

    /// 跑一遍高亮，把结果拆成「未命中的原文」与「命中的片段」，方便断言。
    fn split(dark: bool, text: &str, needle: &str) -> (String, Vec<String>) {
        let ctx = egui::Context::default();
        ctx.set_theme(if dark {
            ThemePreference::Dark
        } else {
            ThemePreference::Light
        });
        let mut plain = String::new();
        let mut marks: Vec<String> = Vec::new();
        let mut output = ctx.run_ui(RawInput::default(), |ui| {
            let job = highlight(ui, text, needle, FontId::proportional(12.0), Color32::WHITE);
            for section in &job.sections {
                let range = section.byte_range.start.0..section.byte_range.end.0;
                let piece = &job.text[range];
                if piece.is_empty() {
                    continue;
                }
                if section.format.background == Color32::TRANSPARENT {
                    plain.push_str(piece);
                } else {
                    marks.push(piece.to_owned());
                }
            }
        });
        // 测试不渲染纹理，显式丢弃字体增量，否则 epaint 在 Drop 时按未应用增量 panic
        output.textures_delta.clear();
        (plain, marks)
    }

    #[test]
    fn empty_needle_keeps_text_untouched() {
        let (plain, marks) = split(true, "svn log -r HEAD:1", "   ");
        assert_eq!(plain, "svn log -r HEAD:1");
        assert!(marks.is_empty(), "{marks:?}");
    }

    #[test]
    fn marks_every_match_case_insensitively() {
        let (plain, marks) = split(true, "SVN svn svn/HRP.Hr", "svn");
        assert_eq!(marks, vec!["SVN", "svn", "svn"]);
        assert_eq!(plain, "  /HRP.Hr");
    }

    #[test]
    fn multibyte_needle_splits_on_char_boundaries() {
        // 前后夹着中文，确认按字节匹配不会把 UTF-8 序列切坏（切坏会直接 panic）
        let (plain, marks) = split(false, "修复 药品明细 保存 药品", "药品");
        assert_eq!(marks, vec!["药品", "药品"]);
        assert_eq!(plain, "修复 明细 保存 ");
    }

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

    // ------------------------------------------------------ 简约样式：无头绘制与点击链

    /// 只填字段、不起任何 svn 任务的 app。卡片渲染和点击链要在无头里跑，就得先有个能用的实例。
    fn stub_app(compact: bool) -> SvnApp {
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
    fn base_input() -> RawInput {
        RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1400.0, 800.0))),
            focused: true,
            ..Default::default()
        }
    }

    /// 把主页跑一帧，返回这一帧画出来的「文字 + 矩形」。弹层的开关状态存在内存里，
    /// 点下去那一帧还画不出弹层，要多跑一帧稳定的空帧才有。
    struct Stage {
        ctx: egui::Context,
        app: SvnApp,
    }

    impl Stage {
        fn new(compact: bool) -> Self {
            let ctx = egui::Context::default();
            // 颜色断言要确定：ink() 在浅色主题下会把强调色压暗，所以固定深色
            ctx.set_theme(ThemePreference::Dark);
            crate::fonts::install_cjk(&ctx);
            Self { ctx, app: stub_app(compact) }
        }

        fn step(&mut self, click: Option<Pos2>) -> Vec<(String, Rect)> {
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
        fn hover(&mut self, pos: Pos2) -> Vec<(String, Rect)> {
            self.run(vec![egui::Event::PointerMoved(pos)])
        }

        fn run(&mut self, events: Vec<egui::Event>) -> Vec<(String, Rect)> {
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

        fn rect_of(texts: &[(String, Rect)], label: &str) -> Option<Rect> {
            texts
                .iter()
                .find(|(text, _)| text == label)
                .map(|(_, rect)| *rect)
        }
    }

    /// 开关一按就得换样式，而且要把选择写回配置（写配置前把 APPDATA 指到临时目录，
    /// 免得测试覆盖掉用户真实的 config.json）
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

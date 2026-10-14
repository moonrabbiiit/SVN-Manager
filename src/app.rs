//! 应用本体：状态构造、每帧的定时与拖放、后台任务结果的回收，以及底部输出面板。
//!
//! `RepoApp` 的结构体定义与共享类型留在 main.rs（crate 根），这样各模块照旧写 `crate::RepoApp`。

use crate::config;
use crate::{APP_TITLE, APP_VERSION, DirView, Level, OutLine, Page, RepoApp};
use crate::{bcompare, fonts, pane_fill, pane_stroke, quiet, update};

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use egui::{Align, Layout, RichText, ScrollArea, Ui};
use crate::config::Config;
use crate::jobs::{Data, Kind, Pool};
use crate::svn::Svn;
use crate::ui::AlignedRow;

impl RepoApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let cfg = Config::load();
        let font_note = fonts::note_for(&cfg.ui_font, fonts::install(&cc.egui_ctx, &cfg.ui_font));
        let accent = {
            let [r, g, b] = crate::ui::accent_rgb(&cfg.accent);
            egui::Color32::from_rgb(r, g, b)
        };
        for style_theme in [egui::Theme::Dark, egui::Theme::Light] {
            cc.egui_ctx
                .style_mut_of(style_theme, |style| crate::ui::tune_style(style, accent));
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
        // git.exe：配置里有路径就用，留空则按 PATH / 注册表 / 标准安装位置现找。
        // 探测很快（几次 --version），启动时同步做，不像 svn 那样要进任务池
        let mut git_exe = PathBuf::from(&cfg.git_exe);
        if git_exe.as_os_str().is_empty() {
            if let Some((found, _)) = crate::git::detect(crate::git::candidates()) {
                git_exe = found;
            }
        }
        let git = crate::git::Git { exe: git_exe };
        let git_version = git.version();
        // 主窗口句柄只在启动时取一次：DWM 的原生标题栏配色只认 HWND，主题随时可切
        #[cfg(windows)]
        let hwnd = {
            use raw_window_handle::HasWindowHandle;
            cc.window_handle().ok().and_then(|handle| match handle.as_raw() {
                raw_window_handle::RawWindowHandle::Win32(win32) => {
                    Some(win32.hwnd.get() as isize)
                }
                _ => None,
            })
        };
        #[cfg(not(windows))]
        let hwnd = None;
        // 自绘标题栏：抹掉系统标题栏，天区 / 缩放 / 按钮命中全由界面层接管
        //（顶栏绘制在 header.rs，命中的原生半边在 titlebar.rs）
        #[cfg(windows)]
        if let Some(hwnd) = hwnd {
            crate::titlebar::install(hwnd);
        }
        // 通知身份：注册自己的 AUMID（显示名 + 图标），系统通知显示本程序而非借用的 PowerShell
        crate::notify::ensure_identity();
        // 右上角与窗口控制键的 SVG 图标：启动时栅格化一次，之后每帧只是贴图
        let icons = crate::icons::IconSet::load(&cc.egui_ctx);
        let mut app = Self {
            dirs: vec![DirView::default(); cfg.dirs.len()],
            probing: false,
            refresh_after_detect: true,
            hwnd,
            caption_dark: None,
            corner_maximized: None,
            version,
            candidates: Vec::new(),
            cfg,
            svn,
            git,
            git_version,
            bc,
            pool: Pool::default(),
            output: Vec::new(),
            page: Page::Main,
            commit: None,
            history: None,
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
            edit_group: None,
            group_buf: String::new(),
            group_focus: false,
            edit_tag: None,
            tag_buf: String::new(),
            tag_focus: false,
            drag: None,
            drag_pending: None,
            drag_pos: None,
            drag_target: None,
            drop_zones: Vec::new(),
            relocate: None,
            upload_all: None,
            batch_relocate: None,
            batch_label: None,
            rollback: None,
            svn_user: String::new(),
            user_probed: false,
            show_settings: false,
            confirm_remove: None,
            auto_scroll: true,
            next_refresh: Instant::now() + Duration::from_secs(4),
            pending_refresh: Vec::new(),
            notify_queue: Vec::new(),
            font_note,
            update_info: None,
            update_ready: false,
            // 启动 3 秒后开始检查更新：避开启动瞬间的 svn 探测高峰
            update_check_at: Some(Instant::now() + Duration::from_secs(3)),
            // 开关打开后每隔 5 分钟查一次；这里先给一个周期，开启时会重置
            next_update_check: Instant::now() + Duration::from_secs(update::AUTO_CHECK_EVERY_SECS),
            show_update_confirm: false,
            update_error: None,
            ai_picks: Vec::new(),
            ai_pick_dir: 0,
            show_worklog: false,
            ai_mode: false,
            ai_extra: String::new(),
            ai_result: String::new(),
            ai_error: String::new(),
            mascot: mascot_texture(&cc.egui_ctx),
            icons: Some(icons),
        };
        app.push(Level::Info, format!("{APP_TITLE} 已启动（配置：{}）", config::config_file().display()));
        // 后端要在任何检测之前定下来：sync_views 之前每个 DirView 的 kind 还是默认的 Svn，
        // 先刷新一轮就会拿 svn 去问 Git 目录，报出「不是 SVN 工作副本 E155007」
        app.sync_views();
        // 自更新的残局在这里收：上次换名留下的 `.old.exe`（旧进程刚退出才删得掉）、
        // 以及下载完没走到换名的暂存文件
        if let Ok(exe) = std::env::current_exe() {
            if update::clean_leftovers(&exe) {
                app.hint(format!("已更新到 V{APP_VERSION}，旧程序文件已清理"));
                app.push(
                    Level::Success,
                    format!("本次是自更新后的第一次启动：V{APP_VERSION} 已就位"),
                );
            }
        }
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
        if app.git.available() {
            app.push(
                Level::Info,
                format!(
                    "git.exe = {}（版本 {}）",
                    app.git.exe.display(),
                    app.git_version.clone().unwrap_or_default()
                ),
            );
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

    /// 提示不再常驻在底部那一行，直接作为一条 INFO 写进上面的日志区
    pub fn hint(&mut self, text: impl Into<String>) {
        self.push(Level::Info, text);
    }

    /// 测试用：日志末尾几条拼起来看（一条提示可能占好几行，只看最后一条会截到半句）
    #[cfg(test)]
    pub(crate) fn log_tail(&self) -> String {
        let from = self.output.len().saturating_sub(4);
        self.output[from..]
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
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

    pub(crate) fn sync_views(&mut self) {
        self.dirs.resize(self.cfg.dirs.len(), DirView::default());
        self.dirs.truncate(self.cfg.dirs.len());
        // 后端类型跟着磁盘走：目录里有 .git 就是 Git，有 .svn 就是 SVN
        for (index, dir) in self.cfg.dirs.iter().enumerate() {
            if let Some(view) = self.dirs.get_mut(index) {
                if let Some(kind) = crate::repo::detect(Path::new(&dir.path)) {
                    view.kind = kind;
                }
            }
        }
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

    pub fn apply_git_exe(&mut self, exe: &str) {
        self.cfg.git_exe = exe.trim().trim_matches('"').to_owned();
        self.git.exe = PathBuf::from(&self.cfg.git_exe);
        self.git_version = self.git.version();
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

    /// 该目录的后端类型。还没检测过时按 SVN 算（列表里躺着的老目录绝大多数是 SVN）。
    pub(crate) fn dir_kind(&self, index: usize) -> crate::repo::RepoKind {
        self.dirs
            .get(index)
            .map(|view| view.kind)
            .unwrap_or(crate::repo::RepoKind::Svn)
    }

    pub(crate) fn git_or_none(&mut self) -> Option<crate::git::Git> {
        if self.git.available() {
            Some(self.git.clone())
        } else {
            self.hint("未找到可用的 git.exe，请在「设置 → git.exe 路径」中指定");
            self.show_settings = true;
            None
        }
    }

    // ------------------------------------------------------------ 任务回收

    fn drain(&mut self, ctx: &egui::Context) {
        let polled = self.pool.poll();
        let mut busy = !polled.lines.is_empty() || !polled.done.is_empty() || !polled.stranded.is_empty();
        // 后台线程没回传结果就没了（svn 线程 panic、起线程失败）：必须说出来，否则界面就是「点了没反应」
        for (kind, dir, label) in polled.stranded {
            self.push(Level::Error, format!("任务「{label}」异常结束（{kind:?}，目录 #{dir}）"));
            self.hint(format!("「{label}」异常结束，未取得结果，请重试"));
        }
        for line in polled.lines {
            let level = if line.starts_with('$') || line.starts_with('→') {
                Level::Command
            } else if line.contains("失败")
                || line.contains("Error")
                || line.contains("svn: E")
                || line.contains("fatal:")
                || line.contains("error:")
            {
                Level::Error
            } else if line.contains("Warning")
                || line.contains("警告")
                || line.contains("冲突")
                || line.contains("warning:")
            {
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
                }
                (Kind::CheckUpdate, Data::UpdateCheck { ok, message, info, ready, quiet }) => {
                    if !ok {
                        // 启动时的自动检查失败只温和提醒（服务器没开是常态），不弹错误打断使用；
                        // 定时那一轮直接闭嘴，下一轮到点再试
                        if !quiet {
                            self.push(Level::Warning, format!("检查更新失败：{message}"));
                        }
                    } else {
                        match info {
                            Some(manifest) => {
                                let latest = manifest.version.trim().to_owned();
                                let notes = manifest.notes.trim().to_owned();
                                self.update_info = Some(manifest);
                                self.update_ready = ready;
                                if ready {
                                    // 版本号没动、只是换了构建时不能报「新版本 V1.2.4（当前 V1.2.4）」
                                    let replaced = update::same_version(&latest, APP_VERSION);
                                    self.push(
                                        Level::Warning,
                                        if replaced {
                                            format!("发现新版本 V{latest}（新构建）")
                                        } else {
                                            format!("发现新版本 V{latest}（当前 V{APP_VERSION}）")
                                        },
                                    );
                                    if !notes.is_empty() {
                                        self.push(Level::Info, format!("更新说明：\n{notes}"));
                                    }
                                    self.hint(format!(
                                        "发现可更新版本 V{latest}，点击顶部「↑ 新版本」按钮即可更新"
                                    ));
                                } else if !quiet {
                                    // 定时检查没查到新版本就什么都不说，日志区只留 svn 操作
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
                        // 下载与校验都已就绪：把新版换到正式名字下并重启（会退出本程序）
                        self.apply_downloaded_update();
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
                    // 更新通知订阅：远端可达、确实有未更新项、版本号没通知过才入队；
                    // 入队即记下该版本，同一帧内重复结果 / 下一轮检测都不会再报
                    let label = self.dir_label(index);
                    if let Some(view) = self.dirs.get(index) {
                        if let Some(dir_cfg) = self.cfg.dirs.get_mut(index) {
                            if dir_cfg.notify
                                && view.remote == Some(true)
                                && view.out_of_date.unwrap_or(0) > 0
                                && !view.remote_rev.is_empty()
                                && view.remote_rev != dir_cfg.notified_rev
                            {
                                dir_cfg.notified_rev = view.remote_rev.clone();
                                self.notify_queue.push(crate::notify::UpdateNotice {
                                    label,
                                    rev: view.remote_rev.clone(),
                                    author: view.last_author.clone(),
                                    behind: view.out_of_date.unwrap_or(0),
                                });
                            }
                        }
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
                (Kind::Diagnose, Data::Run { dir: _, ok, message, .. }) => {
                    // 报告已经写成文件了，这里只报结果，不用跟着刷新整棵树
                    self.hint(message.clone());
                    self.push(if ok { Level::Success } else { Level::Error }, message);
                }
                (Kind::LocalDiff, Data::LocalDiff { dir: index, ok, summary }) => {
                    if let Some(view) = self.dirs.get_mut(index) {
                        // 读失败同样清成 None：本地端比对慢几秒，拿上一次的数冒充现状最坏
                        view.local_diff = ok.then_some(summary);
                        view.local_checked_at = if ok {
                            chrono::Local::now().format("%H:%M:%S").to_string()
                        } else {
                            String::new()
                        };
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
                (Kind::Rollback, Data::Run { dir, ok, message, reload }) => {
                    if ok {
                        // 回退成了就关窗，改动已经在「待提交」里等着核对
                        self.rollback = None;
                        self.hint(message.clone());
                    } else if let Some(dialog) = self.rollback.as_mut().filter(|d| d.dir == dir) {
                        dialog.error = message.clone();
                    }
                    self.push(if ok { Level::Success } else { Level::Error }, message);
                    // 失败也可能是 merge 做成一半，照样重读一遍才是真的状态
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
        // 本帧收集齐的更新订阅统一发一条系统通知：多个目录合并为一条，不逐目录连环弹。
        // 已通知版本号在入队时就写进了配置，这里无论发送成功与否都持久化，避免失败后反复重试弹窗
        if !self.notify_queue.is_empty() {
            let items: Vec<_> = self.notify_queue.drain(..).collect();
            self.persist();
            match crate::notify::send_update_toast(&items) {
                Ok(()) => {
                    let names = items
                        .iter()
                        .map(|item| item.label.as_str())
                        .collect::<Vec<_>>()
                        .join("、");
                    self.push(Level::Info, format!("已发送更新通知：{names}"));
                }
                Err(e) => self.push(Level::Warning, format!("发送系统通知失败：{e}")),
            }
        }
        if busy {
            ctx.request_repaint();
        }
    }

    fn tick(&mut self, ctx: &egui::Context) {
        ctx.request_repaint_after(Duration::from_millis(150));
        // 主题随时可切换（顶部按钮 / 设置里选），这里统一同步给 egui；
        // 原生标题栏不再交给 sync_window_theme 的出厂色，下面按当前视觉自己写给 DWM
        let preference = match self.cfg.theme.as_str() {
            "light" => egui::ThemePreference::Light,
            "dark" => egui::ThemePreference::Dark,
            _ => egui::ThemePreference::System,
        };
        if ctx.options(|opt| opt.theme_preference) != preference {
            ctx.set_theme(preference);
        }
        // 首帧设一次，之后只有深浅真的换了才重设（DWM 调用不便宜，也没必要逐帧发）
        let dark = ctx.theme() == egui::Theme::Dark;
        if self.caption_dark != Some(dark) {
            self.caption_dark = Some(dark);
            let style = ctx.style_of(ctx.theme());
            style_title_bar(
                self.hwnd,
                dark,
                style.visuals.panel_fill,
                style.visuals.widgets.noninteractive.fg_stroke.color,
            );
        }
        // 圆角随最大化切换：非最大化四角圆角（Win11），最大化直角；状态变了才发一次
        let maximized = ctx.input(|i| i.viewport().maximized) == Some(true);
        if self.corner_maximized != Some(maximized) {
            self.corner_maximized = Some(maximized);
            if let Some(hwnd) = self.hwnd {
                crate::titlebar::refresh_shape(hwnd, maximized);
            }
        }
        self.drain(ctx);
        self.spawn_auth_user();
        // 启动后到点的第一次版本更新检查：官方源不用填地址就能查；
        // 自定义源要填了地址才查，地址为空就安静跳过（不催用户配置）
        if let Some(at) = self.update_check_at {
            if Instant::now() >= at {
                self.update_check_at = None;
                if self.cfg.check_update_on_start && self.update_source_ready() {
                    self.spawn_update_check(false);
                }
            }
        }
        // 自动检查更新：开关打开后每 5 分钟查一次。正在查的时候不重复起，
        // 过程静音（只有发现新版本或失败才在日志区说一声）
        if self.cfg.auto_update_check
            && Instant::now() >= self.next_update_check
            && self.update_source_ready()
            && !self.pool.has(Kind::CheckUpdate, usize::MAX)
        {
            self.next_update_check =
                Instant::now() + Duration::from_secs(update::AUTO_CHECK_EVERY_SECS);
            self.spawn_update_check(true);
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

    // ------------------------------------------------------------ 底部输出面板

    fn output_panel(&mut self, ui: &mut Ui) {
        let count = self.output.len();
        ui.aligned_row(|ui| {
            ui.label(RichText::new(format!("输出运行（{count} 行）")).strong().size(13.0));
            if quiet(ui, |ui| ui.button("清空")).clicked() {
                self.output.clear();
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut auto_scroll = self.auto_scroll;
                ui.checkbox(&mut auto_scroll, "自动滚动");
                self.auto_scroll = auto_scroll;
                if quiet(ui, |ui| ui.button("打开配置目录")).clicked() {
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
}

/// 原生标题栏跟着当前视觉走。eframe 自带的 sync_window_theme 只会切「出厂深 / 出厂浅」，
/// 切不出我们这套冷白与冷黑，顶部于是总像别人家的窗口；Win11 的 DWM 允许直接把
/// 标题栏底色和文字色写掉，于是那一条终于和界面是同一套颜色。
/// 非 Windows、拿不到 HWND、或者系统不认这些属性（Win10 及更早）——一律什么都不做，
/// 出厂标题栏照样能用，不值得为配色报错。
fn style_title_bar(hwnd: Option<isize>, dark: bool, fill: egui::Color32, text: egui::Color32) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Graphics::Dwm::{
            DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
            DwmSetWindowAttribute,
        };
        let Some(hwnd) = hwnd else { return };
        let hwnd = hwnd as *mut core::ffi::c_void;
        // COLORREF 的字节序是 0x00BBGGRR，和 Color32 的 RGBA 正好反着
        let colorref = |color: egui::Color32| {
            let [r, g, b, _] = color.to_array();
            ((b as u32) << 16) | ((g as u32) << 8) | r as u32
        };
        let immersive: i32 = if dark { 1 } else { 0 };
        let caption = colorref(fill);
        let foreground = colorref(text);
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
                &immersive as *const i32 as *const _,
                4,
            );
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_CAPTION_COLOR as u32,
                &caption as *const u32 as *const _,
                4,
            );
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_TEXT_COLOR as u32,
                &foreground as *const u32 as *const _,
                4,
            );
        }
    }
    #[cfg(not(windows))]
    let _ = (hwnd, dark, fill, text);
}

/// 右下角吉祥物的显示高度（逻辑点）：原图 872×1160，宽度按原比例跟着算。
/// 日志面板被拖矮时按面板高收，不然头顶会被面板裁掉
const MASCOT_H: f32 = 200.0;

/// 那张贴图的不透明度 0.2（× 255 取整）
const MASCOT_ALPHA: u8 = 51;

/// 启动时把嵌进 exe 的 PNG 解成贴图；解不出来就返回 None，界面少一枚贴图而已
fn mascot_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let rgba = image::load_from_memory(include_bytes!("../mascot.png"))
        .ok()?
        .to_rgba8();
    let image = egui::ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        rgba.as_raw(),
    );
    Some(ctx.load_texture("mascot", image, egui::TextureOptions::LINEAR))
}

/// 贴在日志面板的右下角。只画不注册感应区，所以既不吃点击也不抢日志行的选中
fn paint_mascot(ui: &Ui, tex: &egui::TextureHandle) {
    let outer = ui.max_rect();
    let height = MASCOT_H.min(outer.height());
    let width = height * tex.aspect_ratio();
    ui.painter().image(
        tex.id(),
        egui::Rect::from_min_size(
            egui::pos2(outer.max.x - width, outer.max.y - height),
            egui::vec2(width, height),
        ),
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::from_white_alpha(MASCOT_ALPHA),
    );
}

impl eframe::App for RepoApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 字号 = 整屏缩放：egui 的 zoom_factor 是「系统 DPI × 这个倍数」，文字和控件一起放大。
        // 每帧对一次，设置页改完下一帧就生效，换显示器 / 改系统缩放也能跟上
        let want = fonts::scale_factor(&self.cfg.ui_scale);
        if ctx.zoom_factor() != want {
            ctx.set_zoom_factor(want);
        }
        self.tick(ctx);
    }

    /// eframe 默认的清屏色是近黑（12,12,12,180），面板之间的接缝只要有一像素没被底色盖住，
    /// 淡色描边就是往这支黑上混合——量出来 #E4E8F0 会变成 #313337，看着就是一条黑线。
    /// 底色跟着主题走，接缝处补上的就是同一档白
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        // 顶栏就是自绘标题栏：左右留出一点呼吸位，高度与分隔线在 header() 里画
        egui::Panel::top("header")
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(10, 0)))
            .show(ui, |ui| {
                self.header(ui);
            });
        // 添加目录这条要顶到窗口两边，所以挂在中央面板之外（主页才画）
        if self.page == Page::Main {
            let (fill, line) = (pane_fill(ui), pane_stroke(ui));
            egui::Panel::top("add_dir_bar")
                .frame(
                    egui::Frame::new()
                        .fill(fill)
                        .inner_margin(egui::Margin::symmetric(10, 8))
                        .stroke(egui::Stroke::new(1.0, line)),
                )
                .show(ui, |ui| self.add_dir_bar(ui));
        }
        // 提交页自动收起底部日志面板：改动列表要占满整窗高度，回主页 / 历史页自动恢复
        if self.page != Page::Commit {
            egui::Panel::bottom("output")
                .default_size(190.0)
                .resizable(true)
                .show(ui, |ui| {
                    self.output_panel(ui);
                    // 贴图压在日志之上，右下角贴边（0.2 不透明度，字照样读得清）
                    if let Some(tex) = self.mascot.as_ref() {
                        paint_mascot(ui, tex);
                    }
                });
        }
        let ctx = ui.ctx().clone();
        egui::CentralPanel::default().show(ui, |ui| {
            let page = self.page;
            match page {
                Page::Main => self.main_page(ui),
                Page::Commit => self.commit_page(ui),
                Page::History => self.history_page(ui),
            }
        });
        self.settings(&ctx);
        self.update_confirm_dialog(&ctx);
        self.relocate_dialog(&ctx);
        self.batch_relocate_dialog(&ctx);
        self.batch_label_dialog(&ctx);
        self.rollback_dialog(&ctx);
        self.upload_all_dialog(&ctx);
        self.file_diff_window(&ctx);
        self.file_log_window(&ctx);
        self.worklog_window(&ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testbed::{base_input, stub_app};
    use egui::{Context, ThemePreference};

    /// 嵌进 exe 的右下角贴图：解不出来只是少一枚贴图，跑起来才看得见，所以在测试里先解一次。
    /// 只验「解得开、不是全透明」，不去比具体像素
    #[test]
    fn embedded_mascot_png_decodes_to_something_visible() {
        let rgba = image::load_from_memory(include_bytes!("../mascot.png"))
            .expect("mascot.png 解不开")
            .to_rgba8();
        assert!(
            rgba.pixels().any(|p| p.0[3] > 0),
            "贴图整张都是透明的，画出来等于没画"
        );
        assert!(
            rgba.width() < rgba.height(),
            "贴图是竖构图才按高度铺得好看：实际 {}×{}",
            rgba.width(),
            rgba.height()
        );
    }

    /// 提示不再占底部一行：hint() 的话要作为一条 INFO 落进日志，日志面板里只画一次
    #[test]
    fn hint_lands_in_the_log() {
        let ctx = Context::default();
        ctx.set_theme(ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = stub_app(false);
        app.hint("已添加目录：C:\\Test");

        let last = app.output.last().expect("提示要进日志");
        assert_eq!(last.text, "已添加目录：C:\\Test");
        assert!(matches!(last.level, Level::Info), "提示按 INFO 记，不抢命令输出的颜色");

        let mut out = ctx.run_ui(base_input(), |ui| app.output_panel(ui));
        out.textures_delta.clear();
        let hits = out
            .shapes
            .iter()
            .filter_map(|item| match &item.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                _ => None,
            })
            .filter(|text| text.contains("已添加目录"))
            .count();
        assert_eq!(hits, 1, "这条提示只该在日志里出现一次：{hits}");
    }

    /// 两个 GitHub 源（最新发布 / 测试分支）不用填任何地址；自建服务端源没地址就不算可用
    #[test]
    fn only_a_configured_source_is_ready() {
        let mut app = stub_app(false);
        app.cfg.update_source = update::SOURCE_OFFICIAL.to_owned();
        app.cfg.update_server.clear();
        assert!(app.update_source_ready(), "官方源总是可用");

        app.cfg.update_source = update::SOURCE_TEST.to_owned();
        assert!(app.update_source_ready(), "测试分支也是 GitHub 源，不用填地址");

        app.cfg.update_source = update::SOURCE_CUSTOM.to_owned();
        assert!(!app.update_source_ready(), "自定义源没填地址不该去查一个空地址");
        app.cfg.update_server = "http://192.168.1.251:20700".to_owned();
        assert!(app.update_source_ready());
    }

    /// 定时那一轮：没到点 / 没开开关都不该起任务；到点了起一次并把节拍往后推
    #[test]
    fn auto_check_fires_only_on_its_own_beat() {
        // 定时检查会写配置（spawn 里顺带 persist），别碰到用户真实的 config.json
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_auto_check_test"),
        );
        let ctx = Context::default();
        let mut app = stub_app(false);
        app.cfg.update_source = update::SOURCE_CUSTOM.to_owned();
        // 指向本机一个必定连不通的端口：这一轮只会失败，不会真去联网
        app.cfg.update_server = "http://127.0.0.1:9".to_owned();
        app.next_update_check = Instant::now() - Duration::from_secs(1);

        // 开关关着：到点也不动
        let before = app.next_update_check;
        app.cfg.auto_update_check = false;
        app.tick(&ctx);
        assert!(
            !app.pool.has(Kind::CheckUpdate, usize::MAX),
            "没开开关不该自动检查"
        );
        assert_eq!(app.next_update_check, before, "没开开关不该动节拍");

        // 开关打开：到点起一轮，并把下一次推到 5 分钟后
        app.cfg.auto_update_check = true;
        app.tick(&ctx);
        assert!(app.pool.has(Kind::CheckUpdate, usize::MAX), "到点该起一轮检查");
        assert!(
            app.next_update_check > Instant::now() + Duration::from_secs(update::AUTO_CHECK_EVERY_SECS - 2),
            "节拍要往后推一个周期：还有 {:.0}s",
            app.next_update_check
                .checked_duration_since(Instant::now())
                .map_or(0.0, |d| d.as_secs_f64()),
        );
    }
}

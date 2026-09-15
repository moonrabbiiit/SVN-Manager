//! 应用本体：状态构造、每帧的定时与拖放、后台任务结果的回收，以及底部输出面板。
//!
//! `SvnApp` 的结构体定义与共享类型留在 main.rs（crate 根），这样各模块照旧写 `crate::SvnApp`。

use crate::config;
use crate::{APP_TITLE, APP_VERSION, DirView, Level, OutLine, Page, SvnApp};
use crate::{bcompare, fonts, update};

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use egui::{Align, Layout, RichText, ScrollArea, Ui, Vec2};
use crate::config::Config;
use crate::jobs::{Data, Kind, Pool};
use crate::svn::Svn;

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

    pub(crate) fn sync_views(&mut self) {
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

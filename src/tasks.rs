//! 每个 svn 动作的发起点：起后台任务（检测 / 更新 / 提交 / 日志 / 维护 / relocate）、
//! 目录的增删移，以及各页面之间的跳转。

use crate::{Level, Maintain, Page, Relocate, SvnApp};
use crate::{stats, svn, update};
use std::path::PathBuf;
use std::time::Instant;
use chrono::NaiveDate;
use crate::commit::CommitPage;
use crate::config::DirConfig;
use crate::history::{FileDiff, FileLog, HistoryPage, Zoom};
use crate::jobs::{Data, Kind};
use crate::svn::{LogEntry, LogPath};

impl SvnApp {
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

    /// 当前选的更新源是否可用：官方源不需要任何配置，自定义源得填了服务根目录地址。
    pub fn update_source_ready(&self) -> bool {
        update::is_official(&self.cfg.update_source) || !self.cfg.update_server.trim().is_empty()
    }

    /// 后台检查一次版本更新（官方 GitHub 发布或自建服务端 latest.json，与目录无关的任务）。
    /// 地址有改动时会顺带保存配置。`quiet` 给定时自动检查用：过程不写日志区，
    /// 免得每 5 分钟刷两行、把真正的 svn 记录挤出输出区。
    pub fn spawn_update_check(&mut self, quiet: bool) {
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
                let sink = if quiet { sink.muted() } else { sink };
                sink.line(format!("$ 检查版本更新：{from}"));
                match update::check_from(&source, &server) {
                    Ok(manifest) => {
                        sink.line(format!("→ 最新版本：V{}", manifest.version.trim()));
                        // 「有没有新版」按文件哈希判（要跑一次 certutil）：在后台这一趟算好，
                        // 界面那三处只读结论，别每帧重算
                        let ready = update::has_update(&manifest, crate::APP_VERSION);
                        sink.line(if ready {
                            "→ 更新源上的构建与本地不同：有可更新版本"
                        } else {
                            "→ 与本地运行的程序一致：已是最新"
                        });
                        Data::UpdateCheck {
                            ok: true,
                            message: String::new(),
                            info: Some(manifest),
                            ready,
                            quiet,
                        }
                    }
                    Err(e) => Data::UpdateCheck {
                        ok: false,
                        message: e,
                        info: None,
                        ready: false,
                        quiet,
                    },
                }
            });
    }

    /// 确认对话框里点了「开始更新」：后台下载新版本并校验，结果回来后再换上。
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
                let dest = std::env::temp_dir().join(update::STAGED_EXE);
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

    /// 下载校验完成后：把新版换到正式名字下（正在运行的旧程序改名让路），再拉起新版并退出。
    ///
    /// 全程在程序内完成，不生成任何脚本：换名只在本目录内做（同卷的原子改名），路径不经过
    /// 控制台代码页，所以程序装在中文目录、exe 改成中文名都一样能更新。旧程序文件立刻删不掉
    /// （镜像还映射着），留给新版启动时的 `update::clean_leftovers` 收。
    pub(crate) fn apply_downloaded_update(&mut self) {
        let exe = match std::env::current_exe() {
            Ok(path) => path,
            Err(e) => {
                let message = format!("无法确定程序自身路径：{e}");
                self.hint(message.clone());
                self.push(Level::Error, message);
                return;
            }
        };
        // 新版先落到程序目录：换名要求同一个卷，%TEMP% 可能挂在别的盘上
        let downloaded = std::env::temp_dir().join(update::STAGED_EXE);
        let staged = exe.with_file_name(update::STAGED_EXE);
        if let Err(e) = std::fs::copy(&downloaded, &staged) {
            let message = format!(
                "没法把新版 exe 放到程序目录（{}）：{e}；请手动下载新版本替换，或把它放到有写权限的目录再更新",
                staged.display()
            );
            self.hint(message.clone());
            self.push(Level::Error, message);
            return;
        }
        match update::swap_in_place(&exe, &staged) {
            Ok(old) => {
                self.push(
                    Level::Info,
                    format!(
                        "→ 新版本已就位：{}（旧程序改名到 {}，下次启动时清理）",
                        exe.display(),
                        old.display()
                    ),
                );
                match update::relaunch(&exe) {
                    Ok(()) => {
                        self.push(Level::Success, "程序即将退出，新版本会自动启动…");
                        std::process::exit(0);
                    }
                    Err(e) => {
                        // 起不来就整个退回去：新文件收回暂存名、旧程序回到正式名，
                        // 本进程照旧跑着，用户重试一次即可
                        let _ = std::fs::rename(&exe, &staged);
                        let _ = std::fs::rename(&old, &exe);
                        self.hint(e.clone());
                        self.push(Level::Error, e);
                    }
                }
            }
            Err(e) => {
                let _ = std::fs::remove_file(&staged);
                self.hint(e.clone());
                self.push(Level::Error, e);
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
                // 服务器可达时一次走完整棵树就够：`svn status -u --xml` 的输出同时带
                // 本地改动（wc-status）与服务器上已变更的条目（repos-status）。
                // 早先分两条命令等于把树走两遍，七万文件的目录要多等三秒。
                let (entries, out_of_date, status) = if remote == Some(true) {
                    let (entries, pending, run) = svn.status_against_server(&path);
                    if run.ok {
                        (entries, pending, run)
                    } else {
                        // 带 -u 的这次没走通（服务器中途断了 / 认证问题）：退回只读本地
                        let (local, local_run) = svn.status(&path);
                        (local, None, local_run)
                    }
                } else {
                    // 连接已经判定不通，就别再让那条走网络的命令卡到超时
                    let (local, local_run) = svn.status(&path);
                    (local, None, local_run)
                };
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
}

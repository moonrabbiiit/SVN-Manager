//! Git 后端：与 svn.rs 平行的命令行封装，给 .git 目录用。
//! 版本、状态、提交记录共用 svn.rs 里的数据结构（WcInfo / StatusEntry / LogEntry），
//! 界面层只按目录的 RepoKind 决定调哪一套、版本号怎么显示（repo::fmt_rev）。
//!
//! 与 SVN 的口径对应：
//! · 「可更新 N 项」= 本地领先远端的提交数（fetch 后 rev-list HEAD..@{upstream}）；
//! · 「更新」= `git pull --ff-only`，绝不悄悄生成合并提交；
//! · 「回退」= 目录 `git revert --no-commit <rev>..HEAD`、单文件 `git restore --source`，
//!   产物都是本地未提交改动，不提交就不碰远端；
//! · 「提交」= `git add -A` 后 `git commit`，push 由任务层按闸门决定跟不跟着做。

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::svn::{decode_bytes, format_svn_date, Item, LogEntry, LogPath, Run, StatusEntry, WcInfo};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
use crate::svn::CREATE_NO_WINDOW;

/// 仓库第一个提交没有父提交，取差异时拿这棵空树当「上一个版本」；所有仓库都认这个 SHA。
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// 忽略空白与换行差异时给 `git diff` 追加的参数（对应 svn 的 `-w --ignore-eol-style`）。
pub(crate) const DIFF_IGNORE_ARGS: [&str; 2] = ["-w", "--ignore-cr-at-eol"];

/// `git log` 的输出格式：每条记录以 \x1e 开头，头三个字段（版本 / 作者 / 日期）用 \x1f
/// 分隔，第四段是完整的提交说明（可多行），再一个 \x1f 之后是 --name-status 的路径块。
/// \x1e / \x1f 是控制字符，正常提交说明里不会出现，解析时按它们切开就够了。
const LOG_FORMAT: &str = "--pretty=format:%x1e%H%x1f%an%x1f%cI%x1f%B%x1f";

#[derive(Clone, Debug, Default)]
pub struct Git {
    pub exe: PathBuf,
}

impl Git {
    pub fn available(&self) -> bool {
        !self.exe.as_os_str().is_empty() && self.exe.is_file()
    }

    pub fn version(&self) -> Option<String> {
        if !self.available() {
            return None;
        }
        let run = self.git(&["--version"], None);
        run.ok.then(|| run.out.trim().trim_start_matches("git version ").to_owned())
    }

    fn command(&self, args: &[String], cwd: Option<&Path>) -> Command {
        let mut command = Command::new(&self.exe);
        // quotepath=false：含中文的路径按 UTF-8 原样输出，否则会被 \xxx 八进制转义包住没法认
        command.arg("-c").arg("core.quotepath=false");
        command.args(args);
        // 私有仓库认证失败时直接报错退出，不能停在终端等输入把后台任务挂死
        command.env("GIT_TERMINAL_PROMPT", "0");
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        command
    }

    fn capture(&self, args: &[String], cwd: Option<&Path>) -> Run {
        match self.command(args, cwd).output() {
            Ok(output) => Run {
                ok: output.status.success(),
                code: output.status.code().unwrap_or(-1),
                out: decode_bytes(&output.stdout),
                err: decode_bytes(&output.stderr),
            },
            Err(e) => Run::error(format!("无法启动 {}：{}", self.exe.display(), e)),
        }
    }

    /// 逐行回调执行（update / commit 等耗时命令），结构同 svn.rs 的 stream。
    fn stream(&self, args: &[String], cwd: Option<&Path>, on_line: &dyn Fn(&str)) -> Run {
        let mut child = match self
            .command(args, cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => return Run::error(format!("无法启动 {}：{}", self.exe.display(), e)),
        };
        let stderr = child.stderr.take();
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = stderr {
                let _ = pipe.read_to_end(&mut buf);
            }
            decode_bytes(&buf)
        });
        let mut all: Vec<u8> = Vec::new();
        let mut pending: Vec<u8> = Vec::new();
        let mut chunk = [0_u8; 8192];
        if let Some(mut pipe) = child.stdout.take() {
            while let Ok(size) = pipe.read(&mut chunk) {
                if size == 0 {
                    break;
                }
                all.extend_from_slice(&chunk[..size]);
                pending.extend_from_slice(&chunk[..size]);
                while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=pos).collect();
                    let text = decode_bytes(&line).trim_end_matches(['\n', '\r']).to_owned();
                    if !text.is_empty() {
                        on_line(&text);
                    }
                }
            }
        }
        if !pending.is_empty() {
            let text = decode_bytes(&pending).trim().to_owned();
            if !text.is_empty() {
                on_line(&text);
            }
        }
        let status = child.wait();
        let err = reader.join().unwrap_or_default();
        Run {
            ok: status.as_ref().map(|s| s.success()).unwrap_or(false),
            code: status.ok().and_then(|s| s.code()).unwrap_or(-1),
            out: decode_bytes(&all),
            err,
        }
    }

    fn git(&self, args: &[&str], cwd: Option<&Path>) -> Run {
        let owned: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        self.capture(&owned, cwd)
    }

    fn git_stream(&self, args: &[&str], cwd: Option<&Path>, on_line: &dyn Fn(&str)) -> Run {
        let owned: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        self.stream(&owned, cwd, on_line)
    }

    // ------------------------------------------------------------ 检测 / 基本信息

    /// 当前分支名。分离头指针（detached HEAD）时返回空——那时没法按分支推。
    pub fn branch(&self, dir: &Path) -> String {
        let run = self.git(&["rev-parse", "--abbrev-ref", "HEAD"], Some(dir));
        let name = run.out.trim().to_owned();
        if run.ok && name != "HEAD" {
            name
        } else {
            String::new()
        }
    }

    pub fn info(&self, dir: &Path) -> (Option<WcInfo>, Run) {
        // 真是 Git 仓库才往下走；失败信息原样带回（「不是 git 仓库」也在这里）
        let top = self.git(&["rev-parse", "--show-toplevel"], Some(dir));
        if !top.ok {
            return (None, top);
        }
        // 空仓库（还没打过第一个提交）rev-parse HEAD 会失败：版本留空，界面按未提交显示
        let sha = self.git(&["rev-parse", "HEAD"], Some(dir));
        let head = if sha.ok { sha.out.trim().to_owned() } else { String::new() };
        let last = self.git(&["log", "-1", "--pretty=format:%H%x1f%an%x1f%cI"], Some(dir));
        let (last_rev, last_author, last_date) = if last.ok {
            let parts: Vec<&str> = last.out.trim().split('\x1f').collect();
            let field = |index: usize| parts.get(index).copied().unwrap_or("").to_owned();
            (field(0), field(1), format_svn_date(&field(2)))
        } else {
            (String::new(), String::new(), String::new())
        };
        let remote = self.git(&["remote", "get-url", "origin"], Some(dir));
        let url = if remote.ok { remote.out.trim().to_owned() } else { String::new() };
        let branch = self.branch(dir);
        (
            Some(WcInfo {
                is_wc: true,
                url: url.clone(),
                repos_root: url,
                revision: head,
                branch,
                node_kind: "dir".to_owned(),
                last_rev,
                last_author,
                last_date,
            }),
            top,
        )
    }

    /// 仓库根目录（git rev-parse --show-toplevel）。读不到时给 None。
    pub fn toplevel(&self, dir: &Path) -> Option<PathBuf> {
        let run = self.git(&["rev-parse", "--show-toplevel"], Some(dir));
        run.ok.then(|| PathBuf::from(run.out.trim()))
    }

    /// 上游顶端（远端分支 tip）最后一次提交的（版本、作者、时间），当「服务器最后提交」用。
    /// `rev` 为空或读不到时给空串——调用方退回本地 HEAD 的信息。
    pub fn tip_meta(&self, dir: &Path, rev: &str) -> (String, String, String) {
        if rev.is_empty() {
            return (String::new(), String::new(), String::new());
        }
        let last = self.git(&["log", "-1", "--pretty=format:%H%x1f%an%x1f%cI", rev], Some(dir));
        if last.ok {
            let parts: Vec<&str> = last.out.trim().split('\x1f').collect();
            let field = |index: usize| parts.get(index).copied().unwrap_or("").to_owned();
            (field(0), field(1), format_svn_date(&field(2)))
        } else {
            (String::new(), String::new(), String::new())
        }
    }

    // ------------------------------------------------------------ 状态 / 可更新数

    pub fn status(&self, dir: &Path) -> (Vec<StatusEntry>, Run) {
        // --untracked-files=all：未版本化目录里的每个文件单独列出来，跟 svn status 的递归口径一致
        let run = self.git(
            &["status", "--porcelain=v2", "--untracked-files=all"],
            Some(dir),
        );
        (
            if run.ok { parse_status(&run.out, dir) } else { Vec::new() },
            run,
        )
    }

    /// 一次读出本地状态 + 「落后远端多少个提交」，附带远端上游分支的 tip SHA
    /// （当作 svn 检测里的 remote_rev / 服务器最后提交用）。fetch 失败（断网 / 凭据问题）
    /// 时可更新数返回 None，调用方不能据此断定「已是最新」——口径同 svn status -u。
    /// 没配远端、或分支还没建立上游（没推过），都按落后 0 个提交算，tip 给空串。
    pub fn status_against_server(
        &self,
        dir: &Path,
    ) -> (Vec<StatusEntry>, Option<usize>, String, Run) {
        let (entries, run) = self.status(dir);
        if !run.ok {
            return (Vec::new(), None, String::new(), run);
        }
        let remotes = self.git(&["remote"], Some(dir));
        if !remotes.ok || remotes.out.trim().is_empty() {
            return (
                entries,
                Some(0),
                String::new(),
                Run { ok: true, code: 0, out: String::new(), err: String::new() },
            );
        }
        let fetch = self.git(&["fetch", "--quiet"], Some(dir));
        if !fetch.ok {
            return (entries, None, String::new(), fetch);
        }
        let upstream = self.git(&["rev-parse", "--verify", "--quiet", "@{upstream}"], Some(dir));
        if !upstream.ok {
            return (
                entries,
                Some(0),
                String::new(),
                Run { ok: true, code: 0, out: String::new(), err: String::new() },
            );
        }
        let tip = upstream.out.trim().to_owned();
        let count = self.git(&["rev-list", "--count", &format!("HEAD..{tip}")], Some(dir));
        if !count.ok {
            return (entries, None, tip, count);
        }
        (entries, count.out.trim().parse::<usize>().ok(), tip, count)
    }

    // ------------------------------------------------------------ 提交记录

    /// 全部提交记录，新的在前。`limit` > 0 时限制条数；`author` 非空时只看这个人的
    /// 提交（--author 按正则匹配，用户名先做转义，免得带正则符号的名字把匹配搅乱）。
    pub fn log(&self, dir: &Path, limit: i64, author: &str) -> (Vec<LogEntry>, Run) {
        self.log_common(dir, "", "", limit, author, None)
    }

    /// 带日期区间的提交记录（「提交记录」页按区间查询用）。`since` / `until` 为空表示不限那一头，
    /// 格式就是 git 自己认的日期写法（如 `2026-09-11`）。
    pub fn log_in(
        &self,
        dir: &Path,
        since: &str,
        until: &str,
        limit: i64,
        author: &str,
    ) -> (Vec<LogEntry>, Run) {
        self.log_common(dir, since, until, limit, author, None)
    }

    /// 只看某个仓库相对路径的提交记录（单个文件 / 目录自己的历史）。
    /// git 按 pathspec 过滤，本地文件被改名 / 删除了历史照样查得到；
    /// `:(top)` 让 pathspec 按仓库根匹配，目录是仓库的子目录时照样对得上。
    pub fn log_path(
        &self,
        dir: &Path,
        relpath: &str,
        limit: i64,
        author: &str,
    ) -> (Vec<LogEntry>, Run) {
        self.log_common(dir, "", "", limit, author, Some(relpath))
    }

    fn log_common(
        &self,
        dir: &Path,
        since: &str,
        until: &str,
        limit: i64,
        author: &str,
        pathspec: Option<&str>,
    ) -> (Vec<LogEntry>, Run) {
        let mut args: Vec<String> = vec!["log".to_owned(), "--name-status".to_owned()];
        if !since.is_empty() {
            args.push(format!("--since={since}"));
        }
        if !until.is_empty() {
            args.push(format!("--until={until}"));
        }
        if limit > 0 {
            args.push("-n".to_owned());
            args.push(limit.to_string());
        }
        if !author.is_empty() {
            args.push(format!("--author={}", regex::escape(author)));
        }
        args.push(LOG_FORMAT.to_owned());
        if let Some(rel) = pathspec {
            args.push("--".to_owned());
            args.push(format!(":(top){rel}"));
        }
        let run = self.capture(&args, Some(dir));
        (if run.ok { parse_log(&run.out) } else { Vec::new() }, run)
    }

    /// 本机这个仓库配置的提交人（`git config user.name`），先看仓库级再看全局。
    pub fn auth_user(&self, dir: &Path) -> Option<String> {
        let run = self.git(&["config", "user.name"], Some(dir));
        let name = run.out.trim().to_owned();
        if run.ok && !name.is_empty() {
            return Some(name);
        }
        let global = self.git(&["config", "--global", "user.name"], None);
        let name = global.out.trim().to_owned();
        (global.ok && !name.is_empty()).then_some(name)
    }

    // ------------------------------------------------------------ 更新 / 差异

    /// 只做快进。本地和远端各有新提交时直接失败并把原因报出来，
    /// 绝不悄悄生成合并提交——SVN 的 update 也从不在本地制造版本。
    pub fn update(&self, dir: &Path, on_line: &dyn Fn(&str)) -> Run {
        self.git_stream(&["pull", "--ff-only"], Some(dir), on_line)
    }

    /// 单个文件的本次修改内容（工作区 vs HEAD，含已暂存的部分）。
    pub fn diff(&self, path: &Path, ignore_white: bool) -> Run {
        let mut args: Vec<String> = vec![
            "diff".to_owned(),
            "HEAD".to_owned(),
            "--no-ext-diff".to_owned(),
        ];
        if ignore_white {
            args.extend(DIFF_IGNORE_ARGS.iter().map(|a| (*a).to_owned()));
        }
        args.push("--".to_owned());
        args.push(path.to_string_lossy().into_owned());
        self.capture(&args, path.parent())
    }

    /// 某次提交对这个路径的逐行差异（和它的上一个提交比）。`relpath` 是仓库相对路径，
    /// 本地没有该文件 / 已被改名删除也能取到；仓库第一个提交拿空树当「上一个版本」。
    /// pathspec 加 `:(top)` 前缀按仓库根匹配：目录加的是仓库的子目录时也能对上号。
    pub fn diff_rev(&self, dir: &Path, revision: &str, relpath: &str, ignore_white: bool) -> Run {
        let parent = self.git(
            &["rev-parse", "--verify", "--quiet", &format!("{revision}~1")],
            Some(dir),
        );
        let base = if parent.ok { parent.out.trim().to_owned() } else { EMPTY_TREE.to_owned() };
        let mut args: Vec<String> = vec![
            "diff".to_owned(),
            base,
            revision.to_owned(),
            "--no-ext-diff".to_owned(),
        ];
        if ignore_white {
            args.extend(DIFF_IGNORE_ARGS.iter().map(|a| (*a).to_owned()));
        }
        args.push("--".to_owned());
        args.push(format!(":(top){relpath}"));
        self.capture(&args, Some(dir))
    }

    /// 取 HEAD 里该文件的原始内容写到 `to`，供 Beyond Compare 对比。读的是本地对象库，不走网络。
    pub fn cat_base(&self, path: &Path, to: &Path) -> Result<(), String> {
        let parent = path.parent().unwrap_or(path);
        let top = self.git(&["rev-parse", "--show-toplevel"], Some(parent));
        if !top.ok {
            return Err(top.summary());
        }
        let root = PathBuf::from(top.out.trim());
        let rel = path
            .strip_prefix(&root)
            .map_err(|_| format!("{} 不在 Git 仓库 {} 里", path.display(), root.display()))?
            .to_string_lossy()
            .replace('\\', "/");
        // 写原始字节：show 输出的就是对象库里的原样内容，不能过 decode_bytes（二进制会坏）
        let output = self
            .command(&["show".to_owned(), format!("HEAD:{rel}")], Some(&root))
            .output()
            .map_err(|e| format!("无法启动 {}：{}", self.exe.display(), e))?;
        if !output.status.success() {
            return Err(decode_bytes(&output.stderr).trim().to_owned());
        }
        fs::write(to, &output.stdout).map_err(|e| e.to_string())
    }

    // ------------------------------------------------------------ 提交侧

    pub fn add(&self, dir: &Path, targets: &[String], on_line: &dyn Fn(&str)) -> Run {
        let mut args: Vec<String> = vec!["add".to_owned(), "-A".to_owned(), "--".to_owned()];
        args.extend(targets.iter().cloned());
        self.stream(&args, Some(dir), on_line)
    }

    /// 记录删除。Git 的状态里不会出现「已丢失」（工作区删了就是 D，可直接提交），
    /// 这个方法只服务于提交页里人工勾选删除的入口。
    pub fn delete(&self, dir: &Path, targets: &[String], on_line: &dyn Fn(&str)) -> Run {
        let mut args: Vec<String> = vec!["rm".to_owned(), "-r".to_owned(), "--".to_owned()];
        args.extend(targets.iter().cloned());
        self.stream(&args, Some(dir), on_line)
    }

    /// 暂存 targets 再提交。先 `git add -A --`（把勾选项的增 / 改 / 删都 staged），
    /// 再 `git commit -- <targets>` 只提交这些路径，用户在别处自己 staged 的东西不会带上
    /// ——口径对齐 svn `commit --depth empty` 的「只提交点名的路径」。
    pub fn commit(
        &self,
        dir: &Path,
        targets: &[String],
        message: &str,
        on_line: &dyn Fn(&str),
    ) -> Run {
        let staged = self.add(dir, targets, on_line);
        if !staged.ok {
            return staged;
        }
        let message_file = crate::svn::temp_file("repo_manager_log");
        if fs::write(&message_file, message.as_bytes()).is_err() {
            return Run::error("无法写入提交说明临时文件");
        }
        let mut args: Vec<String> = vec![
            "commit".to_owned(),
            "--file".to_owned(),
            message_file.to_string_lossy().into_owned(),
            "--".to_owned(),
        ];
        args.extend(targets.iter().cloned());
        let run = self.stream(&args, Some(dir), on_line);
        let _ = fs::remove_file(&message_file);
        run
    }

    /// 推到远端。分支还没设上游时先 `-u origin <分支>` 建立跟踪（首次提交后要推）。
    pub fn push(&self, dir: &Path, on_line: &dyn Fn(&str)) -> Run {
        let upstream = self.git(
            &[
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{upstream}",
            ],
            Some(dir),
        );
        if upstream.ok {
            self.git_stream(&["push"], Some(dir), on_line)
        } else {
            let branch = self.branch(dir);
            if branch.is_empty() {
                return Run::error("读不到当前分支名（分离头指针状态），无法推送");
            }
            let args = vec![
                "push".to_owned(),
                "-u".to_owned(),
                "origin".to_owned(),
                branch,
            ];
            self.stream(&args, Some(dir), on_line)
        }
    }

    // ------------------------------------------------------------ 回退 / 撤销

    /// 版本回退：把 `target`（None = 整个仓库，Some = 单个文件的绝对路径）倒回
    /// `revision` 那个提交时的样子。产物是本地未提交改动，不提交就不碰远端，
    /// 不想要了用「撤销修改」还原——口径同 svn 的反向 merge。
    /// 整个仓库：`git revert --no-commit <rev>..HEAD` 把 rev 之后的每个提交逐个反向应用，
    /// rev 之后没有提交时范围是空的，原样结束。撞上冲突会中途停下并留下半成品，
    /// 错误里提示用 `git revert --abort` 整体撤掉。
    /// 单个文件：revert 没法只作用于一个路径，改用 `git restore --source`；
    /// 该文件在 rev 时还不存在 → 回退等于删掉它，走 `git rm -f`。
    pub fn merge_back(
        &self,
        dir: &Path,
        target: Option<&Path>,
        revision: &str,
        on_line: &dyn Fn(&str),
    ) -> Run {
        match target {
            None => {
                let range = format!("{revision}..HEAD");
                let mut run = self.git_stream(&["revert", "--no-commit", &range], Some(dir), on_line);
                if !run.ok && !run.err.contains("--abort") {
                    run.err
                        .push_str("\n（回退中途撞上冲突时，可在终端运行 git revert --abort 撤掉半成品）");
                }
                run
            }
            Some(path) => {
                let rel = path
                    .strip_prefix(dir)
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| path.to_string_lossy().into_owned());
                let exists = self.git(&["cat-file", "-e", &format!("{revision}:{rel}")], Some(dir));
                if exists.ok {
                    self.git_stream(
                        &[
                            "restore",
                            "--staged",
                            "--worktree",
                            &format!("--source={revision}"),
                            "--",
                            &rel,
                        ],
                        Some(dir),
                        on_line,
                    )
                } else {
                    let args = vec![
                        "rm".to_owned(),
                        "-f".to_owned(),
                        "--".to_owned(),
                        rel,
                    ];
                    self.stream(&args, Some(dir), on_line)
                }
            }
        }
    }

    /// 撤销全部本地改动：已暂存的先退回未暂存（git reset），再把工作区里被跟踪的
    /// 文件恢复成 HEAD。未版本化（??）文件不动——与 svn revert --recursive 口径一致。
    pub fn revert_all(&self, dir: &Path, on_line: &dyn Fn(&str)) -> Run {
        let reset = self.git_stream(&["reset"], Some(dir), on_line);
        if !reset.ok {
            return reset;
        }
        self.git_stream(&["restore", "."], Some(dir), on_line)
    }
}

// ------------------------------------------------------------------------ 输出解析

/// 跳过 `n` 个空格分隔的字段，返回 (前 n 个字段的原文, 剩下的整段)。
/// porcelain v2 里路径是最后一个字段、本身可以带空格，所以不能直接按空白切开。
fn split_fields(line: &str, n: usize) -> (&str, &str) {
    let bytes = line.as_bytes();
    let mut idx = 0;
    for _ in 0..n {
        match bytes[idx..].iter().position(|b| *b == b' ') {
            Some(pos) => idx += pos + 1,
            None => return (line, ""),
        }
    }
    (&line[..idx - 1], &line[idx..])
}

/// porcelain v2 的 XY 两列（已暂存 / 工作区）翻译成 svn 口径的状态。
/// 优先取工作区列：git add 之前工作区是什么就报什么，跟 svn status 只看工作区一致；
/// 带 U（未合并）一律按冲突算，必须人工处理。
fn item_from_xy(xy: &str) -> Item {
    if xy.contains('U') {
        return Item::Conflict;
    }
    let pick = xy
        .chars()
        .nth(1)
        .filter(|c| *c != '.')
        .or_else(|| xy.chars().next())
        .unwrap_or('.');
    match pick {
        'M' | 'T' => Item::Modified,
        'A' => Item::Added,
        'D' => Item::Deleted,
        'R' | 'C' => Item::Replaced,
        '?' => Item::Unversioned,
        '!' => Item::Missing,
        _ => Item::Normal,
    }
}

fn make_entry(base: &Path, path: &str, item: Item) -> StatusEntry {
    let full = base.join(path.trim()).display().to_string();
    let name = Path::new(&full)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| full.clone());
    StatusEntry {
        path: full,
        name,
        item,
        // svn 有「属性修改」这一路，git 没有，恒为 none
        props: "none".to_owned(),
        // 与 svn 相同：能随上传走的默认勾上
        checked: item.uploadable(),
    }
}

/// `git status --porcelain=v2 --untracked-files=all` 的输出。
/// `1` 普通改动、`2` 改名（拆成「旧路径删除 + 新路径新增」两条，跟 svn 的口径一致）、
/// `u` 未合并（冲突）、`?` 未版本化；`#` 开头是表头，跳过。
pub fn parse_status(text: &str, base: &Path) -> Vec<StatusEntry> {
    let mut entries = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (kind, rest) = split_fields(line, 1);
        match kind {
            "1" => {
                // 1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>
                let (fields, path) = split_fields(rest, 7);
                if path.is_empty() {
                    continue;
                }
                let xy = fields.split(' ').next().unwrap_or("");
                entries.push(make_entry(base, path, item_from_xy(xy)));
            }
            "2" => {
                // 2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <新路径>\t<原路径>
                let (fields, tail) = split_fields(rest, 8);
                let xy = fields.split(' ').next().unwrap_or("");
                let (new_path, orig_path) = tail.split_once('\t').unwrap_or((tail, ""));
                if xy.starts_with('R') && !orig_path.is_empty() {
                    entries.push(make_entry(base, orig_path, Item::Deleted));
                }
                let new_item = match item_from_xy(xy) {
                    Item::Replaced => Item::Added,
                    other => other,
                };
                entries.push(make_entry(base, new_path, new_item));
            }
            "u" => {
                // u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>
                let (_fields, path) = split_fields(rest, 9);
                if path.is_empty() {
                    continue;
                }
                entries.push(make_entry(base, path, Item::Conflict));
            }
            "?" => entries.push(make_entry(base, rest, Item::Unversioned)),
            "!" => entries.push(make_entry(base, rest, Item::Missing)),
            _ => {}
        }
    }
    entries.sort_by(|a, b| a.item.text().cmp(b.item.text()).then(a.path.cmp(&b.path)));
    entries
}

/// [`LOG_FORMAT`] 的输出：\x1e 分记录，\x1f 分字段；说明放在第四段（可多行），
/// 最后一段是 --name-status 的路径块。
pub fn parse_log(text: &str) -> Vec<LogEntry> {
    let mut logs = Vec::new();
    for record in text.split('\x1e') {
        if record.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = record.split('\x1f').collect();
        if parts.len() < 4 {
            continue;
        }
        // 说明是中间整段（正常只有一段；说明里万一混进 \x1f 也归并回来），路径块是最后一段
        let message = parts[3..parts.len() - 1].join("\x1f");
        let paths = parse_name_status(parts[parts.len() - 1]);
        logs.push(LogEntry {
            revision: parts[0].trim().to_owned(),
            author: parts[1].trim().to_owned(),
            date: format_svn_date(parts[2].trim()),
            message: message.trim().to_owned(),
            paths,
        });
    }
    logs
}

fn parse_name_status(block: &str) -> Vec<LogPath> {
    let mut paths = Vec::new();
    for line in block.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        // M / A / D / R / C / T，与 svn log 的 action 字母同一套口径
        let action = fields.next().unwrap_or("").chars().next().unwrap_or(' ');
        // 改名行是「R100\t新路径\t旧路径」，界面上只看新路径
        let path = fields.next().unwrap_or("").to_owned();
        if path.is_empty() {
            continue;
        }
        paths.push(LogPath {
            action,
            // git 的记录粒度是文件；kind=dir 的特殊显示（加粗 / 按目录回退）不会触发
            kind: "file".to_owned(),
            path,
        });
    }
    paths
}

// ------------------------------------------------------------------------ 自动寻找 git.exe

fn registry_install_path(key: &str) -> Option<PathBuf> {
    let output = Command::new("reg")
        .args(["query", key, "/v", "InstallPath"])
        .creation_flags_compat()
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = decode_bytes(&output.stdout);
    for line in text.lines() {
        if let Some(pos) = line.find("REG_SZ") {
            let value = line[pos + "REG_SZ".len()..].trim();
            if !value.is_empty() {
                return Some(PathBuf::from(value));
            }
        }
    }
    None
}

/// reg query 也要藏掉控制台窗口；非 Windows 平台没有这个概念，直接原样返回。
trait CreationFlagsCompat {
    fn creation_flags_compat(&mut self) -> &mut Command;
}

#[cfg(windows)]
impl CreationFlagsCompat for Command {
    fn creation_flags_compat(&mut self) -> &mut Command {
        self.creation_flags(crate::svn::CREATE_NO_WINDOW)
    }
}

#[cfg(not(windows))]
impl CreationFlagsCompat for Command {
    fn creation_flags_compat(&mut self) -> &mut Command {
        self
    }
}

pub fn candidates() -> Vec<PathBuf> {
    let mut list: Vec<PathBuf> = Vec::new();
    // PATH 里的 git
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            crate::svn::add_path(&mut list, dir.join("git.exe"));
        }
    }
    // Git for Windows 安装时写的注册表键
    for key in [
        r"HKLM\SOFTWARE\GitForWindows",
        r"HKLM\SOFTWARE\WOW6432Node\GitForWindows",
        r"HKCU\SOFTWARE\GitForWindows",
    ] {
        if let Some(path) = registry_install_path(key) {
            crate::svn::add_path(&mut list, path.join("cmd").join("git.exe"));
            crate::svn::add_path(&mut list, path.join("bin").join("git.exe"));
        }
    }
    // 标准安装位置：Program Files / (x86) / 用户级安装
    let mut bases: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("ProgramFiles") {
        bases.push(PathBuf::from(p));
    }
    if let Some(p) = std::env::var_os("ProgramFiles(x86)") {
        bases.push(PathBuf::from(p));
    }
    if let Some(p) = std::env::var_os("LocalAppData") {
        bases.push(PathBuf::from(p).join("Programs"));
    }
    for base in bases {
        crate::svn::add_path(&mut list, base.join("Git").join("cmd").join("git.exe"));
    }
    // scoop / chocolatey
    if let Some(home) = std::env::var_os("USERPROFILE") {
        crate::svn::add_path(
            &mut list,
            PathBuf::from(home).join("scoop").join("shims").join("git.exe"),
        );
    }
    crate::svn::add_path(&mut list, PathBuf::from(r"C:\ProgramData\chocolatey\bin\git.exe"));
    list
}

/// 依次试候选路径，第一个能跑出版本号的就是它。
pub fn detect(list: Vec<PathBuf>) -> Option<(PathBuf, String)> {
    for path in list {
        if !path.is_file() {
            continue;
        }
        let git = Git { exe: path.clone() };
        if let Some(version) = git.version() {
            return Some((path, version));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_v2_states_map_to_svn_items() {
        let base = Path::new("D:\\wc");
        let text = concat!(
            "# branch.oid 1234567890\n",
            "1 MM . 000000 1111111 1111111 aaa aaa src/main.rs\n",
            "1 A. . 000000 0000000 2222222 0000000 bbb readme.md\n",
            "1 .D . 3333333 3333333 0000000 ccc 0000000 gone.txt\n",
            "2 R. . 000000 0000000 4444444 0000000 ddd R100 新目录/新名字.txt\t旧目录/旧名字.txt\n",
            "u UU . 000000 000000 000000 5555555 e1 e2 e3 冲突 文件.txt\n",
            "? 未版本化 新目录/inner file.txt\n",
        );
        let entries = parse_status(text, base);
        let find = |name: &str| {
            entries
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("没有找到 {name}：{entries:?}"))
        };
        assert_eq!(find("main.rs").item, Item::Modified);
        assert_eq!(find("readme.md").item, Item::Added);
        assert_eq!(find("gone.txt").item, Item::Deleted);
        // 改名拆成两条：旧路径删除、新路径新增
        assert_eq!(find("旧名字.txt").item, Item::Deleted);
        assert_eq!(find("新名字.txt").item, Item::Added);
        // 冲突必须人工处理，不随上传走
        let conflict = find("冲突 文件.txt");
        assert_eq!(conflict.item, Item::Conflict);
        assert!(conflict.item.blocked());
        assert!(!conflict.checked);
        // 未版本化要走 add，和 svn 一样默认勾上随上传走
        let untracked = find("inner file.txt");
        assert_eq!(untracked.item, Item::Unversioned);
        assert!(untracked.checked);
        assert!(untracked.path.ends_with("inner file.txt"));
        assert!(untracked.path.starts_with("D:\\wc"));
    }

    #[test]
    fn worktree_column_wins_over_index() {
        // 工作区列优先：.M 和 M. 都按修改报（git add 之前工作区是什么就报什么）
        assert_eq!(item_from_xy(".M"), Item::Modified);
        assert_eq!(item_from_xy("M."), Item::Modified);
        assert_eq!(item_from_xy("A."), Item::Added);
        assert_eq!(item_from_xy(".D"), Item::Deleted);
        assert_eq!(item_from_xy("D."), Item::Deleted);
        assert_eq!(item_from_xy("T."), Item::Modified);
    }

    #[test]
    fn log_records_parse_with_multiline_message_and_renames() {
        let text = concat!(
            "\u{1e}abc123\u{1f}张三\u{1f}2026-10-01T10:00:00+08:00\u{1f}标题第一行\n\n正文第二行\n\u{1f}\n",
            "M\tsrc/a.rs\n",
            "R100\tsrc/new.rs\tsrc/old.rs\n",
            "\u{1e}def456\u{1f}李四\u{1f}2026-10-02T11:00:00+08:00\u{1f}只有标题\n\u{1f}\n",
            "D\tsrc/gone.rs",
        );
        let logs = parse_log(text);
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].revision, "abc123");
        assert_eq!(logs[0].author, "张三");
        assert!(logs[0].message.contains("标题第一行"));
        assert!(logs[0].message.contains("正文第二行"));
        assert!(!logs[0].message.contains("src/a.rs"), "说明里不能混进路径块");
        assert!(!logs[0].date.is_empty());
        assert_eq!(logs[0].paths.len(), 2);
        assert_eq!(logs[0].paths[0].action, 'M');
        assert_eq!(logs[0].paths[0].path, "src/a.rs");
        // 改名记录取新路径
        assert_eq!(logs[0].paths[1].action, 'R');
        assert_eq!(logs[0].paths[1].path, "src/new.rs");
        assert_eq!(logs[1].revision, "def456");
        assert_eq!(logs[1].paths[0].action, 'D');
        assert_eq!(logs[1].paths[0].path, "src/gone.rs");
    }

    #[test]
    fn split_fields_keeps_spaces_in_the_tail() {
        assert_eq!(split_fields("1 MM . a b c d e 路径 有 空格.txt", 8).1, "路径 有 空格.txt");
        assert_eq!(split_fields("?? file.txt", 1).1, "file.txt");
        // 字段不够时返回原串和空尾，调用方按空路径跳过
        assert_eq!(split_fields("1 MM", 7).1, "");
    }

    /// 真跑一遍完整链路：临时裸仓库当 origin，克隆出两个工作副本后走
    /// 提交 / 推送 / 状态 / 可更新数 / 记录 / 差异 / 单文件回退 / 撤销。
    /// 本机没有 git.exe 就跳过（输出格式类的单测在上面兜底）。
    #[test]
    fn full_chain_against_a_bare_origin() {
        let Some((exe, _)) = detect(candidates()) else {
            eprintln!("本机没有 git.exe，跳过整链集成测试");
            return;
        };
        let git = Git { exe };
        let root = std::env::temp_dir().join(format!("repo_manager_git_chain_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("建临时目录");
        let origin = root.join("origin.git");
        let wc = root.join("wc");
        let other = root.join("other");
        // 命令行里传正斜杠，git 在 Windows 上两边都认
        let s = |p: &Path| p.to_string_lossy().replace('\\', "/");

        assert!(git.git(&["init", "--bare", &s(&origin)], None).ok, "init bare");
        // 克隆空仓库 git 会给警告，退出码仍是 0
        assert!(git.git(&["clone", &s(&origin), &s(&wc)], None).ok, "clone");
        // 提交身份只写在仓库级，不动测试机的全局配置；
        // autocrlf 一并关掉：文件内容字节级往返一致，回退断言才不受换行转换影响
        assert!(git.git(&["config", "user.name", "测试工"], Some(&wc)).ok);
        assert!(git.git(&["config", "user.email", "dev@example.com"], Some(&wc)).ok);
        assert!(git.git(&["config", "core.autocrlf", "false"], Some(&wc)).ok);

        // 首次提交 + 推送：还没上游，push 会退回 -u origin <分支>
        let first = wc.join("a.txt");
        fs::write(&first, "第一版\n").unwrap();
        let run = git.commit(&wc, &[".".to_owned()], "first", &|_| {});
        assert!(run.ok, "first commit: {}", run.summary());
        let run = git.push(&wc, &|_| {});
        assert!(run.ok, "first push: {}", run.summary());

        // info：远端地址 + 完整 sha
        let (info, run) = git.info(&wc);
        assert!(run.ok, "info: {}", run.summary());
        let info = info.expect("克隆出来的目录该认成 Git 工作副本");
        assert_eq!(info.revision.len(), 40, "git 版本号存完整 sha：{}", info.revision);
        assert!(info.url.contains("origin.git"), "url 取 origin：{}", info.url);

        // 另一个人在别处提交并推送：本地的「可更新」该是 1
        assert!(git.git(&["clone", &s(&origin), &s(&other)], None).ok, "clone other");
        assert!(git.git(&["config", "user.name", "别人"], Some(&other)).ok);
        assert!(git.git(&["config", "user.email", "other@example.com"], Some(&other)).ok);
        assert!(git.git(&["config", "core.autocrlf", "false"], Some(&other)).ok);
        fs::write(other.join("b.txt"), "来自远端\n").unwrap();
        // 只点名 b.txt：克隆时按全局 autocrlf 检出的 a.txt 在关掉 autocrlf 后会被视为
        // 已修改，但那是本地行尾观感，不该混进远端这次提交
        let run = git.commit(&other, &["b.txt".to_owned()], "remote change", &|_| {});
        assert!(run.ok, "remote commit: {}", run.summary());
        let run = git.push(&other, &|_| {});
        assert!(run.ok, "remote push: {}", run.summary());

        let (entries, pending, tip, probe) = git.status_against_server(&wc);
        assert!(probe.ok, "probe: {}", probe.summary());
        assert_eq!(pending, Some(1), "远端多一个提交");
        assert_eq!(tip.len(), 40, "tip 是远端分支头 sha");
        assert!(entries.is_empty(), "本地没改动时不该有状态条目：{entries:?}");

        // 远端有人推了提交：先更新到最新再改（口径同上传闸门——落后时不许直接提交）
        assert!(git.update(&wc, &|_| {}).ok, "pull --ff-only");
        let (_, pending, _, probe) = git.status_against_server(&wc);
        assert!(probe.ok);
        assert_eq!(pending, Some(0), "更新完就齐了");

        // 本地改一行：状态报修改，diff 里有新内容
        fs::write(&first, "第一版\n本地改过\n").unwrap();
        let (entries, run) = git.status(&wc);
        assert!(run.ok);
        assert!(
            entries.iter().any(|e| e.name == "a.txt" && e.item == Item::Modified),
            "该报出 a.txt 的修改：{entries:?}"
        );
        let diff = git.diff(&first, false);
        assert!(diff.ok && diff.out.contains("本地改过"), "diff：{}", diff.summary());

        // 提交 + push（上游已在首次 push 建立）
        let run = git.commit(&wc, &["a.txt".to_owned()], "local change", &|_| {});
        assert!(run.ok, "local commit: {}", run.summary());
        let run = git.push(&wc, &|_| {});
        assert!(run.ok, "local push: {}", run.summary());
        let (_, pending, _, probe) = git.status_against_server(&wc);
        assert!(probe.ok);
        assert_eq!(pending, Some(0), "推完就齐了");

        // 记录：全仓库与单文件（新的在前）
        let (logs, run) = git.log_in(&wc, "", "", 0, "");
        assert!(run.ok && logs.len() >= 3, "整仓库记录 {} 条：{}", logs.len(), run.summary());
        let (logs, run) = git.log_path(&wc, "a.txt", 0, "");
        assert!(run.ok && logs.len() == 2, "动过 a.txt 的记录：{}", run.summary());
        let first_sha = logs.last().map(|e| e.revision.clone()).unwrap_or_default();
        assert_eq!(first_sha, info.revision, "最早动 a.txt 的就是首次提交");

        // 首次提交对 a.txt 的逐行差异：没有父提交，拿空树兜底也要出 + 行
        let diff = git.diff_rev(&wc, &first_sha, "a.txt", false);
        assert!(diff.ok && diff.out.contains("+第一版"), "首提交差异：{}", diff.summary());

        // 单文件回退到第一版：产物是未提交改动，文件内容真的回去了
        let run = git.merge_back(&wc, Some(first.as_path()), &first_sha, &|_| {});
        assert!(run.ok, "restore file: {}", run.summary());
        assert_eq!(fs::read_to_string(&first).unwrap(), "第一版\n", "回退后回到第一版内容");

        // 撤销全部改动，工作区回到 HEAD
        assert!(git.revert_all(&wc, &|_| {}).ok, "revert all");
        let (entries, run) = git.status(&wc);
        assert!(run.ok && entries.is_empty(), "撤销后应当干净：{entries:?}");

        let _ = fs::remove_dir_all(&root);
    }
}

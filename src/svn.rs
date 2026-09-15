use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use encoding_rs::GBK;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// svn 输出在中文 Windows 上可能是 GBK，`--xml` 恒为 UTF-8，两种都要能解。
pub fn decode_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => GBK.decode(bytes).0.into_owned(),
    }
}

fn temp_file(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("{}_{}_{}", prefix, std::process::id(), nanos))
}

// ------------------------------------------------------------------------ 运行结果

pub struct Run {
    pub ok: bool,
    pub code: i32,
    pub out: String,
    pub err: String,
}

impl Run {
    fn new(ok: bool, code: i32, out: String, err: String) -> Self {
        Self { ok, code, out, err }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::new(false, -1, String::new(), message.into())
    }

    /// 供界面展示的错误摘要。
    pub fn summary(&self) -> String {
        let err = self.err.trim();
        if !err.is_empty() {
            return err.to_owned();
        }
        let out = self.out.trim();
        if !out.is_empty() {
            return out.to_owned();
        }
        format!("命令退出码 {}", self.code)
    }
}

// ------------------------------------------------------------------------ 数据结构

#[derive(Clone, Debug, Default)]
pub struct WcInfo {
    pub is_wc: bool,
    pub url: String,
    pub repos_root: String,
    pub revision: String,
    pub node_kind: String,
    pub last_rev: String,
    pub last_author: String,
    pub last_date: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    Added,
    Modified,
    Deleted,
    Replaced,
    Unversioned,
    Missing,
    Conflict,
    Ignored,
    External,
    Incomplete,
    Normal,
    Other,
}

impl Item {
    pub fn from_svn(raw: &str) -> Self {
        match raw {
            "added" => Self::Added,
            "modified" => Self::Modified,
            "deleted" => Self::Deleted,
            "replaced" => Self::Replaced,
            "unversioned" => Self::Unversioned,
            "missing" => Self::Missing,
            "conflicted" => Self::Conflict,
            "ignored" => Self::Ignored,
            "external" => Self::External,
            "incomplete" => Self::Incomplete,
            "normal" => Self::Normal,
            _ => Self::Other,
        }
    }

    pub fn mark(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Deleted => 'D',
            Self::Replaced => 'R',
            Self::Unversioned => '?',
            Self::Missing => '!',
            Self::Conflict => 'C',
            Self::Ignored => 'I',
            Self::External => 'X',
            Self::Incomplete => '~',
            Self::Normal | Self::Other => ' ',
        }
    }

    pub fn text(self) -> &'static str {
        match self {
            Self::Added => "新增",
            Self::Modified => "修改",
            Self::Deleted => "删除",
            Self::Replaced => "替换",
            Self::Unversioned => "未版本化",
            Self::Missing => "已丢失",
            Self::Conflict => "冲突",
            Self::Ignored => "已忽略",
            Self::External => "外部引用",
            Self::Incomplete => "不完整",
            Self::Normal => "正常",
            Self::Other => "未知",
        }
    }

    /// 可直接提交的状态。
    pub fn committable(self) -> bool {
        matches!(self, Self::Added | Self::Modified | Self::Deleted | Self::Replaced)
    }

    /// 需要先 `svn add` 才能提交。
    pub fn needs_add(self) -> bool {
        matches!(self, Self::Unversioned)
    }

    /// 需要 `svn delete` 记录删除。
    pub fn needs_delete(self) -> bool {
        matches!(self, Self::Missing)
    }

    /// 「全部上传」的口径：可直接提交，或提交前补一次 add / delete 就能一起提交。
    /// 冲突、不完整、已忽略、外部引用都不算——它们必须先人工处理，不能自动带上服务器。
    pub fn uploadable(self) -> bool {
        self.committable() || self.needs_add() || self.needs_delete()
    }

    pub fn color(self) -> (u8, u8, u8) {
        match self {
            Self::Added => (60, 190, 110),
            Self::Modified => (90, 160, 240),
            Self::Deleted => (235, 90, 90),
            Self::Replaced => (185, 140, 240),
            Self::Conflict => (255, 80, 160),
            Self::Missing => (235, 90, 90),
            Self::Unversioned => (150, 150, 150),
            _ => (170, 170, 170),
        }
    }

    /// 必须人工干预才动得了的条目：冲突（含 tree conflict，`svn status --xml` 里同样是
    /// `conflicted`）与不完整。目录行、顶栏汇总和提交页「需处理」都按这个口径数。
    pub fn blocked(self) -> bool {
        matches!(self, Self::Conflict | Self::Incomplete)
    }
}

/// 需要人工处理的条目数。
pub fn blocked_count(entries: &[StatusEntry]) -> usize {
    entries.iter().filter(|entry| entry.item.blocked()).count()
}

#[derive(Clone, Debug)]
pub struct StatusEntry {
    pub path: String,
    pub name: String,
    pub item: Item,
    pub props: String,
    pub checked: bool,
}

#[derive(Clone, Debug)]
pub struct LogPath {
    pub action: char,
    pub kind: String,
    pub path: String,
}

#[derive(Clone, Debug)]
pub struct LogEntry {
    pub revision: String,
    pub author: String,
    pub date: String,
    pub message: String,
    pub paths: Vec<LogPath>,
}

// ------------------------------------------------------------------------ XML 解析

fn first_text(doc: &roxmltree::Document<'_>, tag: &str) -> String {
    doc.descendants()
        .find(|node| node.has_tag_name(tag))
        .and_then(|node| node.text())
        .unwrap_or("")
        .to_owned()
}

fn first_child_text(node: &roxmltree::Node<'_, '_>, tag: &str) -> String {
    node.children()
        .find(|n| n.has_tag_name(tag))
        .and_then(|n| n.text())
        .unwrap_or("")
        .to_owned()
}

pub fn parse_info(xml: &str) -> Option<WcInfo> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let entry = doc.descendants().find(|node| node.has_tag_name("entry"))?;
    let commit = doc.descendants().find(|node| node.has_tag_name("commit"));
    Some(WcInfo {
        is_wc: true,
        url: first_text(&doc, "url"),
        repos_root: first_text(&doc, "root"),
        revision: entry.attribute("revision").unwrap_or("").to_owned(),
        node_kind: entry.attribute("kind").unwrap_or("").to_owned(),
        last_rev: commit
            .as_ref()
            .and_then(|node| node.attribute("revision"))
            .unwrap_or("")
            .to_owned(),
        last_author: first_text(&doc, "author"),
        last_date: format_svn_date(&first_text(&doc, "date")),
    })
}

/// `svn status -u --xml`：带 <repos-status> 子节点的 entry，就是服务器上已经变了、
/// 本地还没更新的条目。光比 `svn info` 的工作副本 Revision 和仓库 HEAD 会误报——
/// 提交只推进被提交路径的版本号，工作副本根目录还停在旧版本上，HEAD 一动就显示「可更新」。
pub fn parse_out_of_date(xml: &str) -> usize {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return 0;
    };
    doc.descendants()
        .filter(|node| node.has_tag_name("entry"))
        .filter(|node| {
            node.children()
                .any(|child| child.has_tag_name("repos-status"))
        })
        .count()
}

/// `svn status --xml`：entry 的 path 属性可能是绝对路径，也可能是相对路径。
pub fn parse_status(xml: &str, base: &Path) -> Vec<StatusEntry> {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for node in doc.descendants().filter(|n| n.has_tag_name("entry")) {
        let Some(raw) = node.attribute("path") else {
            continue;
        };
        let status = node.children().find(|n| n.has_tag_name("wc-status"));
        let item = Item::from_svn(
            status
                .as_ref()
                .and_then(|s| s.attribute("item"))
                .unwrap_or("none"),
        );
        let props = status
            .as_ref()
            .and_then(|s| s.attribute("props"))
            .unwrap_or("none")
            .to_owned();
        let path = if raw.is_empty() || raw == "." {
            base.display().to_string()
        } else if Path::new(raw).is_absolute() {
            raw.to_owned()
        } else {
            base.join(raw).display().to_string()
        };
        let name = Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone());
        entries.push(StatusEntry {
            path,
            name,
            item,
            props,
            // 默认勾上所有「能随上传走」的条目：A/M/D/R 直接提交，?(自动 add)、!(自动 delete)
            // 也一起带上；冲突、不完整、忽略、外部引用必须先人工处理，仍不勾（勾了也提交不了）。
            checked: item.uploadable(),
        });
    }
    entries.sort_by(|a, b| a.item.text().cmp(b.item.text()).then(a.path.cmp(&b.path)));
    entries
}

pub fn parse_log(xml: &str) -> Vec<LogEntry> {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return Vec::new();
    };
    let mut logs = Vec::new();
    for node in doc.descendants().filter(|n| n.has_tag_name("logentry")) {
        let mut paths = Vec::new();
        for path in node.descendants().filter(|n| n.has_tag_name("path")) {
            let action = path
                .attribute("action")
                .unwrap_or("")
                .chars()
                .next()
                .unwrap_or(' ');
            paths.push(LogPath {
                action,
                kind: path.attribute("kind").unwrap_or("").to_owned(),
                path: path.text().unwrap_or("").to_owned(),
            });
        }
        logs.push(LogEntry {
            revision: node.attribute("revision").unwrap_or("").to_owned(),
            author: first_child_text(&node, "author"),
            date: format_svn_date(&first_child_text(&node, "date")),
            message: first_child_text(&node, "msg").trim().to_owned(),
            paths,
        });
    }
    logs
}

/// svn 返回的 UTC 时间可能带 7 位小数秒，统一转为本地时间字符串。
pub fn format_svn_date(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(&trim_fractional(raw)) {
        return parsed
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
    }
    raw.to_owned()
}

fn trim_fractional(raw: &str) -> String {
    let Some(dot) = raw.find('.') else {
        return raw.to_owned();
    };
    let tail = raw[dot + 1..]
        .find(|c| c == 'Z' || c == '+' || c == '-')
        .map(|index| dot + 1 + index)
        .unwrap_or(raw.len());
    if tail - (dot + 1) <= 6 {
        return raw.to_owned();
    }
    format!("{}{}", &raw[..dot + 6], &raw[tail..])
}

// ------------------------------------------------------------------------ svn 客户端

#[derive(Clone, Debug, Default)]
pub struct Svn {
    pub exe: PathBuf,
    pub username: String,
    pub password: String,
}

/// 忽略空白与换行差异时给 `svn diff` 追加的参数。两个值必须整体作为 `-x` 的一个参数
/// 交给 svn 内置的 diff 引擎（所以两条取差异的命令都固定带 `--internal-diff`）。
pub(crate) const DIFF_IGNORE_ARGS: [&str; 2] = ["-x", "-w --ignore-eol-style"];

/// 上面那组参数的一行写法，用于把命令回显到输出记录里。
pub(crate) fn diff_flags(ignore_white: bool) -> String {
    if ignore_white {
        format!(" {} \"{}\"", DIFF_IGNORE_ARGS[0], DIFF_IGNORE_ARGS[1])
    } else {
        String::new()
    }
}

impl Svn {
    pub fn available(&self) -> bool {
        !self.exe.as_os_str().is_empty() && self.exe.is_file()
    }

    pub fn version(&self) -> Option<String> {
        if !self.available() {
            return None;
        }
        let run = self.call(&["--version", "--quiet"], None);
        if run.ok {
            Some(run.out.trim().to_owned())
        } else {
            None
        }
    }

    fn global(&self) -> Vec<String> {
        let mut args = vec![
            "--non-interactive".to_owned(),
            "--trust-server-cert-failures=unknown-ca,cn-mismatch,expired,not-yet-valid,other"
                .to_owned(),
        ];
        if !self.username.trim().is_empty() {
            args.push("--username".to_owned());
            args.push(self.username.trim().to_owned());
            args.push("--password".to_owned());
            args.push(self.password.clone());
        }
        args
    }

    fn command(&self, args: &[String], cwd: Option<&Path>) -> Command {
        let mut command = Command::new(&self.exe);
        command.args(args);
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        command
    }

    fn capture(&self, args: &[String], cwd: Option<&Path>) -> Run {
        match self.command(args, cwd).output() {
            Ok(output) => Run::new(
                output.status.success(),
                output.status.code().unwrap_or(-1),
                decode_bytes(&output.stdout),
                decode_bytes(&output.stderr),
            ),
            Err(e) => Run::error(format!("无法启动 {}：{}", self.exe.display(), e)),
        }
    }

    /// 逐行回调执行（update / commit 等耗时命令）。
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
        Run::new(
            status.as_ref().map(|s| s.success()).unwrap_or(false),
            status.ok().and_then(|s| s.code()).unwrap_or(-1),
            decode_bytes(&all),
            err,
        )
    }

    fn call(&self, args: &[&str], cwd: Option<&Path>) -> Run {
        let mut owned = self.global();
        owned.extend(args.iter().map(|a| (*a).to_owned()));
        self.capture(&owned, cwd)
    }

    fn call_stream(&self, args: &[&str], cwd: Option<&Path>, on_line: &dyn Fn(&str)) -> Run {
        let mut owned = self.global();
        owned.extend(args.iter().map(|a| (*a).to_owned()));
        self.stream(&owned, cwd, on_line)
    }

    pub fn info(&self, target: &str) -> (Option<WcInfo>, Run) {
        let run = self.call(&["info", "--xml", target], None);
        (
            if run.ok {
                parse_info(&run.out)
            } else {
                None
            },
            run,
        )
    }

    pub fn status(&self, dir: &Path) -> (Vec<StatusEntry>, Run) {
        let run = self.call(&["status", "--xml", &dir.to_string_lossy()], None);
        (
            if run.ok {
                parse_status(&run.out, dir)
            } else {
                Vec::new()
            },
            run,
        )
    }

    /// 有几个文件在服务器上已经变了、本地还没更新。要走网络，只在后台检测里调。
    /// 返回 None 表示探测失败（断网 / 认证异常），调用方不能据此断定「已是最新」。
    pub fn out_of_date(&self, dir: &Path) -> Option<usize> {
        let run = self.call(
            &["status", "-u", "--xml", &dir.to_string_lossy()],
            None,
        );
        run.ok.then(|| parse_out_of_date(&run.out))
    }

    /// 提交记录一律读服务器：不带 `-r HEAD:1` 时，在工作副本里执行 `svn log`
    /// 只读到本地 BASE（本地 r6608、服务器已经 r6610 时，服务器上新的记录就看不见）。
    /// `author` 非空时再加 `--search`，让 svn 只回这个人的提交，少传一堆数据。
    /// `target` 可以是工作副本路径，也可以是仓库 URL（单个文件的修改记录就用 URL 查，
    /// 这样即使该文件在本地已被改名 / 删除也能读到记录）。
    /// `limit` > 0 时加 `-l` 限制条数；<= 0 表示不限条数（不带 `-l`，svn 会拉全量）。
    pub fn log(&self, target: &str, limit: i64, author: &str) -> (Vec<LogEntry>, Run) {
        self.log_in(target, "HEAD:1", limit, author)
    }

    /// 同 `log`，但由调用方给出版本区间（`revspec`，如 `HEAD:1` 或 `{2026-09-11}:{2026-09-04}`）。
    /// 按日期查统计时必须走这里：`log` 固定的 `HEAD:1` 无法限定区间。
    /// `revspec` 一律写成「新 → 旧」：`-r` 是正序时 `-l` 会保留区间里**最旧**的 N 条，
    /// 所以日期区间这里不限制条数（`limit` 传 0），只靠区间本身收口。
    pub fn log_in(&self, target: &str, revspec: &str, limit: i64, author: &str) -> (Vec<LogEntry>, Run) {
        let mut args: Vec<&str> = vec!["log", "--xml", "-v", "-r", revspec];
        let count;
        if limit > 0 {
            count = limit.to_string();
            args.extend(["-l", &count]);
        }
        args.push(target);
        // 老版本 svn.exe 不认 --search 时的回退长度：截掉后面的 --search 参数
        let base_len = args.len();
        if !author.is_empty() {
            args.extend(["--search", author]);
        }
        let mut run = self.call(&args, None);
        if !run.ok && !author.is_empty() {
            // 老版本 svn.exe 不认 --search：退回不过滤，靠下面的作者过滤兜底
            args.truncate(base_len);
            run = self.call(&args, None);
        }
        let mut entries = if run.ok {
            parse_log(&run.out)
        } else {
            Vec::new()
        };
        if !author.is_empty() {
            entries.retain(|entry| entry.author.eq_ignore_ascii_case(author));
        }
        (entries, run)
    }

    /// 本机 svn 缓存的登录人（`svn auth`）。给出仓库地址时优先返回同一台服务器上的账号。
    pub fn auth_user(&self, url: &str) -> Option<String> {
        let run = self.call(&["auth"], None);
        if !run.ok {
            return None;
        }
        let want = url
            .split("//")
            .nth(1)
            .unwrap_or("")
            .split('/')
            .next()
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("")
            .to_lowercase();
        let mut kind = String::new();
        let mut realm = String::new();
        let mut first = String::new();
        for line in run.out.lines() {
            let text = line.trim();
            if let Some(value) = text.strip_prefix("Credential kind:") {
                kind = value.trim().to_owned();
                realm.clear();
            } else if let Some(value) = text.strip_prefix("Authentication realm:") {
                realm = value.trim().to_lowercase();
            } else if let Some(value) = text.strip_prefix("Username:") {
                let user = value.trim().to_owned();
                if user.is_empty() || kind != "svn.simple" {
                    continue;
                }
                if first.is_empty() {
                    first = user.clone();
                }
                // realm 形如 "<https://192.168.1.251:443> visualsvn server"
                if !want.is_empty() && realm.contains(&want) {
                    return Some(user);
                }
            }
        }
        (!first.is_empty()).then_some(first)
    }

    pub fn update(&self, dir: &Path, on_line: &dyn Fn(&str)) -> Run {
        self.call_stream(&["update", &dir.to_string_lossy()], None, on_line)
    }

    /// 组装取差异的参数：`head` 是 `svn diff` 的固定部分，`target` 是本地路径或仓库 URL。
    /// 一律带 `--internal-diff`，这样 `-x` 的参数必然由 svn 内置 diff 解释。
    /// 忽略空白与换行由调用方按当前这一项决定（XML 自动忽略，其余默认不忽略）。
    fn diff_args<'a>(&self, head: &[&'a str], target: &'a str, ignore_white: bool) -> Vec<&'a str> {
        let mut args: Vec<&'a str> = head.to_vec();
        if ignore_white {
            args.extend_from_slice(&DIFF_IGNORE_ARGS);
        }
        args.push(target);
        args
    }

    /// 单个文件的本次修改内容。
    pub fn diff(&self, path: &Path, ignore_white: bool) -> Run {
        let target = path.to_string_lossy().into_owned();
        let args = self.diff_args(&["diff", "--internal-diff"], &target, ignore_white);
        self.call(&args, None)
    }

    /// 某次提交对这个路径的逐行差异（和它的上一个版本比）：`svn diff -c <rev> <URL>`。
    /// 用仓库 URL 查，本地没有该文件 / 已被改名删除也能取到；本次新增的文件会整份显示为 +。
    pub fn diff_rev(&self, revision: &str, target: &str, ignore_white: bool) -> Run {
        let args = self.diff_args(&["diff", "-c", revision, "--internal-diff"], target, ignore_white);
        self.call(&args, None)
    }

    /// 取工作副本 BASE 版本的原始内容写到 `to`，供 Beyond Compare 之类外部工具对比。
    /// `svn cat -r BASE` 读的是本地 pristine 存储，不走网络。
    pub fn cat_base(&self, path: &Path, to: &Path) -> Result<(), String> {
        let mut args = self.global();
        args.extend([
            "cat".to_owned(),
            "-r".to_owned(),
            "BASE".to_owned(),
            path.to_string_lossy().into_owned(),
        ]);
        let output = self
            .command(&args, None)
            .output()
            .map_err(|e| format!("无法启动 {}：{}", self.exe.display(), e))?;
        if !output.status.success() {
            return Err(decode_bytes(&output.stderr).trim().to_owned());
        }
        std::fs::write(to, &output.stdout).map_err(|e| e.to_string())
    }

    pub fn cleanup(&self, dir: &Path, on_line: &dyn Fn(&str)) -> Run {
        self.call_stream(&["cleanup", &dir.to_string_lossy()], None, on_line)
    }

    pub fn revert_all(&self, dir: &Path, on_line: &dyn Fn(&str)) -> Run {
        self.call_stream(
            &["revert", "--recursive", &dir.to_string_lossy()],
            None,
            on_line,
        )
    }

    pub fn resolve_working(&self, dir: &Path, on_line: &dyn Fn(&str)) -> Run {
        self.call_stream(
            &[
                "resolve",
                "--accept",
                "working",
                "--recursive",
                &dir.to_string_lossy(),
            ],
            None,
            on_line,
        )
    }

    /// 修改仓库地址（`svn relocate`）：只改写工作副本里的 URL 元数据，不动任何本地文件。
    /// `from` / `to` 是地址前缀，svn 会把工作副本 URL 中匹配到的 `from` 换成 `to`；
    /// svn 会连新地址一起校验仓库 UUID，地址写错只是失败，不会把工作副本改坏。
    pub fn relocate(&self, dir: &Path, from: &str, to: &str, on_line: &dyn Fn(&str)) -> Run {
        let target = dir.to_string_lossy().into_owned();
        self.call_stream(&["relocate", from, to, &target], None, on_line)
    }

    /// 目标过多时改用 `--targets` 文件（按本机 ANSI 编码写入，中文系统即 GBK）。
    fn with_targets(&self, head: Vec<String>, targets: &[String]) -> Vec<String> {
        let width: usize = targets.iter().map(|t| t.chars().count() + 3).sum();
        let mut args = head;
        if width > 12_000 {
            let file = temp_file("svn_manager_targets");
            let body = targets.join("\r\n");
            if fs::write(&file, GBK.encode(&body).0.as_ref()).is_ok() {
                args.push("--targets".to_owned());
                args.push(file.to_string_lossy().into_owned());
                return args;
            }
        }
        args.extend(targets.iter().cloned());
        args
    }

    pub fn add(&self, dir: &Path, targets: &[String], on_line: &dyn Fn(&str)) -> Run {
        let args = self.with_targets(vec!["add".to_owned(), "--force".to_owned()], targets);
        self.stream(&args, Some(dir), on_line)
    }

    pub fn delete(&self, dir: &Path, targets: &[String], on_line: &dyn Fn(&str)) -> Run {
        let args = self.with_targets(vec!["delete".to_owned()], targets);
        self.stream(&args, Some(dir), on_line)
    }

    pub fn commit(
        &self,
        dir: &Path,
        targets: &[String],
        message: &str,
        on_line: &dyn Fn(&str),
    ) -> Run {
        let message_file = temp_file("svn_manager_log");
        if fs::write(&message_file, message.as_bytes()).is_err() {
            return Run::error("无法写入提交说明临时文件");
        }
        let head = self.with_targets(
            vec!["commit".to_owned(), "--depth".to_owned(), "empty".to_owned()],
            targets,
        );
        let mut args = self.global();
        args.extend(head);
        args.push("--file".to_owned());
        args.push(message_file.to_string_lossy().into_owned());
        args.push("--encoding".to_owned());
        args.push("UTF-8".to_owned());
        let run = self.stream(&args, Some(dir), on_line);
        let _ = fs::remove_file(&message_file);
        run
    }
}

// ------------------------------------------------------------------------ 自动寻找 svn.exe

pub(crate) fn add_path(list: &mut Vec<PathBuf>, path: PathBuf) {
    if !list.contains(&path) {
        list.push(path);
    }
}

pub(crate) fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

pub(crate) fn drive_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = (b'C'..=b'Z')
        .map(|letter| PathBuf::from(format!("{}:\\", letter as char)))
        .collect();
    roots.retain(|root| root.exists());
    roots.sort_by_key(|root| root.display().to_string());
    roots
}

const CLIENT_SUBDIRS: &[&str] = &[
    "TortoiseSVN/bin",
    "Subversion/bin",
    "SlikSvn/bin",
    "VisualSVN Subversion/bin",
    "VisualSVN Server/bin",
    "CollabNet/Subversion Client/bin",
    "WinSVN/bin",
    "Apache Subversion/bin",
    "subversion/bin",
    "svn/bin",
];

pub(crate) const SEARCH_BASES: &[&str] = &[
    "Program Files",
    "Program Files (x86)",
    "Program",
    "Program\\Tools",
    "Program Files\\Tools",
    "Tools",
    "Dev",
    "Soft",
    "Software",
    "usr",
    "opt",
];

fn registry_candidates() -> Vec<PathBuf> {
    let mut found = Vec::new();
    for key in [
        "HKLM\\SOFTWARE\\TortoiseSVN",
        "HKLM\\SOFTWARE\\WOW6432Node\\TortoiseSVN",
        "HKCU\\SOFTWARE\\TortoiseSVN",
    ] {
        let Ok(output) = Command::new("reg").args(["query", key]).output() else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        for line in decode_bytes(&output.stdout).lines() {
            let Some(pos) = line.find("REG_SZ") else {
                continue;
            };
            let value = line[pos + "REG_SZ".len()..].trim();
            if value.is_empty() || !value.contains(':') {
                continue;
            }
            let base = PathBuf::from(value);
            for candidate in [base.join("bin").join("svn.exe"), base.join("svn.exe")] {
                if candidate.is_file() {
                    add_path(&mut found, candidate);
                }
            }
        }
    }
    found
}

fn known_locations() -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Some(custom) = env_path("SVN_MANAGER_SVN") {
        add_path(&mut found, custom);
    }
    for name in ["SVN_ROOT", "SVN_HOME", "Subversion_Home"] {
        if let Some(root) = env_path(name) {
            add_path(&mut found, root.join("bin").join("svn.exe"));
            add_path(&mut found, root.join("svn.exe"));
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            add_path(&mut found, dir.join("svn.exe"));
        }
    }
    for name in [
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "LOCALAPPDATA",
        "ProgramData",
    ] {
        let Some(base) = env_path(name) else {
            continue;
        };
        for sub in CLIENT_SUBDIRS {
            add_path(&mut found, base.join(sub).join("svn.exe"));
        }
    }
    if let Some(profile) = env_path("USERPROFILE") {
        for sub in [
            "scoop\\apps\\subversion\\current\\bin",
            "scoop\\apps\\tortoisesvn\\current\\bin",
            "AppData\\Local\\Programs\\TortoiseSVN\\bin",
        ] {
            add_path(&mut found, profile.join(sub).join("svn.exe"));
        }
    }
    if let Some(programdata) = env_path("ProgramData") {
        add_path(&mut found, programdata.join("chocolatey\\bin\\svn.exe"));
    }
    for root in drive_roots() {
        for base in SEARCH_BASES {
            let dir = root.join(base);
            for sub in CLIENT_SUBDIRS {
                add_path(&mut found, dir.join(sub).join("svn.exe"));
            }
        }
    }
    found.extend(registry_candidates());
    found.retain(|path| path.is_file());
    found
}

fn scan_dir(dir: &Path, depth: usize, hits: &mut Vec<PathBuf>, start: &Instant) {
    if depth == 0 || hits.len() >= 24 || start.elapsed() > Duration::from_secs(6) {
        return;
    }
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for item in read.flatten() {
        if start.elapsed() > Duration::from_secs(6) {
            return;
        }
        let name = item.file_name().to_string_lossy().to_lowercase();
        if [
            "windows",
            "$recycle.bin",
            "system volume information",
            "perflogs",
            "recovery",
            "users",
            "appdata",
            "programdata",
            "temp",
        ]
        .iter()
        .any(|skip| name.starts_with(skip))
        {
            continue;
        }
        let Ok(file_type) = item.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let child = item.path();
        if name.contains("svn") {
            let candidate = child.join("bin").join("svn.exe");
            if candidate.is_file() {
                add_path(hits, candidate);
            }
        }
        scan_dir(&child, depth - 1, hits, start);
    }
}

/// 所有可用的 svn.exe 候选路径（按可信度排序）。
pub fn candidates() -> Vec<PathBuf> {
    let mut found = known_locations();
    if found.is_empty() {
        // 常见位置都没有时才做磁盘浅层扫描（最慢的一步，限时 6 秒）
        let start = Instant::now();
        let mut hits = Vec::new();
        for root in drive_roots() {
            scan_dir(&root, 3, &mut hits, &start);
        }
        hits.sort_by_key(|path| path.components().count());
        for hit in hits {
            add_path(&mut found, hit);
        }
    }
    found
}

/// 自动寻找并验证 svn.exe，返回 (路径, 版本号)。
pub fn detect(list: &[PathBuf]) -> Option<(PathBuf, String)> {
    for path in list {
        let svn = Svn {
            exe: path.clone(),
            ..Default::default()
        };
        if let Some(version) = svn.version() {
            return Some((path.clone(), version));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> Svn {
        Svn {
            exe: PathBuf::from("svn"),
            ..Default::default()
        }
    }

    /// 「全部上传」的口径：A/M/D/R 直接可提交，?（自动 add）和 !（自动 delete）也算；
    /// 冲突、不完整、已忽略、外部引用不能自动带上服务器。
    #[test]
    fn uploadable_covers_pending_add_and_delete() {
        let yes = [
            Item::Added,
            Item::Modified,
            Item::Deleted,
            Item::Replaced,
            Item::Unversioned,
            Item::Missing,
        ];
        let no = [
            Item::Conflict,
            Item::Incomplete,
            Item::Ignored,
            Item::External,
            Item::Normal,
            Item::Other,
        ];
        for item in yes {
            assert!(item.uploadable(), "{} 应算待提交", item.mark());
        }
        for item in no {
            assert!(!item.uploadable(), "{} 不应自动提交", item.mark());
        }
    }

    /// status 刷新后的默认勾选口径 = uploadable：?（自动 add）、!（自动 delete）和
    /// A/M/D/R 一样进场就勾上；冲突这类必须人工处理的依旧不勾。
    #[test]
    fn parse_status_checks_everything_uploadable_by_default() {
        let xml = r#"<status>
  <target path="D:\wc">
    <entry path="D:\wc\mod.txt"><wc-status item="modified" props="none"/></entry>
    <entry path="D:\wc\new.txt"><wc-status item="unversioned" props="none"/></entry>
    <entry path="D:\wc\gone.txt"><wc-status item="missing" props="none"/></entry>
    <entry path="D:\wc\clash.txt"><wc-status item="conflicted" props="none"/></entry>
  </target>
</status>"#;
        let entries = parse_status(xml, Path::new(r"D:\wc"));
        let checked = |name: &str| {
            entries
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("缺条目 {name}"))
                .checked
        };
        assert!(checked("mod.txt"), "修改项应默认勾选");
        assert!(checked("new.txt"), "未版本化(?)应默认勾选（提交时自动 add）");
        assert!(checked("gone.txt"), "已丢失(!)应默认勾选（提交时自动 delete）");
        assert!(!checked("clash.txt"), "冲突项必须先人工处理，不默认勾选");
    }

    /// 「可更新」只数带 <repos-status> 的条目：光比本地 Revision 和仓库 HEAD，会把
    /// 自己刚提交的文件误判成「有新版可更新」。
    #[test]
    fn out_of_date_counts_repos_status_entries() {
        let xml = r#"<status>
  <target path="D:\wc">
    <entry path="D:\wc"><wc-status item="normal" props="none" revision="1097"/></entry>
    <entry path="D:\wc\a.vue"><wc-status item="normal" props="none" revision="1097"/><repos-status item="modified" props="none"/></entry>
    <entry path="D:\wc\b.vue"><wc-status item="normal" props="none" revision="1097"/><repos-status item="modified" props="none"/></entry>
  </target>
  <against revision="1098"/>
</status>"#;
        assert_eq!(parse_out_of_date(xml), 2, "只有带 repos-status 的条目算待更新");
        assert_eq!(parse_out_of_date("<status>"), 0, "输出残缺时不误报");
    }

    /// 目录行「冲突 N」与提交页「需处理」共用 blocked_count，口径只能有一处。
    #[test]
    fn blocked_count_covers_conflicts_and_incomplete_only() {
        let xml = r#"<status>
  <target path="D:\wc">
    <entry path="D:\wc\a.java"><wc-status item="conflicted" props="none" revision="10"/></entry>
    <entry path="D:\wc\b.java"><wc-status item="conflicted" props="items" revision="10"/></entry>
    <entry path="D:\wc\c"><wc-status item="incomplete" props="none" revision="10"/></entry>
    <entry path="D:\wc\d.java"><wc-status item="modified" props="none" revision="10"/></entry>
    <entry path="D:\wc\e.java"><wc-status item="unversioned" props="none" revision="10"/></entry>
  </target>
</status>"#;
        let entries = parse_status(xml, Path::new(r"D:\wc"));
        assert_eq!(blocked_count(&entries), 3, "冲突两条 + 不完整一条");
        assert_eq!(blocked_count(&[]), 0);
        let clean = parse_status(
            r#"<status><target path="D:\wc"><entry path="D:\wc\a.java"><wc-status item="modified" props="none" revision="10"/></entry></target></status>"#,
            Path::new(r"D:\wc"),
        );
        assert_eq!(blocked_count(&clean), 0, "普通改动不算需处理");
    }

    #[test]
    fn working_copy_diff_uses_the_internal_diff() {
        let args = client().diff_args(&["diff", "--internal-diff"], r"D:\wc\a.java", false);
        assert_eq!(args, ["diff", "--internal-diff", r"D:\wc\a.java"]);
    }

    /// 忽略空白时 `-w --ignore-eol-style` 必须整体作为 `-x` 的一个参数交给内置 diff。
    #[test]
    fn ignore_white_adds_a_single_x_argument() {
        let args = client().diff_args(&["diff", "-c", "6511", "--internal-diff"], "http://x/a.xml", true);
        assert_eq!(
            args,
            [
                "diff",
                "-c",
                "6511",
                "--internal-diff",
                "-x",
                "-w --ignore-eol-style",
                "http://x/a.xml"
            ]
        );
        assert_eq!(diff_flags(true), " -x \"-w --ignore-eol-style\"");
        assert!(diff_flags(false).is_empty());
    }
}
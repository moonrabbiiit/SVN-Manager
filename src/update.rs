//! 版本更新检查与自动升级。
//!
//! ## 更新源
//!
//! - **官方源**：GitHub 仓库的最新 Release（`OFFICIAL_API`）。版本号取 `tag_name`，
//!   下载地址取资产里第一个 `.exe`，GitHub 同时给出资产的 `sha256` digest，
//!   正好接进下面那条 certutil 校验链；`releases/latest` 本身已排除草稿与预发布。
//! - **自定义源**：局域网里自建的静态服务端，读 `{服务端地址}/latest.json`。
//!
//! ## 协议（服务端只需要静态文件）
//!
//! 客户端把「设置 → 版本更新 → 服务端地址」配置成服务根目录，例如
//! `http://192.168.1.10:8666`（也可以直接配到 json 文件本身）。程序启动后读取
//!
//! ```text
//! {服务端地址}/latest.json
//! ```
//!
//! 内容（UTF-8 JSON）：
//!
//! ```json
//! {
//!   "version": "1.2.0",
//!   "url": "files/svn_manager_1.2.0.exe",
//!   "notes": "修复了 xxx",
//!   "sha256": "……（发布脚本会写；客户端拿它判有没有新版，下载后也用它校验）"
//! }
//! ```
//!
//! - `url` 相对 `latest.json` 所在目录拼接；写完整的 http(s) 地址也可以。
//! - 服务端用 `tools/update_server.py` 即可（发布 + 托管），nginx / IIS 等
//!   静态服务器同样适用。
//!
//! ## 怎么算「有新版本」
//!
//! 不看版本号，看文件：把更新源给的 `sha256`（自建源由 `update_server.py` 发布时写入，
//! 官方源用 GitHub 资产的 digest）与本地正在运行的 exe 的 sha256 比，**不同就算有新版本**。
//! 同一个版本号重新发布的构建因此也能被发现；源没给 `sha256`（早先上传的老资产）或本地
//! exe 算不出哈希时，退回版本号比较（[`has_update`]）。
//!
//! ## 更新流程
//!
//! 发现新版本 → 用户确认 → 程序内下载新 exe（日志区可见进度）→ certutil 校验
//! SHA256（服务端提供时）→ 生成收尾 bat（杀残留实例 → 等主程序退出 → 覆盖 →
//! 重启 → 自删）并启动 → 主程序退出，后续交给 bat 完成。
//! 下载与校验不放進 bat：脚本在磁盘上停留越久，越容易被安全软件当成可疑文件查删。
//! bat 由 `start` 拉起，而 start 打开 .bat 等价于 `cmd /K 脚本`：脚本自删之后只是
//! 「返回」（`exit /b`）的话，cmd 会回到已经不存在的脚本上，打印一句
//! 「找不到批处理文件。」并留下一个空着的命令行窗口——所以脚本结尾必须用 `exit`
//! 直接结束 cmd 进程，成功、失败两条路径都是。
//! bat 里的输出全部用 ASCII，避免任何代码页乱码问题。

use std::ops::Range;
use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
use crate::svn::CREATE_NO_WINDOW;

use serde::{Deserialize, Serialize};

/// 官方源仓库（GitHub）
pub const OFFICIAL_REPO: &str = "moonrabbiiit/SVN-Manager";
/// 最新发布：`releases/latest` 自动排除草稿与预发布，也不按时间排序取，交给 GitHub 判
pub const OFFICIAL_API: &str = "https://api.github.com/repos/moonrabbiiit/SVN-Manager/releases/latest";
pub const OFFICIAL_PAGE: &str = "https://github.com/moonrabbiiit/SVN-Manager";

/// 更新源（写在配置的 `update_source` 里）：官方 GitHub 发布 / 自建服务端 latest.json
pub const SOURCE_OFFICIAL: &str = "official";
pub const SOURCE_CUSTOM: &str = "custom";

/// 选的是不是官方源。老配置里没有这个字段（空串）按官方算：官方源不用用户填任何东西。
pub fn is_official(source: &str) -> bool {
    source.trim() != SOURCE_CUSTOM
}

/// 服务端 latest.json 的字段（缺省字段宽松处理，兼容以后扩展）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UpdateManifest {
    /// 服务端最新版本号，如 "1.2.0"（V1.2.0 / v1.2.0 这类前缀在解析时就剥掉）
    pub version: String,
    /// 新版 exe 下载地址（相对或绝对）
    pub url: String,
    /// 更新说明，界面展示用
    #[serde(default)]
    pub notes: String,
    /// 新 exe 的 SHA256（小写十六进制），非空时 bat 里用 certutil 校验
    #[serde(default)]
    pub sha256: String,
    /// 发布时间，界面展示用
    #[serde(default)]
    pub published_at: String,
}

/// 把版本号拆成数字段："V1.2.10" -> [1, 2, 10]。
/// 非 "主.次.修订" 的部分（构建号后缀等）忽略，解析不出任何数字返回空表。
pub fn parse_version(text: &str) -> Vec<u32> {
    text.trim()
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u32>().ok())
        .collect()
}

/// 远端版本是否比当前版本新（逐段比较，短的补 0：1.2 > 1.1.9，1.2 == 1.2.0）。
pub fn is_newer(remote: &str, current: &str) -> bool {
    let (remote, current) = (parse_version(remote), parse_version(current));
    for index in 0..remote.len().max(current.len()) {
        let r = remote.get(index).copied().unwrap_or(0);
        let c = current.get(index).copied().unwrap_or(0);
        if r != c {
            return r > c;
        }
    }
    false
}

/// 版本号取用前先剥掉 `v` / `V` 前缀：界面各处都自己加「V」，留着会显示成 Vv1.2.4。
/// GitHub 的 tag 与手写 / 老版本发布的 latest.json 都可能带这个前缀。
pub fn version_text(raw: &str) -> String {
    raw.trim().trim_start_matches(['v', 'V']).trim().to_owned()
}

/// 两处版本号是不是同一版（逐段比，短的补 0：1.2 与 1.2.0 算同一版）。
/// 任一边解析不出数字就不算同一版：一个连版本号都没写的源不该被当成「没变化」。
pub fn same_version(left: &str, right: &str) -> bool {
    let (left, right) = (parse_version(left), parse_version(right));
    !left.is_empty()
        && !right.is_empty()
        && (0..left.len().max(right.len())).all(|index| {
            left.get(index).copied().unwrap_or(0) == right.get(index).copied().unwrap_or(0)
        })
}

/// 更新源上有没有可装的新东西——以文件为准。
///
/// 源给了 `sha256` 就与本地正在运行的 exe 的 sha256 比，不同即算有新版本：同一个版本号
/// 重新发布的构建也能被发现。源没给 `sha256`（早先上传的老资产）、或本地 exe 取不到 /
/// 算不出哈希时，退回版本号比较。哈希要走一次 certutil，只在后台检查那一趟调用，
/// 别放进每帧的界面代码里。
pub fn has_update(manifest: &UpdateManifest, current_version: &str) -> bool {
    let remote = manifest.sha256.trim().to_ascii_lowercase();
    if !remote.is_empty() {
        if let Ok(exe) = std::env::current_exe() {
            if let Ok(local) = sha256_of(&exe) {
                return local != remote;
            }
        }
    }
    is_newer(manifest.version.trim(), current_version)
}

/// 拼接下载地址：绝对 http(s) URL 原样返回，相对路径拼到服务根目录后面。
pub fn join_url(base: &str, url: &str) -> String {
    let url = url.trim();
    if url.starts_with("http://") || url.starts_with("https://") {
        return url.to_owned();
    }
    let base = base.trim().trim_end_matches('/');
    let url = url.trim_start_matches('/');
    format!("{base}/{url}")
}

/// 服务根目录 -> latest.json 地址。地址本身就指向 .json 时原样使用。
fn manifest_url(server: &str) -> String {
    let server = server.trim().trim_end_matches('/');
    if server.ends_with(".json") {
        server.to_owned()
    } else {
        format!("{server}/latest.json")
    }
}

/// 取 URL 里的主机名（剥掉 scheme、userinfo、端口与路径）。
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    if let Some(stripped) = host.strip_prefix('[') {
        // IPv6 字面量 [::1]:8080
        stripped.split(']').next().unwrap_or(host)
    } else {
        host.split(':').next().unwrap_or(host)
    }
}

/// 更新地址是否指向内网 / 本机。内网地址绝不该走系统代理：
/// 代理客户端（Clash 等）通过 http_proxy 环境变量劫持 curl 后，
/// 内网请求会被转发到远端节点，结果是超时或 502，永远连不到局域网服务器。
/// AI 日志等其它模块发请求时也用它判断要不要 `--noproxy`。
pub fn is_private_host(url: &str) -> bool {
    let host = host_of(url).to_ascii_lowercase();
    if host == "localhost" || host.starts_with("127.") || host == "::1" {
        return true;
    }
    if host.starts_with("192.168.") || host.starts_with("10.") {
        return true;
    }
    // 172.16.0.0 - 172.31.255.255
    if let Some(rest) = host.strip_prefix("172.") {
        if let Ok(second) = rest.split('.').next().unwrap_or("").parse::<u32>() {
            if (16..=31).contains(&second) {
                return true;
            }
        }
    }
    // 不带点的主机名（http://nas:8666 这类）也当内网
    !host.contains('.') && !host.contains(':')
}

/// 找系统自带的 curl.exe（Win10 1803+）。
/// 32 位进程访问 System32 会被重定向到 SysWOW64（同样带 curl），Sysnative 兜底。
/// AI 日志模块发 HTTPS 请求也复用它，不用另带 HTTP 依赖。
pub fn find_curl() -> Option<PathBuf> {
    for path in [
        PathBuf::from(r"C:\Windows\System32\curl.exe"),
        PathBuf::from(r"C:\Windows\Sysnative\curl.exe"),
    ] {
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// curl 报错里若出现 schannel / (35)，多半是「服务端地址误写成 https、但服务器只讲
/// HTTP」导致 TLS 去握一个明文端口。补一句人话提示，省得用户再排查一轮。
fn enrich_tls_error(stderr: &str) -> String {
    let base = stderr.trim().to_owned();
    if stderr.contains("schannel") || stderr.contains("(35)") {
        format!(
            "{base}\n（TLS 握手失败：更新服务器是普通 HTTP，请确认「服务端地址」是否误写成 https://，应为 http://...）"
        )
    } else {
        base
    }
}

/// 用系统 curl（没有就退回 PowerShell）把 url 下载到 dest。
/// 内网地址绕过系统代理（http_proxy 环境变量会让内网请求连不出去）。
fn fetch(url: &str, dest: &Path) -> Result<(), String> {
    let noproxy = is_private_host(url);
    if let Some(curl) = find_curl() {
        let mut cmd = std::process::Command::new(&curl);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd.args(["-fsSL", "--connect-timeout", "10", "--max-time", "120"]);
        if noproxy {
            cmd.arg("--noproxy").arg("*");
        }
        let output = cmd
            .arg("-o")
            .arg(dest)
            .arg(url)
            .output()
            .map_err(|e| format!("无法启动 {}：{e}", curl.display()))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        return Err(if stderr.is_empty() {
            format!("下载失败（curl 退出码 {}）", output.status)
        } else {
            format!("下载失败：{}", enrich_tls_error(stderr))
        });
    }
    // Invoke-WebRequest 5.1 没有 -NoProxy，清掉默认代理对象即可
    let clear_proxy = if noproxy {
        "[System.Net.WebRequest]::DefaultWebProxy=$null;"
    } else {
        ""
    };
    let mut ps = std::process::Command::new("powershell");
    #[cfg(windows)]
    ps.creation_flags(CREATE_NO_WINDOW);
    let output = ps
        .args(["-NoProfile", "-Command"])
        .arg(format!(
            "$ProgressPreference='SilentlyContinue';{clear_proxy}\
             [Net.ServicePointManager]::SecurityProtocol=[Net.SecurityProtocolType]::Tls12;\
             Invoke-WebRequest -UseBasicParsing -Uri '{url}' -OutFile '{}'",
            dest.display()
        ))
        .output()
        .map_err(|e| format!("无法启动 powershell：{e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    Err(if stderr.is_empty() {
        "下载失败（PowerShell）".to_owned()
    } else {
        format!("下载失败：{stderr}")
    })
}

/// 从服务端拉 latest.json 并解析出最新版本信息。
/// 返回的 manifest.url 已拼成完整下载地址。
pub fn check(server: &str) -> Result<UpdateManifest, String> {
    if server.trim().is_empty() {
        return Err("未配置更新服务端地址".to_owned());
    }
    let url = manifest_url(server);
    let dest = std::env::temp_dir().join("svn_manager_latest.json");
    let _ = std::fs::remove_file(&dest);
    fetch(&url, &dest)?;
    let text = std::fs::read_to_string(&dest)
        .map_err(|e| format!("读取版本信息失败：{e}"))?;
    let _ = std::fs::remove_file(&dest);
    // 用记事本等编辑过的响应可能带 BOM
    let text = text.trim_start_matches('\u{feff}');
    let manifest: UpdateManifest = serde_json::from_str(text)
        .map_err(|e| format!("解析版本信息失败：{e}（{url}）"))?;
    // 服务端写 V1.2.4 也认：剥掉前缀再交给界面（界面各处自己加「V」）
    let version = version_text(&manifest.version);
    if version.is_empty() {
        return Err("服务端版本号为空".to_owned());
    }
    if manifest.url.trim().is_empty() {
        return Err("服务端未提供下载地址（url 字段）".to_owned());
    }
    let full = join_url(server, &manifest.url);
    Ok(UpdateManifest { version, url: full, ..manifest })
}

/// GitHub `releases/latest` 响应里我们要用到的那几项，其余字段忽略。
#[derive(Deserialize)]
struct GitHubAsset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: String,
    /// GitHub 对发布资产给出的校验值，形如 `sha256:<hex>`；老资产可能没有
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Deserialize)]
struct GitHubRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    published_at: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<GitHubAsset>,
}

/// 从 `start` 处（`[` 或 `!`）解出一个 `[文字](地址)`：返回方括号内文字的区间，
/// 以及整段链接之后的下标。方括号后面没跟圆括号就当普通方括号，不剥。
fn link_text(chars: &[char], start: usize) -> Option<(Range<usize>, usize)> {
    let mut index = start;
    if chars[index] == '!' {
        index += 1;
    }
    if chars.get(index) != Some(&'[') {
        return None;
    }
    let close = (index + 1..chars.len()).find(|&i| chars[i] == ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = (close + 2..chars.len()).find(|&i| chars[i] == ')')?;
    Some((index + 1..close, end + 1))
}

/// 单行剥标记。
fn plain_line(line: &str) -> String {
    let mut text = line;
    // 标题：1~6 个 `#` 且紧跟空格才算，`#123` 这种 issue 号不动
    let hashes = text.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && text[hashes..].starts_with(' ') {
        text = text[hashes + 1..].trim_start();
    }
    // 邮件式引用
    while let Some(rest) = text.strip_prefix('>') {
        text = rest.trim_start();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            // 强调（* ~~）与行内代码的标记只丢符号本身；`_` 留着——仓库名里全是下划线
            '`' | '*' | '~' => index += 1,
            '[' | '!' => match link_text(&chars, index) {
                Some((label, next)) => {
                    out.extend(chars[label].iter().copied());
                    index = next;
                }
                None => {
                    out.push(chars[index]);
                    index += 1;
                }
            },
            c => {
                out.push(c);
                index += 1;
            }
        }
    }
    out
}

/// Release 正文是 markdown，界面按纯文本渲染会把 `##`、`**`、`` ` ``、`[文字](链接)` 这些
/// 标记原样露出来。这里只剥标记、不动内容，列表的 `-` 保留（那本来就是纯文本的一部分）。
pub fn plain_text(markdown: &str) -> String {
    let mut kept: Vec<String> = Vec::new();
    for raw in markdown.lines() {
        let line = raw.trim_end();
        // 代码围栏整行丢掉，里面的内容当普通文字留着
        if line.starts_with("```") || line.starts_with("~~~") {
            continue;
        }
        // 只由标点组成的分隔线没有信息量
        if line.chars().count() >= 3 && line.chars().all(|c| matches!(c, '-' | '=' | '*')) {
            continue;
        }
        kept.push(plain_line(line));
    }
    // 连续空行压成一行：正文里列表项之间常夹空行，界面上排一排空行太占地方
    let mut out = String::new();
    let mut last_blank = true;
    for line in kept {
        let blank = line.trim().is_empty();
        if blank && last_blank {
            continue;
        }
        last_blank = blank;
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&line);
    }
    out.trim().to_owned()
}

/// 解析 `releases/latest` 的响应：版本号取 tag（剥掉 `v` 前缀），下载地址取资产里第一个
/// `.exe`，GitHub 给的 sha256 digest 直接当校验值（接上原有那条 certutil 校验链）。
fn parse_release(text: &str) -> Result<UpdateManifest, String> {
    let release: GitHubRelease = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("解析 GitHub 发布信息失败：{e}"))?;
    // tag 普遍写成 `v1.2.0`，而界面各处都自己加「V」前缀，这里不剥掉就会显示成 Vv1.2.0
    let version = version_text(&release.tag_name);
    if version.is_empty() {
        return Err("GitHub 最新发布没有版本号（tag_name）".to_owned());
    }
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name.trim().to_ascii_lowercase().ends_with(".exe"))
        .ok_or_else(|| format!("GitHub 最新发布 {version} 里没有 .exe 资产，没法自动更新"))?;
    if asset.browser_download_url.trim().is_empty() {
        return Err("GitHub 最新发布的 .exe 资产没有下载地址".to_owned());
    }
    let sha256 = asset
        .digest
        .as_deref()
        .unwrap_or_default()
        .trim()
        .strip_prefix("sha256:")
        .unwrap_or_default()
        .to_ascii_lowercase();
    Ok(UpdateManifest {
        version,
        url: asset.browser_download_url.trim().to_owned(),
        notes: plain_text(release.body.unwrap_or_default().as_str()),
        sha256,
        // 响应里是 `2026-09-08T03:34:21Z`，界面只显示发布日那天就够
        published_at: release
            .published_at
            .split('T')
            .next()
            .unwrap_or_default()
            .to_owned(),
    })
}

/// GitHub 匿名接口按 IP 限流（每小时 60 次），403 / 404 的原始报错看不出所以然，补一句人话。
fn enrich_github_error(message: String) -> String {
    if message.contains("403") {
        return format!("{message}\n（GitHub 匿名接口按 IP 限流，每小时 60 次；稍后再试，或在「设置 → 版本更新」改用自定义源）");
    }
    if message.contains("404") {
        return format!("{message}\n（读不到最新发布：仓库还没有发布过 Release，或仓库地址写错了）");
    }
    message
}

/// 从 GitHub 取最新发布。返回的 manifest.url 是资产的 `browser_download_url`（绝对地址）。
pub fn check_github() -> Result<UpdateManifest, String> {
    let dest = std::env::temp_dir().join("svn_manager_github_latest.json");
    let _ = std::fs::remove_file(&dest);
    fetch(OFFICIAL_API, &dest).map_err(enrich_github_error)?;
    let text = std::fs::read_to_string(&dest)
        .map_err(|e| format!("读取 GitHub 发布信息失败：{e}"))?;
    let _ = std::fs::remove_file(&dest);
    parse_release(&text)
}

/// 按配置选中的更新源检查一次；`server` 只在自定义源时用到。
pub fn check_from(source: &str, server: &str) -> Result<UpdateManifest, String> {
    if is_official(source) {
        check_github()
    } else {
        check(server)
    }
}

/// 把 url 下载到 dest（用于新版本 exe），返回下载字节数。
/// 内网地址绕过系统代理（http_proxy 环境变量会让内网请求连不出去）。
/// 下载在程序内完成而不是丢给更新脚本：一来日志区能看到进度与结果，
/// 二来 bat 不用带着「下载几十秒」的可疑特征在磁盘上久留（会被安全软件查删，
/// cmd 逐行读脚本、读到一半文件没了就报「找不到批处理文件」）。
pub fn download(url: &str, dest: &Path) -> Result<u64, String> {
    let noproxy = is_private_host(url);
    if let Some(curl) = find_curl() {
        let mut cmd = std::process::Command::new(&curl);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd.args(["-fsSL", "--connect-timeout", "10", "--max-time", "600"]);
        if noproxy {
            cmd.arg("--noproxy").arg("*");
        }
        let output = cmd
            .arg("-o")
            .arg(dest)
            .arg(url)
            .args(["-w", "%{size_download}"])
            .output()
            .map_err(|e| format!("无法启动 {}：{e}", curl.display()))?;
        if output.status.success() {
            let size = String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse::<u64>()
                .unwrap_or(0);
            return Ok(size);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        return Err(if stderr.is_empty() {
            format!("下载失败（curl 退出码 {}）", output.status)
        } else {
            format!("下载失败：{}", enrich_tls_error(stderr))
        });
    }
    // Invoke-WebRequest 5.1 没有 -NoProxy，清掉默认代理对象即可
    let clear_proxy = if noproxy {
        "[System.Net.WebRequest]::DefaultWebProxy=$null;"
    } else {
        ""
    };
    let mut ps = std::process::Command::new("powershell");
    #[cfg(windows)]
    ps.creation_flags(CREATE_NO_WINDOW);
    let output = ps
        .args(["-NoProfile", "-Command"])
        .arg(format!(
            "$ProgressPreference='SilentlyContinue';{clear_proxy}\
             [Net.ServicePointManager]::SecurityProtocol=[Net.SecurityProtocolType]::Tls12;\
             Invoke-WebRequest -UseBasicParsing -Uri '{url}' -OutFile '{}'",
            dest.display()
        ))
        .output()
        .map_err(|e| format!("无法启动 powershell：{e}"))?;
    if output.status.success() {
        return Ok(std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    Err(if stderr.is_empty() {
        "下载失败（PowerShell）".to_owned()
    } else {
        format!("下载失败：{stderr}")
    })
}

/// 用系统自带的 certutil 算文件 SHA256（64 位小写 hex），用于校验下载的新 exe。
pub fn sha256_of(path: &Path) -> Result<String, String> {
    let mut ct = std::process::Command::new("certutil");
    #[cfg(windows)]
    ct.creation_flags(CREATE_NO_WINDOW);
    let output = ct
        .args(["-hashfile"])
        .arg(path)
        .arg("SHA256")
        .output()
        .map_err(|e| format!("无法启动 certutil：{e}"))?;
    if !output.status.success() {
        return Err(format!("certutil 失败（退出码 {}）", output.status));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // hash 行是 64 位 hex（旧版 certutil 里可能带空格），逐行找而不依赖行序
    for line in text.lines() {
        let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        if compact.len() == 64 && compact.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(compact.to_lowercase());
        }
    }
    Err("certutil 输出中没有校验值".to_owned())
}

/// 运行中自动检查更新的间隔（秒）。GitHub 匿名接口按 IP 限流 60 次/小时，
/// 5 分钟一次只有 12 次/小时，余量都留给手动「检查更新」。
pub const AUTO_CHECK_EVERY_SECS: u64 = 300;

/// 暂存的新版 exe 的文件名。固定 ASCII：脚本里只出现这个名字（配合 `%~dp0`），
/// 中文目录因此完全不影响 cmd 读脚本。
pub const STAGED_EXE: &str = "svn_manager_update.exe";

/// 收尾 bat 里用到的两个文件名走环境变量（由 [`apply_env`] 设置）：
/// 脚本内容因此永远保持纯 ASCII，中文目录 / 中文文件名都由 cmd 在内存里按 UTF-16 展开。
pub const TARGET_ENV: &str = "SVN_MGR_TARGET";
pub const STAGED_ENV: &str = "SVN_MGR_STAGED";

/// 生成应用更新的收尾 bat：杀残留实例 → 等主程序退出 → 覆盖原 exe
/// （被占用则重试，上限 15 次）→ 删暂存文件 → 重启 → bat 自删。
///
/// 为什么不把路径写进脚本：cmd 是按控制台代码页把 .bat 当**文本**读的，脚本里一旦出现
/// 中文目录或中文文件名就乱码，「从哪覆盖到哪」全错——这正是「更新器不支持中文目录」的根因。
/// 所以：目录用 `%~dp0`（cmd 自己展开），文件名用环境变量（Rust 以 UTF-16 传给子进程），
/// 脚本本身永远是纯 ASCII；环境变量没设时退回下面两个默认名，手工重跑脚本也照样能覆盖。
/// 新版 exe 由调用方先暂存到程序目录、名字固定为 [`STAGED_EXE`]。
pub fn build_apply_bat() -> String {
    format!(
        r#"@echo off
setlocal
title SVN Manager Update
if "%{target_env}%"=="" set "{target_env}=svn_manager.exe"
if "%{staged_env}%"=="" set "{staged_env}={staged}"
set /a TRIES=0

echo Applying SVN Manager update ...
taskkill /f /im "%{target_env}%" >nul 2>&1
{WIN_WAIT} /t 2 /nobreak >nul

:copy_retry
copy /y "%~dp0%{staged_env}%" "%~dp0%{target_env}%" >nul 2>&1
if not errorlevel 1 goto copy_ok
set /a TRIES+=1
if %TRIES% GEQ 15 goto copy_fail
echo File is locked, retrying (%TRIES%/15) ...
{WIN_WAIT} /t 2 /nobreak >nul
goto copy_retry

:copy_ok
del "%~dp0%{staged_env}%" >nul 2>&1
start "" "%~dp0%{target_env}%"
del "%~f0" & exit 0

:copy_fail
echo Cannot overwrite "%{target_env}%" in this folder (file locked).
echo Re-run this script to retry: %~f0
pause
exit 1
"#,
        staged = STAGED_EXE,
        target_env = TARGET_ENV,
        staged_env = STAGED_ENV,
        // 必须写全路径：PATH 里若混进 Git Bash / MinGW 的 usr/bin（从 Git Bash
        // 启动本程序就会），裸写 timeout 会命中 GNU coreutils 的 timeout，
        // 报 "invalid time interval '/t'" 并立刻返回——重试循环会瞬间打完 15 次，
        // 更新必然卡在「文件被占用」。
        WIN_WAIT = r"%SystemRoot%\System32\timeout.exe",
    )
}

/// 把「要覆盖哪个 exe」告诉收尾脚本：文件名走环境变量，中文名也不会被代码页搞坏。
pub fn apply_env<'a>(
    command: &'a mut std::process::Command,
    target: &Path,
) -> &'a mut std::process::Command {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "svn_manager.exe".to_owned());
    command.env(TARGET_ENV, name).env(STAGED_ENV, STAGED_EXE)
}

/// 启动收尾脚本：目录由脚本自己用 `%~dp0` 认（cmd 在内存里展开），
/// 文件名由 [`apply_env`] 从环境变量带过去。
pub fn apply_launcher(bat: &Path, target: &Path) -> std::process::Command {
    let mut command = std::process::Command::new("cmd");
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    apply_env(&mut command, target);
    // `start` 会给脚本另起一个控制台窗口：不闪主程序的框，覆盖失败时的 pause 也看得见
    command.args(["/C", "start", "", &bat.to_string_lossy()]);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 隐藏窗口（CREATE_NO_WINDOW）后 certutil 仍然要能正常取到哈希，
    /// 否则更新校验会静默失败——这条是真机回归。
    #[test]
    #[cfg(windows)]
    fn sha256_of_works_with_hidden_window() {
        let path = std::env::temp_dir().join("svn_manager_hash_test.bin");
        std::fs::write(&path, b"abc").unwrap();
        let got = sha256_of(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            got,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// 真机演练收尾 bat：用无害替身跑完整一遍「杀进程 → 覆盖 → 重启 → 删除临时
    /// 文件 → 自删」。这是更新链里最脆的一段（早先出过「未找到批处理文件」），
    /// 默认跳过，手动跑：`cargo test -- --ignored apply_bat`
    #[test]
    #[ignore]
    fn apply_bat_replaces_target_and_cleans_up() {
        let dir = std::env::temp_dir().join("svn_manager_bat_probe");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 目标程序用 rundll32 的副本：启动即退出，不会弹界面
        let target = dir.join("fake_target.exe");
        std::fs::copy(r"C:\Windows\System32\rundll32.exe", &target).unwrap();
        // 新版 exe 由程序自己暂存到目标隔壁（名字固定 STAGED_EXE），bat 只认 %~dp0 + 这个名字
        let staged = dir.join(STAGED_EXE);
        std::fs::write(&staged, b"new-binary-bytes").unwrap();

        let bat = dir.join("apply.bat");
        std::fs::write(&bat, build_apply_bat().as_bytes()).unwrap();
        let mut command = std::process::Command::new("cmd");
        command
            .args(["/C", &bat.to_string_lossy()])
            .stdin(std::process::Stdio::null());
        apply_env(&mut command, &target);
        let out = command.output().unwrap();
        // 不看退出码：bat 最后一步是 del "%~f0" 自删，cmd 读不到后续行时
        // 退出码并不总是 0（真实流程里主程序早已退出，这个值无人关心）。
        // 真正要守住的是下面三个效果和「等待命令没被 GNU timeout 抢走」。
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(
            !stderr.contains("invalid time interval"),
            "等待命令被 GNU coreutils 的 timeout 抢走了（要写全 System32 路径）：{stderr}"
        );
        assert!(!stderr.contains("Cannot overwrite"), "覆盖失败：{stderr}");

        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"new-binary-bytes".to_vec(),
            "目标程序未被新版本覆盖"
        );
        assert!(!staged.exists(), "暂存的新版 exe 没有被清理");
        // del "%~f0" 在脚本末尾执行，进程结束后文件应已消失（等一小会儿）
        for _ in 0..20 {
            if !bat.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(!bat.exists(), "批处理文件没有自删");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真机演练中文目录：程序装在中文目录、连 exe 都改成了中文名时，收尾 bat 仍要
    /// 照常覆盖并重启。用户报的「更新器不支持中文目录」就是这个场景的回归。
    /// 默认跳过，手动跑：`cargo test -- --ignored apply_bat`
    #[test]
    #[ignore]
    fn apply_bat_works_in_a_chinese_directory() {
        let dir = std::env::temp_dir().join("svn管理器_中文目录测试");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("测试程序.exe");
        std::fs::copy(r"C:\Windows\System32\rundll32.exe", &target).unwrap();
        let staged = dir.join(STAGED_EXE);
        std::fs::write(&staged, b"new-binary-bytes").unwrap();

        let bat = dir.join("apply.bat");
        let text = build_apply_bat();
        assert!(
            text.is_ascii(),
            "中文目录与中文名都不能进脚本（走 %~dp0 与环境变量）：\n{text}"
        );
        assert!(!text.contains("测试程序"), "目标名不该出现在脚本里");
        std::fs::write(&bat, text.as_bytes()).unwrap();
        let mut command = std::process::Command::new("cmd");
        command
            .args(["/C", &bat.to_string_lossy()])
            .stdin(std::process::Stdio::null());
        apply_env(&mut command, &target);
        let out = command.output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(!stderr.contains("Cannot overwrite"), "覆盖失败：{stderr}");
        assert!(!stdout.contains("batch file"), "脚本没能读完：{stdout}{stderr}");
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"new-binary-bytes".to_vec(),
            "中文目录里的目标程序没被覆盖"
        );
        assert!(!staged.exists(), "暂存的新版 exe 没有被清理");
        for _ in 0..20 {
            if !bat.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(!bat.exists(), "批处理文件没有自删");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真实启动方式是 `start 脚本`，而 start 打开 .bat 等价于 `cmd /K 脚本`：
    /// 脚本自删后若只是返回（`exit /b`），cmd 会回到已经不存在的脚本上打印
    /// 「找不到批处理文件。」并留下一个空窗口。这条按 /K 复现，守住「结尾必须 exit」。
    /// 默认跳过，手动跑：`cargo test -- --ignored apply_bat`
    #[test]
    #[ignore]
    fn apply_bat_ends_the_host_cmd_process() {
        let dir = std::env::temp_dir().join("svn_manager_bat_k");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("fake_target.exe");
        std::fs::copy(r"C:\Windows\System32\rundll32.exe", &target).unwrap();
        std::fs::write(dir.join(STAGED_EXE), b"new-binary-bytes").unwrap();
        let bat = dir.join("apply.bat");
        std::fs::write(&bat, build_apply_bat().as_bytes()).unwrap();
        // stdin 给空设备：万一脚本又只是返回，cmd 会读完输入直接退出而不是挂住等人按键
        let mut command = std::process::Command::new("cmd");
        command
            .args(["/K", &bat.to_string_lossy()])
            .stdin(std::process::Stdio::null());
        apply_env(&mut command, &target);
        let out = command.output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !text.contains("找不到批处理文件") && !text.to_lowercase().contains("batch file"),
            "脚本自删后 cmd 又回去读已删除的脚本（结尾要用 exit 结束进程）：{text}"
        );
        assert!(!bat.exists(), "批处理文件没有自删");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真机冒烟：默认跳过，需要联网时手动跑，用于验证隐藏窗口没把 curl 弄坏、
    /// 以及内网地址是否真能连通（排查过代理劫持导致 curl 28 的现场）：
    ///
    /// ```text
    /// SVN_UPDATE_TEST_URL=http://192.168.1.251:20700 cargo test -- --ignored
    /// ```
    #[test]
    #[ignore]
    fn check_real_server_smoke() {
        let Ok(server) = std::env::var("SVN_UPDATE_TEST_URL") else {
            return; // 没给地址就当跳过，不算失败
        };
        let info = check(&server).unwrap_or_else(|e| panic!("检查更新失败：{e}"));
        assert!(!info.version.trim().is_empty(), "服务端版本号为空");
        assert!(info.url.starts_with("http"), "下载地址未拼成绝对地址：{}", info.url);
    }

    /// 官方源冒烟：真的去读 GitHub 的最新发布（默认跳过）
    /// `cargo test -- --ignored check_github_smoke`
    #[test]
    #[ignore]
    fn check_github_smoke() {
        let info = check_github().unwrap_or_else(|e| panic!("读 GitHub 最新发布失败：{e}"));
        assert!(!info.version.trim().is_empty(), "最新发布没有版本号");
        assert!(
            info.url.starts_with("https://github.com/") || info.url.contains("releases/download"),
            "下载地址不像 GitHub 资产：{}",
            info.url
        );
        // 真实正文里不该再留下 markdown 标记
        for unwanted in ["## ", "**", "](http"] {
            assert!(!info.notes.contains(unwanted), "更新说明里还有「{unwanted}」");
        }
        println!("NOTES 开头：{}", info.notes.chars().take(160).collect::<String>());
    }

    /// GitHub `releases/latest` 响应里真正会被读到的那几项（抓下来的真实数据）。
    /// 定界要用三层井号：正文里的 `"## 更新内容` 恰好等于 `r##"` 的终止符。
    const RELEASE_FIXTURE: &str = r###"{
        "tag_name": "v1.2.0",
        "name": "v1.2.0 发布",
        "published_at": "2026-09-08T03:34:21Z",
        "body": "## 更新内容\n- 修了点什么",
        "draft": false,
        "prerelease": false,
        "assets": [
            {
                "name": "svn_manager.exe",
                "size": 9992192,
                "digest": "sha256:fa100735bf7a1dcfa9db0a5ea28dffd0ddff9f5165397c795d3825fd243ca3d2",
                "browser_download_url": "https://github.com/moonrabbiiit/SVN-Manager/releases/download/v1.2.0/svn_manager.exe"
            }
        ]
    }"###;

    #[test]
    fn github_release_becomes_a_manifest() {
        let info = parse_release(RELEASE_FIXTURE).expect("该能解析真实响应");
        // tag 上的 v 前缀在这里就剥掉：界面各处自己加「V」，留着会变成 Vv1.2.0
        assert_eq!(info.version, "1.2.0");
        assert!(is_newer(&info.version, "1.1.9"));
        assert_eq!(
            info.url,
            "https://github.com/moonrabbiiit/SVN-Manager/releases/download/v1.2.0/svn_manager.exe"
        );
        // GitHub 给的 `sha256:<hex>` 剥掉前缀，正好喂给现有的 certutil 校验
        assert_eq!(
            info.sha256,
            "fa100735bf7a1dcfa9db0a5ea28dffd0ddff9f5165397c795d3825fd243ca3d2"
        );
        // 界面只展示发布日那天，不带时分秒
        assert_eq!(info.published_at, "2026-09-08");
        // 正文是 markdown，标题符号在解析时就该剥干净
        assert_eq!(info.notes, "更新内容\n- 修了点什么", "实际：{:?}", info.notes);
    }

    /// Release 正文是 markdown，界面按纯文本渲染：标记要剥掉，但内容一个字符都不能伤
    #[test]
    fn release_notes_are_stripped_to_plain_text() {
        let markdown = "## AI 工作日志（重点更新）\n\
            \n\
            - **入口改为右上角开关式**：`AI 日志` 按钮常驻\n\
            - 多个目录（如 hrp_server + vue_ss_server）反复勾选\n\
            \n\
            ***\n\
            \n\
            > 详见 [说明文档](https://x/y)\n\
            ![截图](https://x/z.png)\n\
            \n\
            #123 不是标题\n\
            ```\n\
            代码块里的字留着\n\
            ```";
        let text = plain_text(markdown);
        assert!(text.starts_with("AI 工作日志（重点更新）"), "{text}");
        assert!(
            text.contains("- 入口改为右上角开关式：AI 日志 按钮常驻"),
            "强调与行内代码符号要没，列表短横留着：{text}"
        );
        // 下划线是仓库名的一部分，不能当强调符号吃掉
        assert!(text.contains("hrp_server + vue_ss_server"), "{text}");
        assert!(text.contains("详见 说明文档"), "链接只留文字：{text}");
        assert!(text.contains("截图"), "图片只留说明文字：{text}");
        assert!(text.contains("代码块里的字留着"), "围栏内容当普通文字留着：{text}");
        for unwanted in ["##", "**", "`", "](http", ">", "```", "***"] {
            assert!(!text.contains(unwanted), "还留着「{unwanted}」：{text}");
        }
        // issue 号不是标题，井号得原样留着
        assert!(text.contains("#123 不是标题"), "{text}");
        // 一个空行在字符串里就是两个换行；要压掉的是连续两个以上的空行和那条分隔线
        assert!(!text.contains("\n\n\n"), "{text}");
        assert!(!text.contains("***"), "分隔线该整行去掉：{text}");
        assert_eq!(plain_text(&text), text, "剥过一次就该稳定，别二次损伤");
    }

    /// 只发源码没传 exe 的话，自动更新无从下手，要说清是哪一次发布缺东西
    #[test]
    fn github_release_without_an_exe_asset_is_rejected() {
        let text = r#"{"tag_name":"v1.3.0","assets":[{"name":"source.zip","browser_download_url":"https://x/y.zip"}]}"#;
        let err = parse_release(text).expect_err("没有 exe 资产不该算成功");
        assert!(err.contains("1.3.0"), "报错要指出是哪次发布：{err}");
        assert!(err.contains(".exe"), "报错要说清缺什么：{err}");
        // 版本号为空、资产有 exe 但没给下载地址，也都要拦住
        assert!(parse_release(r#"{"tag_name":"  ","assets":[]}"#).is_err());
        assert!(
            parse_release(r#"{"tag_name":"v1.3.0","assets":[{"name":"a.exe","browser_download_url":" "}]}"#).is_err()
        );
    }

    /// 早先上传的资产可能没有 digest 字段：没给就不校验，而不是当解析失败
    #[test]
    fn github_digest_is_optional() {
        let info = parse_release(
            r#"{"tag_name":"v1.2.0","assets":[{"name":"SVN_MANAGER.EXE","browser_download_url":"https://x/y.EXE"}]}"#,
        )
        .expect("没有 digest 也要能解析");
        assert_eq!(info.sha256, "", "没给校验值就留空");
        // 资产名大小写不限，下载地址原样带回来
        assert_eq!(info.url, "https://x/y.EXE");
    }

    #[test]
    fn source_choice_routes_the_check() {
        // 官方源不依赖任何地址；只有自定义源仍然要求填地址
        assert!(is_official(SOURCE_OFFICIAL));
        assert!(is_official(""), "老配置里没有这个字段要按官方算");
        assert!(is_official("别写错的值"), "认不全的值也退回官方源而不是查一个空地址");
        assert!(!is_official(SOURCE_CUSTOM));
        assert!(check_from(SOURCE_CUSTOM, "  ").is_err());
    }

    #[test]
    fn github_http_errors_get_a_human_hint() {
        assert!(enrich_github_error("下载失败：The requested URL returned error: 403".to_owned())
            .contains("限流"));
        assert!(enrich_github_error("下载失败：The requested URL returned error: 404".to_owned())
            .contains("Release"));
        // 超时之类的原因不硬扯到限流，免得误导
        let plain = enrich_github_error("下载失败：curl: (28) Connection timed out".to_owned());
        assert_eq!(plain, "下载失败：curl: (28) Connection timed out");
    }

    #[test]
    fn version_segments_are_parsed() {
        assert_eq!(parse_version("1.1.1"), vec![1, 1, 1]);
        assert_eq!(parse_version("V1.1.1"), vec![1, 1, 1]);
        assert_eq!(parse_version("v2.0"), vec![2, 0]);
        assert_eq!(parse_version("正式版 1.2.3"), vec![1, 2, 3]);
        assert_eq!(parse_version("10.20.30"), vec![10, 20, 30]);
        assert!(parse_version("无数字").is_empty());
    }

    #[test]
    fn newer_versions_are_detected() {
        assert!(is_newer("1.1.2", "1.1.1"));
        assert!(is_newer("V1.2.0", "1.1.9"));
        assert!(is_newer("1.2", "1.1.9"), "短的按 0 补齐");
        assert!(is_newer("1.10", "1.9.9"), "数字段按数值比较，不是字符串");
        assert!(!is_newer("1.1.1", "1.1.1"), "相同版本不更新");
        assert!(!is_newer("1.1", "1.1.0"), "补 0 后相等不算新");
        assert!(!is_newer("1.0.9", "1.1.0"));
    }

    /// 版本号从源里取出来时统一剥掉 v / V 前缀：界面各处自己加「V」，留着会变成 Vv1.2.4
    #[test]
    fn version_prefixes_are_stripped() {
        assert_eq!(version_text("  v1.2.4 "), "1.2.4");
        assert_eq!(version_text("V1.2.4"), "1.2.4");
        assert_eq!(version_text("1.2.4"), "1.2.4");
        assert_eq!(version_text("  "), "");
    }

    /// 「版本号没变」要能认出来：窗口据此不说「旧 → 新」那套
    #[test]
    fn same_version_ignores_a_missing_segment() {
        assert!(same_version("1.2", "1.2.0"));
        assert!(same_version("V1.2.4", "1.2.4"));
        assert!(!same_version("1.2.4", "1.2.5"));
        assert!(!same_version("无数字", "无数字"), "两边都解析不出数字就不算同一版");
    }

    /// 判定以文件为准：源上的 sha256 与本地 exe 不同就算有新版本（同一个版本号
    /// 重新发布的构建也能发现）；源没给 sha256 才退回版本号比较。
    #[test]
    fn update_is_decided_by_the_file_hash() {
        let manifest = |sha: &str, version: &str| UpdateManifest {
            version: version.to_owned(),
            url: "http://x/y.exe".to_owned(),
            notes: String::new(),
            sha256: sha.to_owned(),
            published_at: String::new(),
        };
        // 本地 exe 的真实哈希：与源上一致就不算更新，哪怕源上的版本号写着 1.0.0
        let mine = sha256_of(&std::env::current_exe().expect("测试进程自己的 exe")).unwrap();
        assert!(!has_update(&manifest(&mine, "1.0.0"), "9.9.9"));
        assert!(
            !has_update(&manifest(&mine.to_uppercase(), "1.0.0"), "9.9.9"),
            "大小写不同的同一个哈希不该算成新版本"
        );
        // 换过一次构建：版本号没动也算更新
        assert!(has_update(&manifest(&"0f".repeat(32), "1.2.4"), "1.2.4"));
        // 源没给 sha256（早先上传的老资产）：退回版本号比较
        assert!(has_update(&manifest("", "1.2.5"), "1.2.4"));
        assert!(!has_update(&manifest("", "1.2.4"), "1.2.4"));
    }

    #[test]
    fn urls_are_joined() {
        assert_eq!(
            join_url("http://a.b:80/c", "files/x.exe"),
            "http://a.b:80/c/files/x.exe"
        );
        assert_eq!(
            join_url("http://a.b:80/c/", "/files/x.exe"),
            "http://a.b:80/c/files/x.exe"
        );
        assert_eq!(
            join_url("http://a.b/c", "http://c.d/x.exe"),
            "http://c.d/x.exe",
            "绝对地址原样保留"
        );
        assert_eq!(
            join_url("http://a.b/c", "https://c.d/x.exe"),
            "https://c.d/x.exe"
        );
    }

    #[test]
    fn manifest_url_prefers_full_json_path() {
        assert_eq!(manifest_url("http://a.b"), "http://a.b/latest.json");
        assert_eq!(manifest_url("http://a.b/"), "http://a.b/latest.json");
        // 直接配到 json 文件也支持
        assert_eq!(manifest_url("http://a.b/x/ver.json"), "http://a.b/x/ver.json");
    }

    #[test]
    fn check_rejects_empty_server() {
        assert!(check("  ").is_err());
    }

    #[test]
    fn private_hosts_bypass_proxy() {
        // RFC1918 私网与本机地址必须绕过代理
        assert!(is_private_host("http://192.168.1.251:20700/latest.json"));
        assert!(is_private_host("http://10.0.0.2/x.exe"));
        assert!(is_private_host("http://172.16.0.1/"));
        assert!(is_private_host("http://172.31.255.255/"));
        assert!(is_private_host("http://localhost:8666/"));
        assert!(is_private_host("http://127.0.0.1:9000/latest.json"));
        assert!(is_private_host("http://[::1]:8666/latest.json"));
        // 不带点的主机名（http://nas:8666）也当内网
        assert!(is_private_host("http://nas:8666/latest.json"));
        assert!(is_private_host("http://svr/x.exe"));
        // 公网地址照常走系统代理
        assert!(!is_private_host("https://example.com/latest.json"));
        assert!(!is_private_host("http://172.32.0.1/"), "172 段只有 16-31 是私网");
        assert!(!is_private_host("http://11.0.0.1/"));
        assert!(!is_private_host("http://192.169.0.1/"));
    }

    #[test]
    fn tls_error_gets_a_human_hint() {
        let msg = enrich_tls_error(
            "curl: (35) schannel: next InitializeSecurityContext failed: SEC_E_INVALID_TOKEN",
        );
        assert!(
            msg.contains("http://"),
            "schannel 报错应提示把地址改回 http：{msg}"
        );
        // 非 TLS 类错误（如代理超时）不应加 https 提示，避免误导
        let plain = enrich_tls_error("curl: (28) Connection timed out after 10001 milliseconds");
        assert!(
            !plain.contains("TLS 握手失败"),
            "非 TLS 错误不应加 https 提示：{plain}"
        );
    }

    #[test]
    fn apply_bat_contains_the_whole_swap_flow() {
        let bat = build_apply_bat();
        // 覆盖目标、杀残留、重试、重启、自删
        assert!(
            bat.contains(r#"copy /y "%~dp0%SVN_MGR_STAGED%" "%~dp0%SVN_MGR_TARGET%""#),
            "{bat}"
        );
        assert!(bat.contains(r#"taskkill /f /im "%SVN_MGR_TARGET%""#));
        assert!(bat.contains("goto copy_retry"));
        assert!(bat.contains(r#"start "" "%~dp0%SVN_MGR_TARGET%""#));
        assert!(bat.contains(r#"del "%~f0""#));
        // 下载与校验已在程序内完成，bat 里不应再出现
        assert!(!bat.contains("curl"), "下载已在程序内完成");
        assert!(!bat.contains("certutil"), "校验已在程序内完成");
        // 脚本必须全 ASCII：中文目录与中文文件名一律走 %~dp0 与环境变量，
        // 不经过控制台代码页，这是「更新器不支持中文目录」的解法
        assert!(bat.is_ascii(), "bat 输出必须全 ASCII");
        // 环境变量没设时退回默认名：手工重跑脚本也照样能覆盖
        assert!(bat.contains(r#"if "%SVN_MGR_TARGET%"=="" set "SVN_MGR_TARGET=svn_manager.exe""#));
        assert!(bat.contains(r#"if "%SVN_MGR_STAGED%"=="" set "SVN_MGR_STAGED=svn_manager_update.exe""#));
    }

    /// 中文目录曾让更新器直接失效：脚本里写着绝对路径，而 cmd 是按控制台代码页把 .bat
    /// 当文本读的，中文一进脚本就乱码，「从哪覆盖到哪」全错。
    /// 现在脚本内容与真实路径彻底解耦——目标名只从 apply_env 的环境变量来（UTF-16）。
    #[test]
    fn apply_bat_never_contains_the_target_path() {
        let bat = build_apply_bat();
        assert!(bat.is_ascii(), "{bat}");
        assert!(!bat.contains("程序") && !bat.contains(r"D:\"), "路径不该进脚本：\n{bat}");
        let mut command = std::process::Command::new("cmd");
        apply_env(&mut command, Path::new(r"D:\程序\SVN 管理器\SVN管理器.exe"));
        let vars: Vec<(String, String)> = command
            .get_envs()
            .filter(|(key, _)| *key == TARGET_ENV || *key == STAGED_ENV)
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.unwrap_or_default().to_string_lossy().into_owned(),
                )
            })
            .collect();
        assert!(
            vars.iter().any(|(key, value)| key == TARGET_ENV && value == "SVN管理器.exe"),
            "目标文件名要原样带过去：{vars:?}"
        );
        assert!(
            vars.iter().any(|(key, value)| key == STAGED_ENV && value == STAGED_EXE),
            "暂存名是固定的 ASCII：{vars:?}"
        );
    }

    #[test]
    fn sha256_of_matches_known_digest() {
        let path = std::env::temp_dir().join("svn_manager_sha_test.txt");
        std::fs::write(&path, b"hello").expect("写测试文件");
        // sha256("hello") 的公认值
        assert_eq!(
            sha256_of(&path).unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        let _ = std::fs::remove_file(&path);
        assert!(sha256_of(Path::new(r"C:\surely\not\exist.bin")).is_err());
    }
}

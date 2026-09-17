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
//! SHA256（服务端提供时）→ 落到程序目录暂存 → [`swap_in_place`] 把**正在运行的**
//! 旧程序改名让路、新文件顶上正式名字 → [`relaunch`] 拉起新版 → 本进程退出；
//! 新版启动时 [`clean_leftovers`] 收掉 `.old.exe`。
//!
//! 全程没有收尾脚本，也就没有「交给控制台按代码页解码的文本文件」这一环：早先用
//! 收尾 bat 时，脚本里一旦出现中文目录/中文文件名，cmd 读脚本就会把行拆错
//! （`'ARGET' 不是内部或外部命令`、`Unknown subcommand: 'Manager'` 那一类），
//! 「更新器不支持中文目录」就是这么来的。换成改名后路径完全不参与解码。
//! Windows 不允许覆盖或删除正在运行的镜像，但**允许改名**，这就是能不用脚本的全部依据。

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

/// 新版 exe 暂存用的文件名。程序目录与 %TEMP% 下各放一份都用它。
pub const STAGED_EXE: &str = "svn_manager_update.exe";

/// 旧程序让路时改成的名字：`svn_manager.exe` → `svn_manager.old.exe`。
/// 名字由正式名推得出来，所以新进程不需要任何跨进程传参就能清掉残局。
pub fn old_path(exe: &Path) -> PathBuf {
    exe.with_extension("old.exe")
}

/// 就地把**正在运行的** `exe` 换成暂存的 `staged`，成功时返回让路后的旧程序路径。
///
/// Windows 不允许覆盖或删除正在运行的镜像（会报拒绝访问），但允许改名——这就是不用
/// 收尾脚本也能自更新的全部依据：旧程序改名让路 → 空出来的名字放新文件 → 调用方
/// 拉起新程序、自己退出。好处是路径完全不经过控制台代码页，脚本文件、`%~dp0`、
/// 环境变量传名那一套都不需要了，中文目录 / 中文文件名一视同仁。
///
/// 第二步失败会把旧程序改回原名再返回错误（不留「正式名字下没有程序」的中间态）；
/// 调用方在这之后要么重试，要么让用户手动替换。
pub fn swap_in_place(exe: &Path, staged: &Path) -> Result<PathBuf, String> {
    let old = old_path(exe);
    // 上一次更新留下的旧程序这时已经退出，能删就删；删不掉也不挡这次换名
    if old.exists() {
        let _ = std::fs::remove_file(&old);
    }
    std::fs::rename(exe, &old)
        .map_err(|e| format!("没法把正在运行的程序改名让路（{}）：{e}", exe.display()))?;
    match std::fs::rename(staged, exe) {
        Ok(()) => Ok(old),
        Err(e) => {
            let _ = std::fs::rename(&old, exe);
            Err(format!("没法把新版本改名到位（{}）：{e}", exe.display()))
        }
    }
}

/// 拉起换好名的新版程序。父进程随后 `exit` 不影响它继续跑：Windows 不会连坐子进程。
pub fn relaunch(exe: &Path) -> Result<(), String> {
    let mut command = std::process::Command::new(exe);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("拉起新版本失败（{}）：{e}", exe.display()))
}

/// 启动时收拾上一次自更新的残局，返回「这次启动是更新后的第一次启动」。
///
/// `.old.exe` 只能在**新版自己**起来之后删：换名那会儿旧进程还在跑，它的镜像还映射着，
/// 系统不给删（`remove_file` 直接失败）。新版起来时旧进程通常刚好在退出，所以这里短促
/// 重试几次；实在删不掉就留着，下一次启动还会再收一遍。
///
/// 顺手清掉两个暂存文件：下载完没走到换名就退出（用户取消、或上一次更新失败）时留下的。
pub fn clean_leftovers(exe: &Path) -> bool {
    let old = old_path(exe);
    let mut updated = false;
    if old.exists() {
        updated = true;
        for _ in 0..10 {
            if std::fs::remove_file(&old).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
    }
    if let Some(dir) = exe.parent() {
        let _ = std::fs::remove_file(dir.join(STAGED_EXE));
    }
    let _ = std::fs::remove_file(std::env::temp_dir().join(STAGED_EXE));
    updated
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

    /// 换名的正路：新文件顶上正式名字，旧程序让到 `.old.exe`，暂存文件不再存在；
    /// 残局由启动时的 `clean_leftovers` 收掉。目录与 exe 名都用中文——这条路不经过
    /// 控制台代码页，中文不该有任何影响（脚本方案就是死在这儿的）。
    #[test]
    fn swap_in_place_puts_the_new_exe_in_place() {
        let dir = std::env::temp_dir().join("svn管理器_换名测试");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("SVN管理器.exe");
        let staged = dir.join(STAGED_EXE);
        std::fs::write(&exe, b"old-binary").unwrap();
        std::fs::write(&staged, b"new-binary").unwrap();

        let old = swap_in_place(&exe, &staged).expect("换名该成功");
        assert_eq!(std::fs::read(&exe).unwrap(), b"new-binary".to_vec(), "正式名字下要是新版");
        assert_eq!(std::fs::read(&old).unwrap(), b"old-binary".to_vec(), "旧程序要改名让路");
        assert!(!staged.exists(), "暂存文件该被换走");

        assert!(clean_leftovers(&exe), "有 .old.exe 就算「更新后的第一次启动」");
        assert!(!old.exists(), ".old.exe 没被清掉");
        assert!(!clean_leftovers(&exe), "清完再启动就不该再报「刚更新过」");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 第二步失败要回滚：旧程序改回原名，程序还是能跑的那个版本，
    /// 不会停在「正式名字下没有程序」的中间态。
    #[test]
    fn swap_in_place_rolls_back_when_the_staged_file_is_gone() {
        let dir = std::env::temp_dir().join("svn_manager_swap_rollback");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("svn_manager.exe");
        std::fs::write(&exe, b"old-binary").unwrap();
        let staged = dir.join(STAGED_EXE); // 故意不存在

        assert!(swap_in_place(&exe, &staged).is_err(), "暂存文件不在就该报错");
        assert_eq!(std::fs::read(&exe).unwrap(), b"old-binary".to_vec(), "旧程序要回到原名下");
        assert!(!old_path(&exe).exists(), "回滚后不该留下 .old.exe");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真机回归：**正在运行**的 exe 只有改名是允许的（覆盖、删除都会被系统拒绝），
    /// 这是「不用收尾脚本也能自更新」的全部依据，所以拿一个真跑着的进程过一遍。
    /// 替身用 System32 里 ping.exe 的副本（不是系统里那个），跑几秒不占界面；
    /// 换名之后它照旧跑着，正好顺势验证「运行时的镜像删不掉」，也就是清理必须交给新版启动。
    #[test]
    #[cfg(windows)]
    fn swap_in_place_renames_a_running_exe() {
        let dir = std::env::temp_dir().join("svn_manager_running_swap");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("svn_manager.exe");
        std::fs::copy(r"C:\Windows\System32\ping.exe", &exe).expect("复制替身 exe");
        let mut child = std::process::Command::new(&exe)
            .args(["-n", "20", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("替身进程该能起来");
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(child.try_wait().unwrap().is_none(), "替身进程这时该还在跑");

        let staged = dir.join(STAGED_EXE);
        std::fs::write(&staged, b"new-binary-bytes").unwrap();
        let old = swap_in_place(&exe, &staged).expect("运行中的 exe 该能改名让路");
        assert_eq!(
            std::fs::read(&exe).unwrap(),
            b"new-binary-bytes".to_vec(),
            "新版本没顶上正式名字"
        );
        assert!(old.exists(), "旧程序不在 .old.exe 下");
        assert!(
            std::fs::remove_file(&old).is_err(),
            "还在跑的镜像居然删得掉？那清理就不必等新版启动了"
        );

        let _ = child.kill();
        let _ = child.wait();
        assert!(clean_leftovers(&exe), "有 .old.exe 就该算「更新后的第一次启动」");
        assert!(!old.exists(), "旧进程退出后该删得掉");
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

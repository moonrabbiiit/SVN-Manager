use std::path::{Path, PathBuf};
use std::process::Command;

use crate::svn::{add_path, decode_bytes, drive_roots, env_path, SEARCH_BASES};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
use crate::svn::CREATE_NO_WINDOW;

/// Beyond Compare 主程序文件名。
const EXE: &str = "BCompare.exe";

/// 安装目录里可能出现的文件夹名（优先 Beyond Compare 5）。
const DIR_NAMES: &[&str] = &["Beyond Compare 5", "Beyond Compare 4", "Beyond Compare"];

/// BC 在 `%APPDATA%` 下存放设置文件的上级目录名，会话记录是
/// `%APPDATA%\\Scooter Software\\Beyond Compare 5\\BCSessions.xml`，少拼这一层就一条记录都读不到。
const SETTINGS_ROOT: &str = "Scooter Software";

/// 依次按 环境变量 → PATH → 注册表 App Paths → 各磁盘常见安装目录 查找 BCompare.exe。
pub fn candidates() -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Some(custom) = env_path("REPO_MANAGER_BCOMPARE") {
        add_path(&mut found, custom.clone());
        add_path(&mut found, custom.join(EXE));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            add_path(&mut found, dir.join(EXE));
        }
    }
    for hive in [
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths",
        r"HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths",
    ] {
        let mut command = Command::new("reg");
        command.args(["query", &format!("{hive}\\{EXE}"), "/ve"]);
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        let Ok(output) = command.output() else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        for line in decode_bytes(&output.stdout).lines() {
            let Some(pos) = line.find("REG_SZ") else {
                continue;
            };
            let value = line[pos + "REG_SZ".len()..].trim().trim_matches('"');
            if value.contains(':') {
                add_path(&mut found, PathBuf::from(value));
            }
        }
    }
    for root in drive_roots() {
        for base in SEARCH_BASES {
            for name in DIR_NAMES {
                add_path(&mut found, root.join(base).join(name).join(EXE));
            }
        }
    }
    found.retain(|path| path.is_file());
    found
}

/// 自动寻找 BCompare.exe（取第一个存在的候选）。
pub fn detect() -> Option<PathBuf> {
    candidates().into_iter().next()
}

/// 注册表里存放 Beyond Compare 试用标识（CacheID）的位置：4、5 各一代，
/// 另外新版还会写一个不带版本号的键，一起处理才不会漏。
const CACHE_KEYS: &[&str] = &[
    r"HKCU\Software\Scooter Software\Beyond Compare 5",
    r"HKCU\Software\Scooter Software\Beyond Compare 4",
    r"HKCU\Software\Scooter Software\Beyond Compare",
];

/// 重置 Beyond Compare 的试用状态：删掉注册表里的 CacheID，等价于
/// `reg delete "HKEY_CURRENT_USER\Software\Scooter Software\Beyond Compare 4" /v CacheID /f`。
/// 三个键都处理一遍，逐步返回说明文字。
pub fn reset_cache() -> Vec<String> {
    let mut notes = Vec::new();
    for key in CACHE_KEYS {
        let mut probe = Command::new("reg");
        probe.args(["query", key, "/v", "CacheID"]);
        #[cfg(windows)]
        probe.creation_flags(CREATE_NO_WINDOW);
        let Ok(output) = probe.output() else {
            notes.push(format!("无法执行 reg.exe：{key}"));
            continue;
        };
        if !output.status.success() {
            notes.push(format!("{key}：没有 CacheID，跳过"));
            continue;
        }
        let mut command = Command::new("reg");
        command.args(["delete", key, "/v", "CacheID", "/f"]);
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        notes.push(match command.output() {
            Ok(deleted) if deleted.status.success() => format!("{key}：CacheID 已删除"),
            Ok(deleted) => {
                let text = decode_bytes(&deleted.stderr);
                let reason = text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("reg.exe 执行失败");
                format!("{key}：删除失败——{reason}")
            }
            Err(e) => format!("无法执行 reg.exe：{e}"),
        });
    }
    // BC 退出时会把 CacheID 写回去，正在运行的话先提醒一句
    let mut check = Command::new("tasklist");
    check.args(["/nh", "/fo", "csv", "/fi", "imagename eq BCompare.exe"]);
    #[cfg(windows)]
    check.creation_flags(CREATE_NO_WINDOW);
    if let Ok(output) = check.output() {
        if decode_bytes(&output.stdout).contains("BCompare.exe") {
            notes.push("检测到 Beyond Compare 正在运行，它退出时会把 CacheID 写回，请关闭后再重置一次".to_owned());
        }
    }
    notes
}

/// 启动 Beyond Compare。targets 为空时只打开主窗口，传两个路径即为两路对比。
/// 启动 Beyond Compare。targets 为空时只打开主窗口，传两个路径即为两路对比；
/// switches 是 `/filters=...` 这类命令行开关——BC 拿到路径只会按程序默认设置新建一次比较，
/// 不会去读记录，记录里的设置只能靠开关带过去。
pub fn launch(exe: &Path, targets: &[&Path], switches: &[String]) -> Result<(), String> {
    let mut command = Command::new(exe);
    // BC 官方命令行格式为 [/switches] left [right]，把开关放在路径前面，
    // 避免老版本 BC 把 /filters=... 误当成第三个路径或无名会话。
    command.args(switches).args(targets);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("无法启动 {}：{e}", exe.display()))
}

/// Beyond Compare 里保存的一条对比记录（BCSessions.xml 中的一个 Session 节点）。
pub struct BcSession {
    /// 会话名，也就是 BC 标签页上显示的那个名字
    pub name: String,
    pub left: String,
    pub right: String,
    /// 记录里的名称筛选（Filters/NameFilter），没设置过就是空
    pub filter: String,
    /// 文件夹对比为 true，单文件对比为 false
    pub folder: bool,
    /// 记录里的最后修改时间，形如 `2026-09-02 17:30:55`
    pub modified: String,
}

/// 会话记录文件：`%APPDATA%\Scooter Software\Beyond Compare 5\BCSessions.xml`。
pub fn session_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Some(appdata) = std::env::var_os("APPDATA") else {
        return files;
    };
    for name in DIR_NAMES {
        let file = Path::new(&appdata)
            .join(SETTINGS_ROOT)
            .join(name)
            .join("BCSessions.xml");
        if file.is_file() {
            files.push(file);
        }
    }
    files
}

/// BC 的「新建会话默认值」节点：会话存储里那个不带 Value 属性的 TDirCompareSession。
/// 往它下面的 <Rules> 里写开关，之后所有新建的文件夹比较（含命令行打开的）都按它来，
/// BC 退出时也会原样保留这个节点。键名取自 BCompare.exe 里 TDirRules 的成员名。
const DEFAULTS_NODE: &str = "<TDirCompareSession>";

/// 「新建会话默认值」里能配的比较条件，对应 BC 会话设置「比较」页上的勾选。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DefaultRules {
    /// 比较内容
    pub content: bool,
    /// 比较文件名大小写
    pub filename_case: bool,
}

/// 重写「新建会话默认值」节点里的 <Rules>，只做字符串增删，不重新序列化整个文件。
fn apply_default_rules(text: &str, rules: &DefaultRules) -> Option<String> {
    let open = text.find(DEFAULTS_NODE)? + DEFAULTS_NODE.len();
    let state = text[open..].find("<State>")? + open;
    let body = &text[..state];
    let whitespace = &body[body.trim_end().len()..];
    let newline = if whitespace.contains("\r\n") { "\r\n" } else { "\n" };
    let indent = whitespace.rsplit('\n').next().unwrap_or_default().to_owned();
    let mut children = String::new();
    if rules.content {
        children += &format!("{newline}{indent}\t<UseContentComparison Value=\"True\"/>");
    }
    if rules.filename_case {
        children += &format!("{newline}{indent}\t<UseCaseSensitiveComparison Value=\"True\"/>");
    }
    let middle = if children.is_empty() {
        format!("{newline}{indent}")
    } else {
        format!("{newline}{indent}<Rules>{children}{newline}{indent}</Rules>{newline}{indent}")
    };
    Some(format!("{}{}{}", &text[..open], middle, &text[state..]))
}

/// 读取「新建会话默认值」里的比较条件；None = 找不到那个节点（没装 BC 或存储结构不认识）。
pub fn default_rules() -> Option<DefaultRules> {
    let Some(file) = session_files().into_iter().next() else {
        return None;
    };
    let text = std::fs::read_to_string(file).ok()?;
    let open = text.find(DEFAULTS_NODE)? + DEFAULTS_NODE.len();
    let state = text[open..].find("<State>")? + open;
    let body = &text[open..state];
    Some(DefaultRules {
        content: body.contains(r#"<UseContentComparison Value="True""#),
        filename_case: body.contains(r#"<UseCaseSensitiveComparison Value="True""#),
    })
}

/// 把比较条件写进 BC 的会话默认值，等价于在 BC 会话设置底部下拉里选「更新会话默认值」。
/// BC 退出时会整个覆盖会话存储，所以动手前必须确认它没在跑，并且先备份一份。
pub fn set_default_rules(rules: &DefaultRules) -> Result<String, String> {
    let mut check = Command::new("tasklist");
    check.args(["/nh", "/fo", "csv", "/fi", "imagename eq BCompare.exe"]);
    #[cfg(windows)]
    check.creation_flags(CREATE_NO_WINDOW);
    if let Ok(output) = check.output() {
        if decode_bytes(&output.stdout).contains(EXE) {
            return Err(format!("{EXE} 正在运行，它退出时会把会话存储整个覆盖掉，请先全部关闭再改"));
        }
    }
    let Some(file) = session_files().into_iter().next() else {
        return Err("找不到 BCSessions.xml，请先用 Beyond Compare 打开过一次文件夹对比".to_owned());
    };
    let text = std::fs::read_to_string(&file).map_err(|e| format!("读取会话存储失败：{e}"))?;
    let Some(next) = apply_default_rules(&text, rules) else {
        return Err("会话存储里找不到「新建文件夹比较」的默认值节点，先在 Beyond Compare 里做一次文件夹对比再试".to_owned());
    };
    let backup = file.with_file_name("BCSessions.xml.repomanager.bak");
    std::fs::copy(&file, &backup).map_err(|e| format!("备份失败：{e}"))?;
    std::fs::write(&file, next.as_bytes()).map_err(|e| format!("写入失败：{e}"))?;
    let mut on = Vec::new();
    if rules.content {
        on.push("比较内容");
    }
    if rules.filename_case {
        on.push("比较文件名大小写");
    }
    Ok(format!(
        "已把 BC 会话默认值的比较条件设为：{}（备份：{}），重启 Beyond Compare 后对新建的文件夹比较生效",
        if on.is_empty() { "全部取消".to_owned() } else { on.join("、") },
        backup.display()
    ))
}
/// 路径的最后一级名字，反斜杠、正斜杠、结尾分隔符都算同一层。
fn tail(path: &str) -> &str {
    path.trim_end_matches(['\\', '/'])
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(path)
}

/// 按路径名认服务器端：最后一级名字里含 server（不区分大小写）就算命中。
/// 用户的习惯是对比两侧目录命名为 xxx_server / xxx_local；
/// 两侧都含或都不含时调用方应沿用记录原有的左右顺序，不要硬猜。
pub fn looks_like_server(path: &str) -> bool {
    tail(path).to_lowercase().contains("server")
}

/// 按服务器端所在侧把两个路径排成 (左, 右)。
/// `side` 是设置里的 bc_server_side：right 时两边对调，其余（含 left）服务器端在左。
pub fn order_sides(server: &Path, local: &Path, side: &str) -> (PathBuf, PathBuf) {
    if side.trim().eq_ignore_ascii_case("right") {
        (local.to_path_buf(), server.to_path_buf())
    } else {
        (server.to_path_buf(), local.to_path_buf())
    }
}

/// 解析 BCSessions.xml：任意层级下的 TDirCompareSession / TTextCompareSession 都收，
/// BC 自己留下的无名空节点、没有左右路径的节点跳过。
pub fn parse_sessions(xml: &str) -> Vec<BcSession> {
    let Ok(doc) = roxmltree::Document::parse(xml.trim_start_matches('\u{feff}')) else {
        return Vec::new();
    };
    let mut list = Vec::new();
    for node in doc
        .descendants()
        .filter(|n| matches!(n.tag_name().name(), "TDirCompareSession" | "TTextCompareSession"))
    {
        let Some(name) = node.attribute("Value").map(str::trim) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let Some(specs) = node.children().find(|child| child.has_tag_name("Specs")) else {
            continue;
        };
        let side = |tag: &str| {
            specs
                .children()
                .find(|child| child.has_tag_name(tag))
                .and_then(|child| child.attribute("Value"))
        };
        let (Some(left), Some(right)) = (side("Left"), side("Right")) else {
            continue;
        };
        list.push(BcSession {
            name: name.to_owned(),
            left: left.to_owned(),
            right: right.to_owned(),
            folder: node.has_tag_name("TDirCompareSession"),
            filter: node
                .children()
                .find(|child| child.has_tag_name("Filters"))
                .and_then(|group| group.children().find(|child| child.has_tag_name("NameFilter")))
                .and_then(|child| child.attribute("Value"))
                .unwrap_or_default()
                .to_owned(),
            modified: node
                .children()
                .find(|child| child.has_tag_name("LastModified"))
                .and_then(|child| child.attribute("Value"))
                .unwrap_or_default()
                .to_owned(),
        });
    }
    list
}

/// 读取 Beyond Compare 已保存的全部对比记录。
pub fn sessions() -> Vec<BcSession> {
    let mut list = Vec::new();
    for file in session_files() {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        list.extend(parse_sessions(&text));
    }
    list
}

/// 按目录找对应的对比记录：先用目录名和记录左右两侧的最后一级名字比（不区分大小写），
/// 名字比不中时再退一步，看记录某一侧和选中目录是不是同一棵树（互为前缀）；
/// 同时命中多条时优先文件夹对比，其次优先带名称筛选的那条——BC 会把每次比较都自动存成一条
/// 记录，同一对文件夹往往存了好几份，最新那份常常是命令行开出来的、什么条件都没配；
/// 两者都相同才按记录时间取最新。
pub fn match_session<'a>(list: &'a [BcSession], dir: &Path) -> Option<&'a BcSession> {
    let name = match dir.file_name().map(|n| n.to_string_lossy().to_lowercase()) {
        Some(name) if !name.is_empty() => name,
        _ => return None,
    };
    let whole = dir.to_string_lossy().to_lowercase();
    let whole = whole.trim_end_matches(['\\', '/']);
    let mut best: Option<(u8, &BcSession)> = None;
    for session in list {
        let sides = [session.left.to_lowercase(), session.right.to_lowercase()];
        let named = sides.iter().any(|side| tail(side) == name);
        let same_tree = sides.iter().any(|side| {
            let side = format!("{}\\", side.trim_end_matches(['\\', '/']));
            let root = format!("{whole}\\");
            side.starts_with(&root) || root.starts_with(&side)
        });
        let score = if named {
            if session.folder { 4 } else { 3 }
        } else if same_tree {
            if session.folder { 2 } else { 1 }
        } else {
            continue;
        };
        let better = match best {
            None => true,
            Some((old_score, old)) => {
                let keyed = (!session.filter.is_empty(), session.modified.as_str());
                let old_keyed = (!old.filter.is_empty(), old.modified.as_str());
                score > old_score || (score == old_score && keyed > old_keyed)
            }
        };
        if better {
            best = Some((score, session));
        }
    }
    best.map(|(_, session)| session)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按路径名认服务器端只看最后一级名字，中间路径里有 server 不算。
    #[test]
    fn server_side_is_recognized_by_folder_name() {
        assert!(looks_like_server(r"D:\code\HRP_server"));
        assert!(looks_like_server(r"D:\code\SERVER_A"));
        assert!(looks_like_server(r"D:\code\vue_ss_server\"));
        assert!(!looks_like_server(r"D:\code\serverless\app"));
        assert!(!looks_like_server(r"D:\code\HRP_local"));
    }

    /// 服务器端放哪一侧由设置决定；除 right 外（含老配置的空值）一律按左侧处理。
    #[test]
    fn order_sides_follows_the_setting() {
        let server = Path::new(r"D:\code\HRP_server");
        let local = Path::new(r"D:\code\HRP_local");
        let (left, right) = order_sides(server, local, "left");
        assert_eq!(left, server);
        assert_eq!(right, local);
        let (left, right) = order_sides(server, local, "right");
        assert_eq!(left, local);
        assert_eq!(right, server);
        // 老配置 / 手工改坏过的值都当「左侧」
        let (left, _) = order_sides(server, local, "");
        assert_eq!(left, server);
        let (left, _) = order_sides(server, local, "Left ");
        assert_eq!(left, server);
    }

    /// 取自真实 BCSessions.xml 的片段：两层 TSessionFolder 嵌套、一条无名空节点、一条单文件记录。
    const XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<BCSessions Version="2" MinVersion="2">
    <TSessionFolder>
        <Items>
            <TSessionFolder>
                <Items>
                    <TDirCompareSession Value="HRP_server &lt;--> HRP_local">
                        <Filters>
                            <NameFilter Value="-*.iml;-*.java"/>
                        </Filters>
                        <LastModified Value="2026-09-02 17:30:55"/>
                        <Specs>
                            <Left Value="D:\Program\Work\hhyp\Code\HRP_server"/>
                            <Right Value="D:\Program\Work\hhyp\Code\HRP_local"/>
                        </Specs>
                    </TDirCompareSession>
                    <TDirCompareSession Value="vue_ss_server &lt;--> vue_ss_local">
                        <LastModified Value="2026-09-02 17:40:31"/>
                        <Specs>
                            <Left Value="D:\Program\Work\hhyp\Code\vue_ss_server"/>
                            <Right Value="D:\Program\Work\hhyp\Code\vue_ss_local"/>
                        </Specs>
                    </TDirCompareSession>
                    <TTextCompareSession Value="ctmcontract.xml">
                        <LastModified Value="2026-07-27 14:08:39"/>
                        <Specs>
                            <Left Value="D:\Program\Work\hhyp\Code\HRP_server\HRP.CTM\config\mapper\business\ctmcontract.xml"/>
                            <Right Value="D:\Program\Work\hhyp\Code\HRP_local\HRP.CTM\config\mapper\business\ctmcontract.xml"/>
                        </Specs>
                    </TTextCompareSession>
                </Items>
            </TSessionFolder>
            <TDirCompareSession>
                <State>
                    <SortCol Value="colModified"/>
                </State>
            </TDirCompareSession>
        </Items>
    </TSessionFolder>
</BCSessions>
"#;

    fn names(list: &[BcSession]) -> Vec<&str> {
        list.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn reads_nested_sessions_and_skips_nameless_one() {
        let list = parse_sessions(XML);
        assert_eq!(names(&list), vec!["HRP_server <--> HRP_local", "vue_ss_server <--> vue_ss_local", "ctmcontract.xml"]);
        assert!(list[0].folder && !list[2].folder);
        assert_eq!(list[0].filter, "-*.iml;-*.java");
        assert!(list[1].filter.is_empty(), "没写 Filters 的记录筛选应为空");
    }

    /// BC 会把每次比较都自动存成一条记录，同一对文件夹常常存出好几份：本程序用命令行
    /// 开出来的那份什么条件都没配，时间却是最新的（BC4 用户反馈的正是被它抢了位置）。
    const DUP: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<BCSessions Version="1" MinVersion="1">
    <TSessionFolder>
        <Items>
            <TSessionFolder>
                <Items>
                    <TDirCompareSession Value="svn &lt;--> ypy_project">
                        <LastModified Value="2026-09-04 11:29:17"/>
                        <Specs>
                            <Left Value="E:\项目\svn"/>
                            <Right Value="E:\项目\ypy_project"/>
                        </Specs>
                    </TDirCompareSession>
                    <TDirCompareSession Value="svn &lt;--> ypy_project">
                        <Filters>
                            <NameFilter Value="-*.iml"/>
                        </Filters>
                        <LastModified Value="2025-05-06 14:26:20"/>
                        <Specs>
                            <Left Value="E:\项目\svn"/>
                            <Right Value="E:\项目\ypy_project"/>
                        </Specs>
                    </TDirCompareSession>
                    <TDirCompareSession Value="svn &lt;--> ypy_project">
                        <Filters>
                            <NameFilter Value="-*.classpath"/>
                        </Filters>
                        <LastModified Value="2025-06-01 09:00:00"/>
                        <Specs>
                            <Left Value="E:\项目\svn"/>
                            <Right Value="E:\项目\ypy_project"/>
                        </Specs>
                    </TDirCompareSession>
                </Items>
            </TSessionFolder>
        </Items>
    </TSessionFolder>
</BCSessions>
"#;

    /// 命中同样多条时先挑带名称筛选的那条（而不是文档顺序或时间最新的那条），
    /// 都带筛选才按时间取最新。
    #[test]
    fn prefers_the_record_carrying_conditions() {
        let list = parse_sessions(DUP);
        let hit = match_session(&list, Path::new(r"E:\项目\svn")).expect("应命中 svn 那条记录");
        assert_eq!(hit.filter, "-*.classpath", "被自动保存的空记录抢掉了真正的会话");
    }

    #[test]
    fn matches_either_side_by_folder_name() {
        let list = parse_sessions(XML);
        for side in [
            r"D:\Program\Work\hhyp\Code\HRP_server",
            r"d:\program\work\hhyp\code\hrp_local\",
        ] {
            let hit = match_session(&list, Path::new(side)).unwrap_or_else(|| panic!("应命中 HRP 记录：{side}"));
            assert_eq!(hit.name, "HRP_server <--> HRP_local");
        }
    }

    #[test]
    fn prefers_folder_record_for_subdirectory() {
        let list = parse_sessions(XML);
        let hit = match_session(&list, Path::new(r"D:\Program\Work\hhyp\Code\HRP_server\HRP.CTM"))
            .expect("子目录应落到包含它的那条记录");
        assert_eq!(hit.name, "HRP_server <--> HRP_local");
    }

    /// 本机装过 Beyond Compare 时，拿真实的 BCSessions.xml 再验一遍：
    /// 每条文件夹记录都应该能按自己的左右路径找回来（没装就直接跳过）。
    #[test]
    fn real_records_match_their_own_paths() {
        let _appdata = crate::testbed::lock_appdata();
        let Some(appdata) = std::env::var_os("APPDATA") else {
            return;
        };
        if !Path::new(&appdata).join(SETTINGS_ROOT).is_dir() {
            return; // 这台机器没装 BC，真实记录无从校验
        }
        let list = sessions();
        assert!(!list.is_empty(), "装了 BC 却读不到任何对比记录，八成是记录文件路径拼错了");
        for (i, session) in list.iter().enumerate() {
            assert!(!session.left.is_empty() && !session.right.is_empty(), "{}", session.name);
            // 文件对比记录的两侧往往落在某条文件夹记录的树里，按分数会先命中那条，
            // 这是有意为之，不校验；重名或同一对路径存了两条的记录也分不出该返回哪条
            if !session.folder {
                continue;
            }
            if list.iter().enumerate().any(|(j, other)| {
                j != i
                    && (other.name == session.name
                        || (other.left.eq_ignore_ascii_case(&session.left)
                            && other.right.eq_ignore_ascii_case(&session.right))
                        || (other.left.eq_ignore_ascii_case(&session.right)
                            && other.right.eq_ignore_ascii_case(&session.left)))
            }) {
                continue;
            }
            for side in [&session.left, &session.right] {
                let hit = match_session(&list, Path::new(side)).unwrap_or_else(|| panic!("匹配不到自己的路径：{side}"));
                assert_eq!(hit.name, session.name, "{side} 匹配到了别的记录");
            }
        }
    }

    /// 真实记录文件里写了几个 NameFilter，解析出来就得有几条非空筛选。
    /// 以前少拼了 Scooter Software 一层，记录一条都读不到，症状就是 BC 打开了却没有任何条件。
    #[test]
    fn real_name_filters_are_all_parsed() {
        let _appdata = crate::testbed::lock_appdata();
        let Some(appdata) = std::env::var_os("APPDATA") else {
            return;
        };
        if !Path::new(&appdata).join(SETTINGS_ROOT).is_dir() {
            return; // 这台机器没装 BC
        }
        assert!(!session_files().is_empty(), "装了 BC 却找不到会话记录文件，路径拼错了");
        let mut raw = 0usize;
        let mut parsed = 0usize;
        for file in session_files() {
            let text = std::fs::read_to_string(file).unwrap();
            raw += text.matches("<NameFilter").count();
            parsed += parse_sessions(&text).iter().filter(|s| !s.filter.is_empty()).count();
        }
        assert_eq!(parsed, raw, "BCSessions.xml 里有 NameFilter 没被解析出来");
    }

    /// 「新建会话默认值」里 <Rules> 的增删：只动那一个节点，其余字节不变，而且能原样撤回。
    #[test]
    fn default_rules_round_trip() {
        let text = "<TDirCompareSession>\r\n\t\t\t\t<State>\r\n\t\t\t\t\t<DisplayFilter Value=\"x\"/>\r\n\t\t\t\t</State>\r\n\t\t\t</TDirCompareSession>";
        let both = DefaultRules {
            content: true,
            filename_case: true,
        };
        let on = apply_default_rules(text, &both).expect("应该能写入默认值节点");
        assert!(on.contains(r#"<UseContentComparison Value="True"/>"#));
        assert!(on.contains(r#"<UseCaseSensitiveComparison Value="True"/>"#));
        assert!(on.contains("\r\n\t\t\t\t<Rules>"), "缩进要跟 BC 自己的写法一致");
        // 只开一个时另一个键名不能残留，两个都关就该整个 <Rules> 都不写
        let only_case = apply_default_rules(&on, &DefaultRules { content: false, filename_case: true }).unwrap();
        assert!(!only_case.contains("UseContentComparison"));
        assert_eq!(apply_default_rules(&on, &DefaultRules::default()).as_deref(), Some(text));
        assert_eq!(apply_default_rules("<root/>", &both), None, "没有默认值节点时不能乱写");
    }
    #[test]
    fn unrelated_or_root_path_matches_nothing() {
        let list = parse_sessions(XML);
        assert!(match_session(&list, Path::new(r"D:\Program\Work\other")).is_none());
        assert!(match_session(&list, Path::new(r"D:\")).is_none());
    }
}

//! 本地端 ↔ 服务器端：把「更多 → 绑定对比对象」里绑的那个目录（用户真正写代码的地方，
//! 不受版本控制，界面上统称「本地开发侧」）与该目录的 SVN 工作副本逐文件比一遍，
//! 主页报出「本地差异 N 项」，一项都没有就是「本地无差异」。
//!
//! 判定不跟修改时间、也不跟字节数较真：BC 从服务器端合到本地端时是否保留时间戳取决于
//! 本机 BC 设置，而 BOM（3 字节）、CRLF 与 LF 之差都能只改字节数不改内容。所以只有
//! 「大小和时间戳都一样」才直接算一致，其余一律读内容，并按 BC 默认的忽略规则把
//! 只差 BOM / 行尾 / 行尾空白 / 末尾换行的那批算成一致（实测旧系统后端 548 项里
//! 541 项属于这一类：508 个 BOM、19 个行尾、14 个行尾空白或末尾换行，主页的数
//! 因此从 548 收到 6，和 BC 窗口里看到的那几项对上）。

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant, UNIX_EPOCH};

use regex::Regex;

/// 差异明细里最多带出多少条相对路径（给 tooltip 用，全量清单交给 Beyond Compare）
pub const SAMPLE_MAX: usize = 50;

/// 内容核对的预算：只按时间收口，60 秒跑不完的那部分才记成未核对。
/// 不按文件数或字节数卡——本地端与工作副本的时间戳几乎处处不同（BC 合过的文件时间戳
/// 按复制时刻重写），于是整棵树基本都要读内容，卡文件数会把没轮到的那批静默记成未核对，
/// 主页的数就成了随手可漏的下界（实测易朋云 HRP.SR 一个真改了内容的 mapper xml 就这么漏掉）。
/// 比对在后台线程跑、行上只有转圈标记，不占界面也不锁操作，跑得久只是这个数晚点到。
/// `max_files` 是防失控的兜底（正常工程树到不了这个量级），不是工作上限。
struct Limits {
    max_files: usize,
    max_time: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_files: 200_000,
            max_time: Duration::from_secs(60),
        }
    }
}

/// 走树深度上限：正常工程树到不了 40 层，这条是防环的兜底——目录联结（junction）在
/// Windows 的目录枚举里就报成普通目录，`file_type` 认不出来，只能靠深度收口。
const MAX_DEPTH: usize = 40;
/// 内容核对的分块大小：64KB 足够把顺序读跑满，又能让第一个差异字节尽早退出
const CHUNK: usize = 64 * 1024;

/// 一条差异判定结果，随最近一次本地端检测显示在主页上。
#[derive(Clone, Debug, Default)]
pub struct Summary {
    /// 两边都有、内容不一样（含「大小相同但读出来不同」）
    pub differing: usize,
    /// 只有本地端有：他要传上去却还没 `svn add` 的东西就在这堆里
    pub only_local: usize,
    /// 只有服务器端有：本地缺的，或他本地删掉的
    pub only_server: usize,
    /// 两边都有、只差 BOM / 行尾 / 行尾空白 / 末尾换行：Beyond Compare 默认把这类算一致，
    /// 所以不计进差异数，但要在 tooltip 里说清有多少，别让人以为完全没动过
    pub trivial: usize,
    /// 该读内容判定但预算用满没读（大小不同、或大小相同而时间不同）—— 差异数是下界，不是结论
    pub unverified: usize,
    /// 两边一致的文件数，用来说明「0 差异」是真的比过、不是没比
    pub same: usize,
    /// 抽样相对路径（排序后），最多 `SAMPLE_MAX` 条
    pub samples: Vec<String>,
}

impl Summary {
    /// 主页那枚标记上的数：只要「不一样」就算一项，未核对的不混进来。
    pub fn total(&self) -> usize {
        self.differing + self.only_local + self.only_server
    }

    /// 真的比过、且一项差异都没有：未核对的那些不能算「无差异」，它们只是没读出来而已。
    pub fn is_clean(&self) -> bool {
        self.total() == 0 && self.unverified == 0
    }

    /// 标记与日志行上的措辞，四处共用一份，免得改了这处漏了那处。
    pub fn label(&self) -> String {
        if self.is_clean() {
            "本地无差异".to_owned()
        } else {
            format!("本地差异 {} 项", self.total())
        }
    }
}

/// Beyond Compare 名称筛选的排除条件。
#[derive(Clone, Debug, Default)]
pub struct Excludes {
    dirs: Vec<String>,
    files: Vec<Regex>,
}

impl Excludes {
    /// 解析 `DirConfig::bc_filter`。真实形态（本机配置解码后）：
    /// `-*.iml;-.project;-log4j2.xml;-.\.idea\;-bin\;-web\`
    /// 分号分隔；只有 `-` 前缀是排除；结尾带 `\` 的是目录名，按末级名在任意层级命中；
    /// 其余按文件名匹配（前导点属于名字本身，`.project`、`.classpath` 都是文件不是相对路径）。
    /// `+` 前缀和无前缀的包含模式这里不支持：差异口径要和他用同一份筛选打开的 BC 对得上，
    /// 而 BC 那边包含条件是记在会话里的，我们只有这份排除串。
    pub fn parse(raw: &str) -> Self {
        let mut out = Self::default();
        // 工作副本的元数据目录永远不参与比较：它只存在于服务器端，算进去会把整个
        // 工作副本的文件数都当成「本地端缺失」
        out.dirs.push(".svn".to_owned());
        for item in raw.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let Some(pattern) = item.strip_prefix('-') else { continue };
            let pattern = pattern.trim();
            if pattern.is_empty() {
                continue;
            }
            // 结尾的 `\` 是 BC 里「目录名」的写法，取末级名即可在任意层级命中
            if let Some(dir) = pattern.strip_suffix('\\') {
                let name = dir.rsplit('\\').next().unwrap_or("").trim_matches('\\');
                if !name.is_empty() {
                    out.dirs.push(name.to_ascii_lowercase());
                }
                continue;
            }
            if let Some(re) = glob_to_regex(pattern) {
                out.files.push(re);
            }
        }
        out
    }

    fn skips_dir(&self, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        self.dirs.iter().any(|d| *d == lower) || self.matches_file(&lower)
    }

    fn skips_file(&self, name: &str) -> bool {
        self.matches_file(&name.to_ascii_lowercase())
    }

    fn matches_file(&self, lower: &str) -> bool {
        self.files.iter().any(|re| re.is_match(lower))
    }
}

/// 把 BC 的名称模式编译成整串匹配的正则（`*` → `.*`、`?` → `.`、其余逐字符转义，统一小写）。
/// 不含通配符的模式同样走这里——`.project`、`log4j2.xml` 就是要按字面名精确命中。
fn glob_to_regex(pattern: &str) -> Option<Regex> {
    let mut body = String::with_capacity(pattern.len() * 2);
    for ch in pattern.to_ascii_lowercase().chars() {
        match ch {
            '*' => body.push_str(".*"),
            '?' => body.push('.'),
            _ => body.push_str(&regex::escape(&ch.to_string())),
        }
    }
    Regex::new(&format!("^{body}$")).ok()
}

#[derive(Clone)]
struct Entry {
    rel: String,
    size: u64,
    modified: u64,
}

/// 按默认预算比较两个目录。任一侧不是可读目录时返回 Err（主页宁可什么都不显示，
/// 也不能把「没比成」报成「没有差异」）。
pub fn compare(local: &Path, server: &Path, filter: &str) -> Result<Summary, String> {
    compare_with(local, server, filter, &Limits::default())
}

fn compare_with(local: &Path, server: &Path, filter: &str, limits: &Limits) -> Result<Summary, String> {
    if !local.is_dir() {
        return Err(format!("本地开发侧目录读不到：{}", local.display()));
    }
    if !server.is_dir() {
        return Err(format!("服务器端目录读不到：{}", server.display()));
    }
    let ex = Excludes::parse(filter);
    let start = Instant::now();

    // 只给服务器端建表，走本地端时逐条摘走，剩下自然就是「仅服务器端」——
    // 一棵表比两棵省一半内存，也少一次全树 stat
    let mut index: HashMap<String, Entry> = HashMap::new();
    walk(server, server, &ex, 0, &mut |rel, size, modified| {
        index.insert(rel.to_ascii_lowercase(), Entry { rel: rel.to_owned(), size, modified });
    });

    let mut summary = Summary::default();
    let mut verified = 0usize;
    let local_owned = local.to_owned();
    walk(local, local, &ex, 0, &mut |rel, size, modified| {
        let key = rel.to_ascii_lowercase();
        let matched = index.remove(&key);
        let Some(entry) = matched else {
            summary.only_local += 1;
            push_sample(&mut summary, rel);
            return;
        };
        if entry.size == size && entry.modified == modified {
            // 大小、时间戳都一样就不读文件：这是两万多半秒走完的关键。BC 合过的文件
            // 时间戳会变，落不进这一档，交给下面的核对
            summary.same += 1;
            return;
        }
        // 大小不同也不等于改了：BOM 是 3 字节、一行 CRLF 变 LF 是 1 字节，
        // 所以到这一步只有读内容才算数
        if verified >= limits.max_files || start.elapsed() > limits.max_time {
            summary.unverified += 1;
            return;
        }
        verified += 1;
        match verify(&local_owned.join(rel), &server.join(&entry.rel)) {
            Content::Same => summary.same += 1,
            Content::Equivalent => summary.trivial += 1,
            Content::Different => {
                summary.differing += 1;
                push_sample(&mut summary, rel);
            }
        }
    });

    summary.only_server = index.len();
    for entry in index.values() {
        push_sample(&mut summary, &entry.rel);
    }
    summary.samples.sort();
    summary.samples.truncate(SAMPLE_MAX);
    Ok(summary)
}

fn push_sample(summary: &mut Summary, rel: &str) {
    if summary.samples.len() < SAMPLE_MAX {
        summary.samples.push(rel.to_owned());
    }
}

/// 递归走一棵树，把每个没被筛选排除的文件交给 sink（相对路径以 root 为基准、统一 `/` 分隔，
/// 大小写按磁盘上的原样）。目录名与文件名都过筛选——BC 的名称筛选本来就同时作用在两侧，
/// 一个名叫 `foo.iml` 的目录留着也会带出一堆无关差异。
fn walk(root: &Path, dir: &Path, ex: &Excludes, depth: usize, sink: &mut dyn FnMut(&str, u64, u64)) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(read) = fs::read_dir(dir) else { return };
    for item in read.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        let Ok(file_type) = item.file_type() else { continue };
        let child = item.path();
        let Ok(rest) = child.strip_prefix(root) else { continue };
        let rel = rest.display().to_string().replace('\\', "/");
        if file_type.is_dir() {
            if ex.skips_dir(&name) {
                continue;
            }
            walk(root, &child, ex, depth + 1, sink);
        } else if file_type.is_file() && !ex.skips_file(&name) {
            let Ok(meta) = item.metadata() else { continue };
            sink(&rel, meta.len(), mtime_secs(&meta));
        }
    }
}

/// 修改时间取整秒。复制工具写回的亚秒精度不一定和源文件一致，按秒比才把「同一时刻」
/// 认成同一时刻；真按秒不同也不据此判差异，那交给内容核对。
fn mtime_secs(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 一对文件的内容判定
enum Content {
    /// 一模一样
    Same,
    /// 只差 BOM / 行尾 / 行尾空白 / 末尾换行 —— Beyond Compare 默认算一致，我们跟着算一致
    Equivalent,
    /// 真不一样
    Different,
}

/// 超过这个大小只做逐字节比较，不整读进内存做归一化：文本文件到不了这个量级，
/// 而大文件里真改了的字节也不该被 BOM 那套规则洗掉
const NORMALIZE_MAX: u64 = 16 * 1024 * 1024;

fn verify(a: &Path, b: &Path) -> Content {
    if same_content(a, b) {
        return Content::Same;
    }
    let big = |p: &Path| fs::metadata(p).map(|m| m.len() > NORMALIZE_MAX).unwrap_or(true);
    if big(a) || big(b) {
        return Content::Different;
    }
    let (Ok(x), Ok(y)) = (fs::read(a), fs::read(b)) else {
        return Content::Different;
    };
    if !looks_text(&x) || !looks_text(&y) {
        return Content::Different;
    }
    if normalize(&x) == normalize(&y) {
        Content::Equivalent
    } else {
        Content::Different
    }
}

/// 前 8KB 里没有 NUL 就当文本看（和 BC 只对文本套用忽略规则同理），
/// 二进制文件里恰好出现 `0D 0A` 不该被当成换行折掉
fn looks_text(bytes: &[u8]) -> bool {
    !bytes[..bytes.len().min(8 * 1024)].contains(&0)
}

/// 去掉 UTF-8 BOM、把 CRLF / CR 折成 LF、去掉每行行尾的空格与制表符，末尾多一个少一个
/// 换行也不算差异——这几条就是 BC 默认的忽略规则。结尾空行仍算差异，BC 会报出来。
fn normalize(bytes: &[u8]) -> Vec<u8> {
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let mut lf = Vec::with_capacity(body.len());
    let mut i = 0;
    while i < body.len() {
        if body[i] == b'\r' {
            lf.push(b'\n');
            i += usize::from(body.get(i + 1) == Some(&b'\n'));
        } else {
            lf.push(body[i]);
        }
        i += 1;
    }
    let mut lines: Vec<&[u8]> = lf
        .split(|b| *b == b'\n')
        .map(|line| match line.iter().rposition(|b| !matches!(b, b' ' | b'\t' | 0x0B | 0x0C)) {
            Some(kept) => &line[..=kept],
            None => &line[..0],
        })
        .collect();
    if lines.last() == Some(&&[][..]) {
        lines.pop();
    }
    lines.join(&b'\n')
}

/// 分块读字节比内容，遇到第一个差异字节就退出。不引哈希依赖（项目里唯一的哈希是
/// `update.rs` 调外部 certutil），而这里要的就是「一模一样」。读取失败按「不一样」算——
/// 一侧正被占用时宁可多报一项，也不要漏掉。
fn same_content(a: &Path, b: &Path) -> bool {
    let (Ok(mut fa), Ok(mut fb)) = (fs::File::open(a), fs::File::open(b)) else {
        return false;
    };
    let mut ba = vec![0u8; CHUNK];
    let mut bb = vec![0u8; CHUNK];
    loop {
        let na = read_chunk(&mut fa, &mut ba);
        let nb = read_chunk(&mut fb, &mut bb);
        if na != nb || ba[..na] != bb[..nb] {
            return false;
        }
        if na == 0 {
            return true;
        }
    }
}

fn read_chunk(file: &mut fs::File, buf: &mut [u8]) -> usize {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    filled
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 造一个文件并把修改时间钉死：内容相同、大小相同而时间不同这类组合，
    /// 靠真实时钟 sleep 是测不准的。
    fn put(root: &Path, rel: &str, bytes: &[u8], modified: u64) {
        let path = root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(bytes).unwrap();
        file.set_times(
            fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(modified)),
        )
        .unwrap();
    }

    fn tree(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("svn_manager_localdiff_{name}"));
        let local = root.join("local");
        let server = root.join("server");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&local).unwrap();
        fs::create_dir_all(&server).unwrap();
        (local, server)
    }

    #[test]
    fn splits_into_three_buckets() {
        let (local, server) = tree("buckets");
        put(&local, "same.txt", b"aaa", 1_700_000_000);
        put(&server, "same.txt", b"aaa", 1_700_000_000);
        put(&local, "size.txt", b"aaa", 1_700_000_000);
        put(&server, "size.txt", b"aaaa", 1_700_000_000);
        put(&local, "src/only_local.java", b"x", 1_700_000_000);
        put(&server, "src/only_server.java", b"y", 1_700_000_000);
        let s = compare(&local, &server, "").unwrap();
        assert_eq!((s.differing, s.only_local, s.only_server, s.same), (1, 1, 1, 1));
        assert_eq!(s.total(), 3);
        assert_eq!(s.samples.len(), 3, "三类都要进抽样");
    }

    /// 这是整个判定的要点：BC 合过的文件时间戳会变，光看时间会把它们全报成差异。
    #[test]
    fn same_size_different_timestamp_is_decided_by_content() {
        let (local, server) = tree("timestamp");
        put(&local, "merged.txt", b"hello", 1_700_000_500);
        put(&server, "merged.txt", b"hello", 1_700_000_100);
        put(&local, "edited.txt", b"hello", 1_700_000_500);
        put(&server, "edited.txt", b"hellp", 1_700_000_100);
        let s = compare(&local, &server, "").unwrap();
        assert_eq!(s.differing, 1, "同大小同时间戳但内容不同的那项要按内容算成差异");
        assert_eq!(s.same, 1, "只是时间戳不同的那项不算差异");
        assert_eq!(s.samples, vec!["edited.txt".to_owned()]);
    }

    #[test]
    fn path_case_is_ignored_on_windows() {
        let (local, server) = tree("case");
        put(&local, "Src/A.TXT", b"aaa", 1_700_000_000);
        put(&server, "src/a.txt", b"aaa", 1_700_000_000);
        let s = compare(&local, &server, "").unwrap();
        assert_eq!((s.same, s.differing, s.only_local, s.only_server), (1, 0, 0, 0));
    }

    /// 筛选串按本机配置里的真实写法解析：目录名模式带尾 `\`，`.project` 这类是文件名。
    #[test]
    fn bc_filter_excludes_dirs_at_any_depth_and_named_files() {
        let (local, server) = tree("filter");
        let filter = r"-*.iml;-.project;-log4j2.xml;-.\.idea\;-bin\;-package*.json";
        // 只在一侧出现、又被筛选排除的项，都不该进差异数
        put(&local, "bin/a.class", b"x", 1_700_000_000);
        put(&local, "src/main/bin/b.class", b"x", 1_700_000_000);
        put(&local, ".idea/work.xml", b"x", 1_700_000_000);
        put(&local, ".project", b"x", 1_700_000_000);
        put(&local, "log4j2.xml", b"x", 1_700_000_000);
        put(&local, "package-lock.json", b"x", 1_700_000_000);
        put(&local, "project.iml", b"x", 1_700_000_000);
        put(&server, "keep.java", b"x", 1_700_000_000);
        let s = compare(&local, &server, filter).unwrap();
        assert_eq!(s.only_local, 0, "被筛掉的都该不算差异");
        assert_eq!((s.only_server, s.differing), (1, 0), "筛选没命中的服务器端项照报");
    }

    #[test]
    fn svn_metadata_never_counts() {
        let (local, server) = tree("svn");
        put(&server, ".svn/entries.db", b"x", 1_700_000_000);
        put(&server, "src/.svn/pristine/00", b"x", 1_700_000_000);
        let s = compare(&local, &server, "").unwrap();
        assert_eq!(s.only_server, 0, "工作副本元数据不算「本地端缺失」");
    }

    /// 预算用满时不假装比过：没读内容的那些单独记 unverified，也不混进抽样。
    #[test]
    fn verify_budget_reports_unverified_instead_of_guessing() {
        let (local, server) = tree("budget");
        for i in 0..6 {
            put(&local, &format!("src/f{i}.txt"), b"hello", 1_700_000_500);
            put(&server, &format!("src/f{i}.txt"), b"hello", 1_700_000_100);
        }
        let tight = Limits { max_files: 2, max_time: Duration::from_secs(60) };
        let s = compare_with(&local, &server, "", &tight).unwrap();
        assert_eq!(s.same, 2, "预算内的照常核对");
        assert_eq!((s.unverified, s.differing, s.samples.len()), (4, 0, 0));
    }

    /// 标记、卡片 tooltip、整卡 tooltip、输出日志四处共用这一句措辞：
    /// 真有没核对的就不许说「无差异」，哪怕当前数是 0
    #[test]
    fn label_wording_follows_the_count() {
        let clean = Summary { same: 120, ..Default::default() };
        assert_eq!(clean.label(), "本地无差异");
        assert!(clean.is_clean());
        let pending = Summary { unverified: 3, ..Default::default() };
        assert_eq!(pending.label(), "本地差异 0 项");
        assert!(!pending.is_clean());
        let hit = Summary { differing: 5, only_local: 2, only_server: 1, ..Default::default() };
        assert_eq!(hit.label(), "本地差异 8 项");
    }

    /// 主页的数要和 BC 显示的数对上：BC 的文件夹对比默认忽略 BOM、行尾、行尾空白、
    /// 末尾的换行，这四类各造一个都不许算差异；真改了字的、二进制文件、以及只差结尾
    /// 空行的照旧要算。
    #[test]
    fn bc_ignores_bom_line_endings_and_trailing_blanks() {
        let (local, server) = tree("bc_rules");
        const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
        let t = 1_700_000_000;
        put(&local, "bom.txt", b"hello\n", t);
        put(&server, "bom.txt", &[BOM, b"hello\n"].concat(), t + 7);
        put(&local, "eol.txt", b"a\nb\n", t);
        put(&server, "eol.txt", b"a\r\nb\r\n", t + 7);
        put(&local, "blanks.txt", b"a  \nb\t\n", t);
        put(&server, "blanks.txt", b"a\nb\n", t + 7);
        put(&local, "eof.txt", b"a\nb", t);
        put(&server, "eof.txt", b"a\nb\n", t + 7);
        put(&local, "real.txt", b"a\n", t);
        put(&server, "real.txt", b"b\n", t + 7);
        // 二进制里的 0D 0A 不是换行，这类只能按字节判
        put(&local, "raw.bin", &[0u8, 1, 2, 0x0D, 0x0A], t);
        put(&server, "raw.bin", &[0u8, 1, 2], t + 7);
        // 只差结尾空行：BC 默认不忽略这一类，我们也不忽略——宁可多报一项，不要漏
        put(&local, "blank.txt", b"a\n\n", t);
        put(&server, "blank.txt", b"a\n", t + 7);
        let s = compare(&local, &server, "").unwrap();
        assert_eq!((s.differing, s.trivial, s.only_local, s.only_server), (3, 4, 0, 0));
        assert_eq!(s.total(), 3, "只差 BOM / 行尾 / 行尾空白 / 末尾换行的不该进差异数");
        assert!(
            s.samples
                .iter()
                .all(|p| p == "real.txt" || p == "raw.bin" || p == "blank.txt"),
            "抽样里只该有真不一样的：{:?}",
            s.samples
        );
    }

    #[test]
    fn unreadable_target_is_an_error_not_zero() {
        let root = std::env::temp_dir().join("svn_manager_localdiff_missing");
        let server = root.join("server");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&server).unwrap();
        let err = compare(&root.join("nope"), &server, "").unwrap_err();
        assert!(err.contains("本地开发侧"), "应说明是哪一侧读不到：{err}");
    }

    /// 真机核对：拿本机配置里每一对「本地端 ↔ 工作副本」跑一遍，只报计数、耗时和筛选，
    /// 不把整个 Config 打出来（里面有凭据）。依赖本机目录、要跑几秒，平时跳过。
    #[test]
    #[ignore = "读本机真实目录，核对时手动跑 cargo test localdiff -- --ignored --nocapture"]
    fn real_dirs_report_counts() {
        let cfg = crate::config::Config::load();
        for dir in &cfg.dirs {
            let target = dir.bc_target.trim_matches('"');
            if target.is_empty() {
                println!("{}：没绑定本地端，跳过", dir.path);
                continue;
            }
            let started = Instant::now();
            let summary = compare(Path::new(target), Path::new(&dir.path), &dir.bc_filter);
            match summary {
                Ok(s) => println!(
                    "{} ↔ {}：差异 {}（内容 {} · 仅本地 {} · 仅服务器 {}）一致 {} 按BC口径算一致 {} 未核对 {} · {:.1}s｜筛选「{}」",
                    dir.label,
                    Path::new(target).file_name().unwrap_or_default().to_string_lossy(),
                    s.total(),
                    s.differing,
                    s.only_local,
                    s.only_server,
                    s.same,
                    s.trivial,
                    s.unverified,
                    started.elapsed().as_secs_f32(),
                    dir.bc_filter,
                ),
                Err(e) => println!("{}：比对失败 {e}", dir.path),
            }
        }
    }
}

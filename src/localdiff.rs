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
    /// 任意层级都命中的目录名（末级名精确匹配）
    dirs: Vec<String>,
    /// 任意层级都命中的目录名模式（名字里带 `*` `?` 的，整名匹配）
    dir_globs: Vec<Regex>,
    /// 根目录锚定的目录路径模式（BC 写法 `.\out\` / `.\*\.settings\` / `.\HRP.Web\build\`）。
    /// 一律按「整段相对路径」编译成正则：`*` 匹配一段路径、不跨级，命中即整枝排除。
    /// 模式自带层级信息（`^a/b(/.*)?$` 只认根上的 a/b，深层同名项天然不匹配），
    /// 所以不再需要区分「根上的那份」和深层同名项。issue #3 只修了带通配符的那一半，
    /// 纯字面量的**多段**路径（`.\HRP.Web\build\`、`.\Infrastructure\Model\target\`）
    /// 留在按 at_root 判定的老路上，而这类 rel 里必然带 `/`，永远打不中，整片构建产物
    /// 被记成「只在本地开发侧」
    root_dir_globs: Vec<Regex>,
    /// 任意层级都命中的文件名模式
    files: Vec<Regex>,
    /// 只在根目录命中的文件相对路径模式（BC 写法 `.\auth.json` / `\auth.json`）
    root_files: Vec<Regex>,
}

impl Excludes {
    /// 解析 `DirConfig::bc_filter`。真实形态（本机配置解码后）：
    /// `-*.iml;-.project;-log4j2.xml;-.\.idea\;-bin\;-web\`，vue 这边还有
    /// `-package-lock.json;-.\__base_hrformat.vue;-.\auth.json;…`
    /// 分号分隔；只有 `-` 前缀是排除；结尾带 `\` 的是目录名。
    /// 带 `.\` 或 `\` 前缀的是「锚定根目录」：只排除最外层那一份，子目录里的同名
    /// 文件 / 目录照常参与比较——以前把 `.\名字` 当文件名字面量去匹配，永远打不中，
    /// 根目录的排除全部失效，主页就比 BC 多报出两个「只在本地开发侧」。
    /// 不带路径前缀的目录按末级名在任意层级命中，文件按文件名任意层级命中
    /// （前导点属于名字本身，`.project`、`.classpath` 都是文件不是相对路径）。
    /// 名字与路径里的 `*` `?` 都按通配符算（issue #3：`.\*\.settings\` 这种
    /// 「根目录下任意一段路径 + 固定目录名」的 BC 写法必须展开，否则永不命中）。
    /// 锚定根目录的**目录**一律走整段路径正则（`.\out\`、`.\HRP.Web\build\`），
    /// 多段路径的 rel 里必然带 `/`，任何「按末级名 or 按 at_root 判定」的写法都打不中。
    /// `+` 前缀和无前缀的包含模式这里不支持：差异口径要和他用同一份筛选打开的 BC 对得上，
    /// 而 BC 那边包含条件是记在会话里的，我们只有这份排除串。
    pub fn parse(raw: &str) -> Self {
        let mut out = Self::default();
        // 工作副本的元数据目录永远不参与比较：svn 的 .svn 只在服务器端、git 的 .git 只在
        // 本地端，算进去会把整个工作副本的文件数都当成单侧缺失
        out.dirs.push(".svn".to_owned());
        out.dirs.push(".git".to_owned());
        for item in raw.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let Some(pattern) = item.strip_prefix('-') else { continue };
            let pattern = pattern.trim();
            if pattern.is_empty() {
                continue;
            }
            // `.\名字`（BC 面板里填 `.\xxx` 的写法）和 `\名字` 都锚定根目录；
            // 相对路径统一按 `/` 存，和 walk 产出的 rel 一致
            let (root_only, pattern) = match pattern
                .strip_prefix(".\\")
                .or_else(|| pattern.strip_prefix("./"))
                .or_else(|| pattern.strip_prefix('\\'))
            {
                Some(rest) => (true, rest),
                None => (false, pattern),
            };
            let pattern = pattern.replace('\\', "/");
            // 结尾的 `/` 是 BC 里「目录名」的写法：锚定根目录时整段按相对路径算，
            // 不锚定时取末级名在任意层级命中；带通配符的分别进对应的 glob 列表
            if let Some(dir) = pattern.strip_suffix('/') {
                if root_only {
                    let path = dir.trim_matches('/').to_ascii_lowercase();
                    if path.is_empty() {
                        continue;
                    }
                    // 根锚定的目录一律编译成「整段路径」正则：单段的 `.\out\` 只命中根上的
                    // out，多段的 `.\HRP.Web\build\` 也不必再靠 at_root 判定——多段路径的
                    // rel 必然带 `/`，靠 at_root 就永远打不中（本机 biaozhunword 的
                    // build/classes、springbootWork 的 Infrastructure\Model\target 就是
                    // 这么被漏成 121 / 4519 条「只在本地开发侧」的）
                    if let Some(re) = root_dir_glob_regex(&path) {
                        out.root_dir_globs.push(re);
                    }
                } else {
                    let name = dir.rsplit('/').next().unwrap_or("").trim_matches('/');
                    if !name.is_empty() {
                        let name = name.to_ascii_lowercase();
                        if name.contains('*') || name.contains('?') {
                            if let Some(re) = glob_to_regex(&name) {
                                out.dir_globs.push(re);
                            }
                        } else {
                            out.dirs.push(name);
                        }
                    }
                }
                continue;
            }
            if let Some(re) = glob_to_regex(&pattern) {
                if root_only {
                    out.root_files.push(re);
                } else {
                    out.files.push(re);
                }
            }
        }
        out
    }

    /// 扁平清单版的排除判定：给一条相对根目录的路径，判它自己或它的任一级父目录是否被排除。
    /// `walk()` 走的是递归剪枝，提交页拿到的是「一条一条状态条目」、没有目录树可剪，所以自己
    /// 逐级认父段。条目到底是文件还是目录 `StatusEntry` 没带，末级段就两种身份各试一次：
    /// 这样 `bin\` 既命中 `bin` 目录本身，也命中 `src/main/bin/x.class`。
    pub fn hits(&self, rel: &str) -> bool {
        let joined = rel.replace('\\', "/").to_ascii_lowercase();
        let flat = joined.trim_matches('/');
        if flat.is_empty() {
            return false;
        }
        let last = flat.rsplit('/').next().unwrap_or(flat);
        if self.skips_file(last, flat) || self.skips_dir(last, flat) {
            return true;
        }
        for (index, _) in flat.match_indices('/') {
            let parent = &flat[..index];
            let name = parent.rsplit('/').next().unwrap_or("");
            if self.skips_dir(name, parent) {
                return true;
            }
        }
        false
    }

    /// rel 是相对根目录的 `/` 分隔路径
    fn skips_dir(&self, name: &str, rel: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        if self.dirs.iter().any(|d| *d == lower)
            || self.dir_globs.iter().any(|re| re.is_match(&lower))
            || self.matches_file(&lower)
        {
            return true;
        }
        // 锚定根目录的模式自带层级信息，直接拿 rel 整段匹配就够了：
        // 目录走 `^a/b(/.*)?$` 这种路径正则，文件走 `^a/b.txt$`；
        // `x/a/b` 天然不匹配，不用再靠 at_root 区分根上和深层同名项
        self.root_hit(rel)
            || self.root_dir_globs.iter().any(|re| re.is_match(&rel.to_ascii_lowercase()))
    }

    fn skips_file(&self, name: &str, rel: &str) -> bool {
        self.matches_file(&name.to_ascii_lowercase()) || self.root_hit(rel)
    }

    /// 根目录锚定的文件条目：rel（已统一小写）整段等于模式才算
    fn root_hit(&self, rel: &str) -> bool {
        let lower = rel.to_ascii_lowercase();
        self.root_files.iter().any(|re| re.is_match(&lower))
    }

    fn matches_file(&self, lower: &str) -> bool {
        self.files.iter().any(|re| re.is_match(lower))
    }
}

/// 把 BC 的名称模式编译成整串匹配的正则（`*` → 一段内任意字符、`?` → 单字符、
/// 其余逐字符转义，统一小写）。`*` 不跨 `/`：路径模式里它按「一段路径」算，
/// 文件名模式匹配的本来就是裸文件名，两种口径在这里汇合成同一个实现。
/// 不含通配符的模式同样走这里——`.project`、`log4j2.xml` 就是要按字面名精确命中。
fn glob_to_regex(pattern: &str) -> Option<Regex> {
    Regex::new(&format!("^{}$", glob_body(pattern))).ok()
}

/// 根目录锚定的目录路径模式：既匹配目录本身，也匹配它下面的任意内容
fn root_dir_glob_regex(pattern: &str) -> Option<Regex> {
    Regex::new(&format!("^{}(/.*)?$", glob_body(pattern))).ok()
}

/// 模式的正则主体（不带锚点）：`*` → `[^/]*`、`?` → `[^/]`、其余转义，统一小写
fn glob_body(pattern: &str) -> String {
    let mut body = String::with_capacity(pattern.len() * 2);
    for ch in pattern.to_ascii_lowercase().chars() {
        match ch {
            '*' => body.push_str("[^/]*"),
            '?' => body.push_str("[^/]"),
            _ => body.push_str(&regex::escape(&ch.to_string())),
        }
    }
    body
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
    // 抽样分两堆：内容不同的排前面，单侧缺失的排后面——上万的单侧缺失不该把
    // 真差异挤出抽样上限（issue #3：50 条抽样全是 bin 构建产物，真差异翻不到）
    let mut diff_samples: Vec<String> = Vec::new();
    let mut other_samples: Vec<String> = Vec::new();
    let local_owned = local.to_owned();
    walk(local, local, &ex, 0, &mut |rel, size, modified| {
        let key = rel.to_ascii_lowercase();
        let matched = index.remove(&key);
        let Some(entry) = matched else {
            summary.only_local += 1;
            push_capped(&mut other_samples, rel);
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
                push_capped(&mut diff_samples, rel);
            }
        }
    });

    summary.only_server = index.len();
    for entry in index.values() {
        push_capped(&mut other_samples, &entry.rel);
    }
    diff_samples.sort();
    other_samples.sort();
    summary.samples = diff_samples
        .into_iter()
        .chain(other_samples)
        .take(SAMPLE_MAX)
        .collect();
    Ok(summary)
}

fn push_capped(list: &mut Vec<String>, rel: &str) {
    if list.len() < SAMPLE_MAX {
        list.push(rel.to_owned());
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
            if ex.skips_dir(&name, &rel) {
                continue;
            }
            walk(root, &child, ex, depth + 1, sink);
        } else if file_type.is_file() && !ex.skips_file(&name, &rel) {
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

// ============================================================ 诊断报告
//
// 主页只报「差异 N 项」，可用户拿 Beyond Compare 打开却看不到差在哪——
// 比对把读失败的文件按「不同」算、结尾空行按差异算，这些都和 BC 的显示对不上，
// 而且多半在别人机器上才出现，本地复现不了。所以把每一项判定展开成文字报告：
// 两边的大小和修改时间、判成不同的原因、第一个差异字节、差在哪些行，
// 让用户导出这个文件发回来，看一眼就知道是占用、行尾还是真改了内容。

/// 报告里每类最多展开多少个文件：报告是发回来人工看的，几百个文件没人读
const LISTING_MAX: usize = 100;
/// 每个不同的文件最多摘录多少对行
const LINES_MAX: usize = 5;
/// 行摘录里每行最多多少字节，超了截断（汉字一个 3 字节，别切在半个字上）
const LINE_BYTES: usize = 150;
/// 差异处的十六进制对照，每行多少字节
const HEX_PER_LINE: usize = 16;

/// 单个文件的诊断结论。
enum Inspect {
    /// 字节完全一样，只是大小或修改时间对不上
    Same,
    /// 只差 BOM / 行尾 / 行尾空白 / 末尾换行（BC 口径算一致）
    Equivalent(String),
    /// 一侧打不开或读不了：常规比对把它按「不同」算，多半是文件被临时占用
    Unreadable { side: &'static str, message: String },
    /// 超过 16MB 只做了逐字节比较，没做归一化
    TooBig { first: Option<usize> },
    /// 二进制内容不同
    Binary { first: Option<usize> },
    /// 文本内容不同：first 是第一个差异字节的位置，lines 是开头几对不同的行
    ///（本地行号、服务器行号、本地摘录、服务器摘录——两侧行号分开算，插入删除对不上号）
    Text { first: usize, lines: Vec<(usize, usize, String, String)> },
}

/// 两棵树都再走一遍，把判定逐项展开成报告文本。
/// 和 `compare` 用同一套筛选与预算，所以数字能对得上主页。
pub fn diagnose(local: &Path, server: &Path, filter: &str) -> Result<String, String> {
    if !local.is_dir() {
        return Err(format!("本地开发侧目录读不到：{}", local.display()));
    }
    if !server.is_dir() {
        return Err(format!("服务器端目录读不到：{}", server.display()));
    }
    let ex = Excludes::parse(filter);
    let start = Instant::now();

    let mut index: HashMap<String, Entry> = HashMap::new();
    walk(server, server, &ex, 0, &mut |rel, size, modified| {
        index.insert(rel.to_ascii_lowercase(), Entry { rel: rel.to_owned(), size, modified });
    });

    let mut differing: Vec<(String, u64, u64, u64, u64, Inspect)> = Vec::new();
    let mut same_looks: Vec<String> = Vec::new();
    let mut same_looks_total = 0usize;
    let mut equivalent: Vec<String> = Vec::new();
    let mut only_local: Vec<(String, u64, u64)> = Vec::new();
    let mut counts = Summary::default();
    let local_owned = local.to_owned();
    walk(local, local, &ex, 0, &mut |rel, size, modified| {
        let key = rel.to_ascii_lowercase();
        let Some(entry) = index.remove(&key) else {
            counts.only_local += 1;
            only_local.push((rel.to_owned(), size, modified));
            return;
        };
        if entry.size == size && entry.modified == modified {
            counts.same += 1;
            return;
        }
        if start.elapsed() > Limits::default().max_time {
            counts.unverified += 1;
            return;
        }
        let inspect = inspect(&local_owned.join(rel), &server.join(&entry.rel));
        match inspect {
            Inspect::Same => {
                counts.same += 1;
                same_looks_total += 1;
                if same_looks.len() < LISTING_MAX {
                    same_looks.push(rel.to_owned());
                }
            }
            Inspect::Equivalent(reason) => {
                counts.trivial += 1;
                if equivalent.len() < LISTING_MAX {
                    equivalent.push(format!("{rel}（{reason}）"));
                }
            }
            Inspect::Unreadable { side, message } => {
                counts.differing += 1;
                if differing.len() < LISTING_MAX {
                    differing.push((rel.to_owned(), size, modified, entry.size, entry.modified, Inspect::Unreadable { side, message }));
                }
            }
            Inspect::TooBig { first } | Inspect::Binary { first } => {
                counts.differing += 1;
                if differing.len() < LISTING_MAX {
                    differing.push((rel.to_owned(), size, modified, entry.size, entry.modified, match inspect {
                        Inspect::TooBig { .. } => Inspect::TooBig { first },
                        _ => Inspect::Binary { first },
                    }));
                }
            }
            Inspect::Text { first, lines } => {
                counts.differing += 1;
                if differing.len() < LISTING_MAX {
                    differing.push((rel.to_owned(), size, modified, entry.size, entry.modified, Inspect::Text { first, lines }));
                }
            }
        }
    });
    counts.only_server = index.len();
    let mut only_server: Vec<(String, u64, u64)> = index
        .into_values()
        .map(|e| (e.rel, e.size, e.modified))
        .collect();
    only_server.sort_by(|a, b| a.0.cmp(&b.0));
    only_local.sort_by(|a, b| a.0.cmp(&b.0));

    // ---------------- 拼报告
    let mut out = String::new();
    out.push_str("本地差异诊断报告\r\n");
    out.push_str(&format!("程序版本 {}\r\n", crate::APP_VERSION));
    out.push_str(&format!("生成时间 {}\r\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S")));
    out.push_str(&format!("本地开发侧：{}\r\n", local.display()));
    out.push_str(&format!("工作副本（服务器端）：{}\r\n", server.display()));
    out.push_str(&format!("对比筛选条件：{}\r\n", if filter.trim().is_empty() { "（未设置）" } else { filter }));
    out.push_str("判定口径：与 Beyond Compare 默认一致，只差 BOM / 行尾 / 行尾空白 / 末尾换行算一致\r\n");
    out.push_str(&format!(
        "汇总：内容不同 {} · 只在本地开发侧 {} · 只在服务器端 {} · 一致 {} · 按BC口径算一致 {} · 未核对 {}\r\n",
        counts.differing, counts.only_local, counts.only_server, counts.same, counts.trivial, counts.unverified
    ));
    out.push_str(&format!("扫描用时 {:.1} 秒\r\n\r\n", start.elapsed().as_secs_f32()));

    if differing.is_empty() {
        out.push_str("== 内容不同的文件 ==\r\n（无）\r\n\r\n");
    } else {
        out.push_str(&format!("== 内容不同的文件（展开 {} / 共 {} 项）==\r\n\r\n", differing.len(), counts.differing));
        for (no, (rel, lsize, ltime, ssize, stime, inspect)) in differing.iter().enumerate() {
            out.push_str(&format!("【{}】{}\r\n", no + 1, rel));
            out.push_str(&format!("    本地端     大小 {} · 修改 {}\r\n", lsize, stamp(*ltime)));
            out.push_str(&format!("    服务器端   大小 {} · 修改 {}\r\n", ssize, stamp(*stime)));
            match inspect {
                Inspect::Unreadable { side, message } => {
                    out.push_str(&format!(
                        "    判定：{side}读不出来（{message}）\r\n    常规比对把这类按「不同」算——多半是 IDE / 编译 / 杀毒软件临时占用，\r\n    Beyond Compare 那时候能正常打开，就出现「本软件报差异、BC 看不到」。\r\n    过一会儿重新检测，这一项多半自己消失。\r\n\r\n"
                    ));
                }
                Inspect::TooBig { first } => {
                    out.push_str("    判定：超过 16MB，只做了逐字节比较\r\n");
                    if let Some(at) = first {
                        out.push_str(&format!("    第一个差异字节：第 {at} 字节（0x{at:X}）\r\n"));
                    }
                    out.push_str("\r\n");
                }
                Inspect::Binary { first } => {
                    out.push_str("    判定：二进制内容不同\r\n");
                    if let Some(at) = first {
                        out.push_str(&format!("    第一个差异字节：第 {at} 字节（0x{at:X}）\r\n"));
                    }
                    out.push_str("\r\n");
                }
                Inspect::Text { first, lines } => {
                    out.push_str(&format!("    判定：文本内容不同，第一个差异字节：第 {first} 字节（0x{first:X}）\r\n"));
                    // 差异处十六进制对照：编码不同（GBK↔UTF-8）、看不见的字符在这段里现形
                    if let Ok(a) = fs::read(local.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))) {
                        if let Ok(b) = fs::read(server.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))) {
                            let dump = hex_diff(&a, &b, *first);
                            if !dump.is_empty() {
                                out.push_str("    差异处字节对照（本地 / 服务器）：\r\n");
                                for line in dump.lines() {
                                    out.push_str(&format!("      {line}\r\n"));
                                }
                            }
                        }
                    }
                    if !lines.is_empty() {
                        out.push_str("    内容摘录（转义显示，\\r 回车 \\t 制表；行号按换行计）：\r\n");
                        for (ln, rn, left, right) in lines {
                            out.push_str(&format!("      本地   第 {} 行: {left}\r\n", ln + 1));
                            out.push_str(&format!("      服务器 第 {} 行: {right}\r\n", rn + 1));
                        }
                    }
                    out.push_str("\r\n");
                }
                Inspect::Same | Inspect::Equivalent(_) => {}
            }
        }
    }

    if equivalent.is_empty() {
        out.push_str("== 按 BC 口径算一致（不计入差异数）==\r\n（无）\r\n\r\n");
    } else {
        out.push_str(&format!(
            "== 按 BC 口径算一致（不计入差异数，共 {} 项，列 {}）==\r\n",
            counts.trivial,
            equivalent.len()
        ));
        for item in &equivalent {
            out.push_str(&format!("- {item}\r\n"));
        }
        out.push_str("\r\n");
    }
    if !same_looks.is_empty() {
        out.push_str(&format!(
            "== 大小或修改时间不同、但内容一致（{} 项，列 {}）==\r\n这些文件多半是 Beyond Compare 合并时重写了时间戳，属于正常现象。\r\n",
            same_looks_total,
            same_looks.len()
        ));
        for item in &same_looks {
            out.push_str(&format!("- {item}\r\n"));
        }
        out.push_str("\r\n");
    }
    listing(&mut out, "只在本地开发侧（工作副本里没有）", &only_local, counts.only_local);
    listing(&mut out, "只在服务器端（本地开发侧里没有）", &only_server, counts.only_server);
    if counts.unverified > 0 {
        out.push_str(&format!(
            "== 未核对 ==\r\n有 {} 个文件因为预算用满没有读内容，上面的差异数是不完整的下界。\r\n",
            counts.unverified
        ));
    }
    Ok(out)
}

fn listing(out: &mut String, title: &str, items: &[(String, u64, u64)], total: usize) {
    if items.is_empty() {
        out.push_str(&format!("== {title} ==\r\n（无）\r\n\r\n"));
        return;
    }
    out.push_str(&format!("== {title}（共 {total} 项，列 {}）==\r\n", items.len()));
    for (rel, size, modified) in items.iter().take(LISTING_MAX) {
        out.push_str(&format!("- {rel}（大小 {size} · 修改 {}）\r\n", stamp(*modified)));
    }
    out.push_str("\r\n");
}

fn stamp(secs: u64) -> String {
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| secs.to_string())
}

/// 逐项读内容判定，返回带原因的结论。与 `verify` 同一套规则，只是把结论拆细了。
fn inspect(a: &Path, b: &Path) -> Inspect {
    let (mut fa, mut fb) = match (fs::File::open(a), fs::File::open(b)) {
        (Ok(fa), Ok(fb)) => (fa, fb),
        (Err(e), _) => return Inspect::Unreadable { side: "本地端", message: e.to_string() },
        (_, Err(e)) => return Inspect::Unreadable { side: "服务器端", message: e.to_string() },
    };
    let mut ba = vec![0u8; CHUNK];
    let mut bb = vec![0u8; CHUNK];
    let mut offset = 0usize;
    let first = loop {
        let na = read_chunk(&mut fa, &mut ba);
        let nb = read_chunk(&mut fb, &mut bb);
        // 先比公共部分，再比长度：一侧是另一侧前缀时，差异就在短文件结尾处
        let common = na.min(nb);
        if let Some(at) = ba[..common].iter().zip(bb[..common].iter()).position(|(x, y)| x != y) {
            break Some(offset + at);
        }
        if na != nb {
            break Some(offset + common);
        }
        offset += na;
        if na == 0 {
            break None;
        }
    };
    let Some(first) = first else {
        return Inspect::Same;
    };
    let big = |p: &Path| fs::metadata(p).map(|m| m.len() > NORMALIZE_MAX).unwrap_or(true);
    if big(a) || big(b) {
        return Inspect::TooBig { first: Some(first) };
    }
    let (x, y) = match (fs::read(a), fs::read(b)) {
        (Ok(x), Ok(y)) => (x, y),
        (Err(e), _) => return Inspect::Unreadable { side: "本地端", message: e.to_string() },
        (_, Err(e)) => return Inspect::Unreadable { side: "服务器端", message: e.to_string() },
    };
    if !looks_text(&x) || !looks_text(&y) {
        return Inspect::Binary { first: Some(first) };
    }
    if normalize(&x) == normalize(&y) {
        return Inspect::Equivalent(trivial_reason(&x, &y));
    }
    Inspect::Text { first, lines: differing_lines(&x, &y) }
}

/// Equivalent 的原因：从粗到细各归一化一步，第一步打平的那步就是差异所在。
fn trivial_reason(x: &[u8], y: &[u8]) -> String {
    const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
    let a = x.strip_prefix(BOM).unwrap_or(x);
    let b = y.strip_prefix(BOM).unwrap_or(y);
    if a == b {
        return "只差 BOM".to_owned();
    }
    let fold = |v: &[u8]| {
        let mut out = Vec::with_capacity(v.len());
        let mut i = 0;
        while i < v.len() {
            if v[i] == b'\r' {
                out.push(b'\n');
                i += usize::from(v.get(i + 1) == Some(&b'\n'));
            } else {
                out.push(v[i]);
            }
            i += 1;
        }
        out
    };
    let (fa, fb) = (fold(a), fold(b));
    if fa == fb {
        return "只差行尾（CRLF ↔ LF）".to_owned();
    }
    let trim = |v: &[u8]| -> Vec<u8> {
        v.split(|c| *c == b'\n')
            .map(|line| match line.iter().rposition(|c| !matches!(c, b' ' | b'\t' | 0x0B | 0x0C)) {
                Some(kept) => line[..=kept].to_vec(),
                None => Vec::new(),
            })
            .collect::<Vec<_>>()
            .join(&b'\n')
    };
    let (ta, tb) = (trim(&fa), trim(&fb));
    if ta == tb {
        return "只差行尾空白（空格 / 制表符）".to_owned();
    }
    let eof = |v: &[u8]| -> Vec<u8> {
        let mut lines: Vec<&[u8]> = v.split(|c| *c == b'\n').collect();
        if lines.last() == Some(&&[][..]) {
            lines.pop();
        }
        lines.join(&b'\n')
    };
    if eof(&ta) == eof(&tb) {
        return "只差结尾的换行".to_owned();
    }
    "多步归一化后才一致".to_owned()
}

/// 开头几对不同的行：两边同步往前走，行一样就各进一步；
/// 行不一样时在小窗口里找下一次对齐的位置，对齐前的行都算差异对。
/// 这不是完整 diff，只是给报告摘开头几处，够看出改了什么。
/// 行号按换行计（0 基，打印时 +1）——以前这里给的是行首字节偏移，
/// 大文件就摘出「第 2127 行」这种怪行号（issue #3）。
fn differing_lines(x: &[u8], y: &[u8]) -> Vec<(usize, usize, String, String)> {
    fn split(v: &[u8]) -> Vec<(usize, &[u8])> {
        let mut lines = Vec::new();
        let mut start = 0usize;
        let mut no = 0usize;
        for (i, b) in v.iter().enumerate() {
            if *b == b'\n' {
                lines.push((no, &v[start..i]));
                no += 1;
                start = i + 1;
            }
        }
        lines.push((no, &v[start..]));
        lines
    }
    let (la, lb) = (split(x), split(y));
    let (mut i, mut j) = (0usize, 0usize);
    let mut found = Vec::new();
    while i < la.len() && j < lb.len() && found.len() < LINES_MAX {
        if la[i].1 == lb[j].1 {
            i += 1;
            j += 1;
            continue;
        }
        // 在前后各 20 行里找重新对齐的点，找不到就整块算不同、直接收尾
        let mut aligned = None;
        'seek: for skip in 1..=20 {
            if i + skip < la.len() && la[i + skip].1 == lb[j].1 {
                aligned = Some((i + skip, j));
                break 'seek;
            }
            if j + skip < lb.len() && la[i].1 == lb[j + skip].1 {
                aligned = Some((i, j + skip));
                break 'seek;
            }
        }
        let (ni, nj) = aligned.unwrap_or((la.len(), lb.len()));
        for (a, b) in (i..ni).zip(j..nj) {
            if found.len() >= LINES_MAX {
                break;
            }
            if la[a].1 != lb[b].1 {
                found.push((la[a].0, lb[b].0, excerpt(la[a].1), excerpt(lb[b].1)));
            }
        }
        i = ni;
        j = nj;
    }
    found
}

/// 一行摘录：先按 UTF-8 解（解不开用替换符），控制字符转义，超长截断。
fn excerpt(line: &[u8]) -> String {
    let cut = |mut text: String| -> String {
        if text.len() > LINE_BYTES {
            text.truncate(LINE_BYTES);
            text.push('…');
        }
        text
    };
    let text = String::from_utf8_lossy(line).replace('\r', "\\r").replace('\t', "\\t");
    cut(text)
}

/// 第一个差异处的十六进制对照：差异位置向前取 8 字节、向后取 HEX_PER_LINE*3 字节，
/// 两侧各按 16 字节一行排出十六进制和可见字符。
fn hex_diff(a: &[u8], b: &[u8], at: usize) -> String {
    let start = at.saturating_sub(8);
    let end = (at + HEX_PER_LINE * 3).min(a.len().max(b.len()));
    let dump = |label: &str, data: &[u8]| -> String {
        let mut out = String::new();
        let mut pos = start;
        while pos < end.min(data.len()) {
            let stop = (pos + HEX_PER_LINE).min(end).min(data.len());
            let row = &data[pos..stop];
            let hex: Vec<String> = row.iter().map(|byte| format!("{byte:02X}")).collect();
            let text: String = row
                .iter()
                .map(|byte| {
                    if (0x20..0x7F).contains(byte) {
                        *byte as char
                    } else {
                        '·'
                    }
                })
                .collect();
            out.push_str(&format!("{label} 0x{pos:06X}  {:<47}  {text}\r\n", hex.join(" ")));
            pos = stop;
        }
        out
    };
    format!("{}{}", dump("本地  ", a), dump("服务器", b))
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
        let root = std::env::temp_dir().join(format!("repo_manager_localdiff_{name}"));
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

    /// BC 的 `.\名字` 只排除根目录那一份（vue_new 这台机器的真实场景：
    /// `.\__base_hrformat.vue`、`.\auth.json` 排除不掉，主页就比 BC 多报两个
    /// 「只在本地开发侧」）。子目录里的同名文件 / 目录不能被根目录锚定的模式带走。
    #[test]
    fn dot_slash_patterns_anchor_to_root_only() {
        let (local, server) = tree("root_anchor");
        let t = 1_700_000_000;
        let filter = r"-package-lock.json;-.\__base_hrformat.vue;-.\auth.json;-.\.idea\;-bin\";
        put(&local, "__base_hrformat.vue", b"x", t); // 根目录，被排除
        put(&local, "auth.json", b"x", t); // 根目录，被排除
        put(&local, "bin/a.class", b"x", t); // 任意层级目录 bin，被排除
        put(&local, ".idea/work.xml", b"x", t); // 根下 .idea，被排除
        put(&local, "src/auth.json", b"x", t); // 子目录同名文件，不该被带走
        put(&local, "src/.idea/work.xml", b"x", t); // 子目录 .idea，不该被带走
        put(&server, "keep.java", b"x", t);
        let s = compare(&local, &server, filter).unwrap();
        assert_eq!(s.only_local, 2, "只剩子目录里的两个该算差异");
        assert_eq!(s.only_server, 1);
        assert!(s.samples.contains(&"src/auth.json".to_owned()), "{:?}", s.samples);
        assert!(s.samples.contains(&"src/.idea/work.xml".to_owned()), "{:?}", s.samples);
    }

    /// 扁平清单判定要和 `walk()` 的剪枝口径对齐：目录名命中它本身、也命中它底下任意深度的
    /// 文件；根锚定的多段目录只认根上那一份（PR #4 修的就是这种写法永不命中）。
    #[test]
    fn excludes_hits_matches_flat_paths_through_their_parents() {
        let ex = Excludes::parse(r"-bin\;-.project;-.\HRP.Web\build\;-*.log");
        assert!(ex.hits("bin"), "目录条目本身");
        assert!(ex.hits(r"src\main\bin\b.class"), "深层 bin 底下的文件，反斜杠输入一样算");
        assert!(ex.hits("HRP.Web/build/out.txt"), "根锚定的多段目录命中它下面的内容");
        assert!(!ex.hits("x/HRP.Web/build/out.txt"), "根锚定不该把深层同名目录一起带走");
        assert!(ex.hits("debug.log"), "带通配符的文件名");
        assert!(ex.hits("A/B/.PROJECT"), "Windows 路径大小写不算数");
        assert!(!ex.hits("bin2/x.txt"), "名字前缀像但不等于");
        assert!(!ex.hits("src/app.vue"), "没命中的正常条目");
        assert!(ex.hits(".svn/entries"), "工作副本元数据永远排除");
    }

    /// `\名字` 与 `.\名字` 同义，都锚定根目录；子目录里的照常比较
    #[test]
    fn leading_backslash_also_anchors_to_root() {
        let (local, server) = tree("backslash_anchor");
        let t = 1_700_000_000;
        let filter = r"-\auth.json;-\notes\";
        put(&local, "auth.json", b"x", t); // 根目录，被排除
        put(&local, "notes/a.md", b"x", t); // 根下 notes，被排除
        put(&local, "sub/auth.json", b"x", t); // 子目录同名，照常算
        put(&local, "sub/notes/b.md", b"x", t); // 子目录同名目录，照常算
        let s = compare(&local, &server, filter).unwrap();
        assert_eq!(s.only_local, 2, "{:?}", s.samples);
        assert!(s.samples.contains(&"sub/auth.json".to_owned()), "{:?}", s.samples);
        assert!(s.samples.contains(&"sub/notes/b.md".to_owned()), "{:?}", s.samples);
    }

    /// 根锚定的**多段**目录路径（`.\A\B\`）：rel 里必然带 `/`，不能再靠 at_root 判定，
    /// 否则纯字面量的多段路径整片漏排。本机 biaozhunword 的 `.\HRP.Web\build\` 漏成 121 条、
    /// springbootWork 的 `.\Infrastructure\Model\target\` 漏成 4519 条，都是这条规则没命中的结果。
    #[test]
    fn multisegment_root_dir_pattern_excludes_that_path() {
        let (local, server) = tree("multiseg_dir");
        let t = 1_700_000_000;
        let filter = r"-.\HRP.Web\build\;-.\Infrastructure\Model\target\";
        put(&local, "HRP.Web/build/classes/applicationContext.xml", b"x", t); // 命中
        put(&local, "HRP.Web/build/classes/db/migration/V1.sql", b"x", t); // 命中，整枝
        put(&local, "Infrastructure/Model/target/classes/A.class", b"x", t); // 命中
        put(&local, "Infrastructure/Model/src/A.java", b"x", t); // 同级但不在 target 下，照报
        put(&local, "HRP.Web/web/WEB-INF/web.xml", b"x", t); // 不在筛选里，照报
        put(&local, "Other/Infrastructure/Model/target/A.class", b"x", t); // 深层同名，锚定根就不排
        let s = compare(&local, &server, filter).unwrap();
        assert_eq!(s.only_local, 3, "{:?}", s.samples);
        for keep in [
            "Infrastructure/Model/src/A.java",
            "HRP.Web/web/WEB-INF/web.xml",
            "Other/Infrastructure/Model/target/A.class",
        ] {
            assert!(s.samples.contains(&keep.to_owned()), "漏了 {keep}：{:?}", s.samples);
        }
    }

    /// 根锚定的多段文件路径同理（`.\A\B.txt`），以前也永远打不中
    #[test]
    fn multisegment_root_file_pattern_excludes_that_path() {
        let (local, server) = tree("multiseg_file");
        let t = 1_700_000_000;
        let filter = r"-.\Config\app.properties";
        put(&local, "Config/app.properties", b"x", t); // 命中
        put(&local, "Other/Config/app.properties", b"x", t); // 深层同名，照报
        let s = compare(&local, &server, filter).unwrap();
        assert_eq!(s.only_local, 1, "{:?}", s.samples);
        assert_eq!(s.samples, vec!["Other/Config/app.properties".to_owned()]);
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

    // ------------------------------------------------------------ 诊断报告

    /// 报告要把每类判定都展开：真改内容的给出第一个差异字节和行摘录，
    /// 只差行尾的写明原因，大小时间不同但内容一致的单独列出免得吓人。
    #[test]
    fn diagnose_report_expands_each_verdict() {
        let (local, server) = tree("diagnose");
        let t = 1_700_000_000;
        put(&local, "src/Real.java", b"alpha\r\nbeta\r\n", t);
        put(&server, "src/Real.java", b"alpha\nBeta\r\n", t + 7);
        put(&local, "eol.txt", b"x\r\n", t);
        put(&server, "eol.txt", b"x\n", t + 7);
        put(&local, "stamp.txt", b"same", t);
        put(&server, "stamp.txt", b"same", t + 7);
        put(&local, "gone.txt", b"x", t);
        put(&server, "extra.txt", b"y", t);
        let report = diagnose(&local, &server, "").unwrap();
        assert!(report.contains("内容不同 1"), "{report}");
        assert!(report.contains("src/Real.java"));
        assert!(report.contains("第一个差异字节"), "真差异要有字节位置：{report}");
        assert!(report.contains("本地   第 2 行"), "行摘录两侧都要带真实行号：{report}");
        assert!(report.contains("服务器 第 2 行"), "两侧行号分开算：{report}");
        assert!(report.contains("只差行尾"), "Equivalent 要写明原因：{report}");
        assert!(report.contains("stamp.txt"), "只差时间戳的要单独列出：{report}");
        assert!(report.contains("gone.txt"), "只在本地侧的要列出：{report}");
        assert!(report.contains("extra.txt"), "只在服务器侧的要列出：{report}");
        assert!(report.contains("汇总：内容不同 1 · 只在本地开发侧 1 · 只在服务器端 1 · 一致 1"), "汇总数要和主页口径一致：{report}");
    }

    /// 行摘录的行号必须是行号不是字节偏移：前面垫 100 行无关内容，
    /// 真差异在第 101 行——以前这里打印的是行首字节偏移（700 多），
    /// 大文件就摘出「第 2127 行」这种怪行号（issue #3 报告里可见）。
    #[test]
    fn report_line_numbers_count_lines_not_bytes() {
        let (local, server) = tree("line_numbers");
        let filler = "filler line\r\n".repeat(100);
        put(&local, "deep.txt", format!("{filler}mine\r\n").as_bytes(), 1_700_000_000);
        put(&server, "deep.txt", format!("{filler}yours\r\n").as_bytes(), 1_700_000_007);
        let report = diagnose(&local, &server, "").unwrap();
        assert!(report.contains("本地   第 101 行"), "行号按换行计：{report}");
        assert!(report.contains("服务器 第 101 行"), "行号按换行计：{report}");
        // 第一个差异字节在 1300（100 行 × 13 字节），不许混进行号
        assert!(report.contains("第 1300 字节"), "字节位置照常报：{report}");
        assert!(!report.contains("第 1300 行"), "字节偏移不许冒充行号：{report}");
    }

    /// issue #3 根因回归：BC 写法的根目录锚定目录模式里带 `*`（`.\*\.settings\`、
    /// `.\*\bin\`），`*` 匹配一段路径——以前被当字面量存起来永不命中，
    /// bin / .settings 整片漏排，上万条构建产物把真差异淹没。
    #[test]
    fn wildcard_dir_patterns_exclude_at_any_single_level() {
        let (local, server) = tree("issue3_wildcard");
        let t = 1_700_000_000;
        // 真差异：该被数出来
        put(&local, "HRP.GL/config/keep.xml", b"mine", t);
        put(&server, "HRP.GL/config/keep.xml", b"yours", t);
        // 该被 `.\*\.settings\` / `.\*\bin\` 排除的：一段路径 + 固定目录名
        put(&local, "HRP.DRUG/.settings/prefs.xml", b"x", t);
        put(&local, "HRP.DRUG/bin/Drug.class", b"x", t);
        put(&server, "HRP.DRUG/.settings/server_only.xml", b"x", t);
        put(&server, "HRP.WA/bin/Wa.class", b"x", t);
        // 两层深的同名目录：`*` 只匹配一段路径，这里保守地继续比较
        //（宁可多报一项，也不要把 BC 里没排除的判漏）
        put(&local, "a/b/.settings/deep.xml", b"x", t);
        let s = compare(&local, &server, r#"-.\*\.settings\;-.\*\bin\"#).unwrap();
        assert_eq!(
            (s.differing, s.only_local, s.only_server),
            (1, 1, 0),
            "单段通配要排掉各模块下的 bin / .settings，两层深的保守保留：samples={:?}",
            s.samples
        );
        assert_eq!(
            s.samples,
            vec!["HRP.GL/config/keep.xml".to_owned(), "a/b/.settings/deep.xml".to_owned()]
        );
    }

    /// 目录名里带通配符的模式（如 `-*build\`）按整名匹配、任意层级命中
    #[test]
    fn wildcard_dir_names_exclude_at_any_depth() {
        let (local, server) = tree("dir_name_glob");
        let t = 1_700_000_000;
        put(&local, "web/x.html", b"x", t);
        put(&local, "src/sub/web/y.html", b"x", t);
        put(&server, "keep.java", b"x", t);
        let s = compare(&local, &server, r"-*web\").unwrap();
        assert_eq!((s.only_local, s.only_server), (0, 1), "{:?}", s.samples);
    }

    /// 真差异该排在抽样最前面：上万个单侧缺失不许把内容不同的文件挤出上限
    #[test]
    fn differing_files_come_first_in_samples() {
        let (local, server) = tree("sample_priority");
        let t = 1_700_000_000;
        put(&local, "real_diff.txt", b"mine", t);
        put(&server, "real_diff.txt", b"yours", t);
        // 按字母序排在 real_diff 前面的一大波单侧缺失
        for i in 0..60 {
            put(&local, &format!("aaa_only/f{i:03}.bin"), b"x", t);
        }
        let s = compare(&local, &server, "").unwrap();
        assert_eq!(s.differing, 1);
        assert_eq!(
            s.samples.first().map(String::as_str),
            Some("real_diff.txt"),
            "真差异要排最前：{:?}",
            s.samples
        );
    }

    /// 被 IDEA / 编译 / 杀毒独占打开的文件，常规比对按「不同」算——
    /// 这正是用户「BC 看不到差异、本软件却报差异」的常见来路，报告里要写清原因。
    #[cfg(windows)]
    #[test]
    fn locked_file_is_expanded_with_reason() {
        use std::os::windows::fs::OpenOptionsExt;
        let (local, server) = tree("locked");
        let t = 1_700_000_000;
        put(&local, "src/Locked.java", b"local text", t);
        put(&server, "src/Locked.java", b"server text", t + 7);
        let _guard = std::fs::OpenOptions::new().read(true).share_mode(0).open(local.join("src").join("Locked.java")).unwrap();
        let report = diagnose(&local, &server, "").unwrap();
        assert!(report.contains("读不出来"), "{report}");
        assert!(report.contains("内容不同 1"), "{report}");
    }

    /// 报告是给用户用记事本看的，必须能按记事本的习惯断行。
    #[test]
    fn report_lines_end_with_crlf() {
        let (local, server) = tree("crlf");
        put(&local, "a.txt", b"x", 1_700_000_000);
        put(&server, "a.txt", b"y", 1_700_000_000);
        let report = diagnose(&local, &server, "").unwrap();
        assert!(report.contains("\r\n"));
        for (i, byte) in report.bytes().enumerate() {
            if byte == b'\r' {
                assert_eq!(report.as_bytes().get(i + 1), Some(&b'\n'), "第 {i} 字节出现裸 \\r");
            }
        }
    }

    /// 旧目录要清掉，连着两次跑同一个 tree 名不能互相污染。
    /// 两侧时间戳必须错开：大小和时间全同的文件走快速通道算一致，不读内容。
    #[test]
    fn diagnose_is_repeatable() {
        for _ in 0..2 {
            let (local, server) = tree("repeat");
            put(&local, "a.txt", b"x", 1_700_000_000);
            put(&server, "a.txt", b"y", 1_700_000_001);
            let report = diagnose(&local, &server, "").unwrap();
            assert!(report.contains("内容不同 1"), "{report}");
        }
    }

    #[test]
    fn unreadable_target_is_an_error_not_zero() {
        let root = std::env::temp_dir().join("repo_manager_localdiff_missing");
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

//! 个人提交文件数量统计。
//!
//! 口径：数据源就是 `svn log --xml -v` 里的 `<path>` 条目——一次提交涉及几个文件（含目录）
//! 就计几条，同一个文件被提交 3 次算 3 条，与「提交记录」页的「涉及文件（N 项）」一致；
//! 另外并列给出提交次数与按仓库路径去重后的文件数，三个口径对照着看。
//!
//! 图全部用 egui 自带的 `Painter` 手绘（折线 / 直方 / 日历火力图），不引入图表 crate。
//! 数据按目录各起一个后台任务读（`Kind::Stats`），回来后在 `collect` 里按 (版本, 路径) 去重合并。

use std::collections::{HashMap, HashSet};

use chrono::{Datelike, Local, NaiveDate, TimeDelta};
use egui::{
    Align, Align2, Color32, FontId, Frame, Grid, Id, Label, Layout, Pos2, Rangef, Rect, Response,
    RichText, ScrollArea, Sense, Stroke, StrokeKind, TextEdit, TextWrapMode, Ui, Vec2,
};

use crate::jobs::{Data, Kind};
use crate::svn::LogEntry;
use crate::worklog::{card_colors, note_card};
use crate::{ink, Page, SvnApp};

/// 「全部目录（合并）」在页面状态里的取值，与任务池的哨兵编号一致（见 `jobs::Kind::Stats`）
pub const ALL_DIRS: usize = usize::MAX;

/// 火力图按天画到多少天为止；再宽就降级成「年 × 月」格，否则五年要画两百多列
const HEATMAP_DAY_LIMIT: i64 = 400;

// ------------------------------------------------------------------------ 查询条件

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RangePreset {
    Today,
    Week,
    Month,
    Year,
    All,
    Custom,
}

impl RangePreset {
    pub const ALL: [Self; 6] = [
        Self::Today,
        Self::Week,
        Self::Month,
        Self::Year,
        Self::All,
        Self::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Today => "当天",
            Self::Week => "一周",
            Self::Month => "一个月",
            Self::Year => "一年",
            Self::All => "所有",
            Self::Custom => "自定义",
        }
    }

    /// 相对今天往回覆盖多少天（含今天）；「所有」「自定义」返回 None 由调用方处理
    pub fn back_days(self) -> Option<i64> {
        match self {
            Self::Today => Some(0),
            Self::Week => Some(6),
            Self::Month => Some(29),
            Self::Year => Some(364),
            _ => None,
        }
    }

    pub fn hover(self) -> &'static str {
        match self {
            Self::Today => "今天 00:00 到现在",
            Self::Week => "含今天在内的最近 7 天",
            Self::Month => "含今天在内的最近 30 天",
            Self::Year => "含今天在内的最近 365 天",
            Self::All => "不限日期，把每个目录的整个提交史全扫一遍：大仓库要等几十秒",
            Self::Custom => "手填起止日期（含两端），填完点「查询」",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChartKind {
    Line,
    Bar,
    Heatmap,
}

impl ChartKind {
    pub const ALL: [Self; 3] = [Self::Line, Self::Bar, Self::Heatmap];

    pub fn label(self) -> &'static str {
        match self {
            Self::Line => "折线图",
            Self::Bar => "直方图",
            Self::Heatmap => "火力图",
        }
    }

    pub fn hover(self) -> &'static str {
        match self {
            Self::Line => "看趋势：每天（每月）一条折线，面积填充标出文件条目数",
            Self::Bar => "比多少：每格两根柱子，蓝的是文件条目数、橙的是提交次数",
            Self::Heatmap =>
                "GitHub 式日历：列是一周、行是周一到周日，颜色越深当天提交的文件越多",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Granularity {
    Hour,
    Day,
    Month,
}

impl Granularity {
    /// 跨度决定分格：当天按小时，两个月内按天，再宽按月（否则柱子只有两像素宽）
    pub fn pick(days: i64) -> Self {
        if days <= 1 {
            Self::Hour
        } else if days <= 62 {
            Self::Day
        } else {
            Self::Month
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Hour => "按小时",
            Self::Day => "按天",
            Self::Month => "按月",
        }
    }
}

impl Default for Granularity {
    fn default() -> Self {
        Self::Day
    }
}

#[derive(Clone, Debug)]
pub struct Bucket {
    pub key: String,
    pub label: String,
    pub files: usize,
    pub revs: usize,
}

impl Default for Bucket {
    fn default() -> Self {
        Self {
            key: String::new(),
            label: String::new(),
            files: 0,
            revs: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Totals {
    /// 提交次数（版本数，跨目录去重后）
    pub revs: usize,
    /// 文件条目数：各次提交涉及的 path 累加，同一文件改三次算三条
    pub files: usize,
    /// 去重文件数：区间内被本人碰过的不同仓库路径
    pub distinct_files: usize,
    pub active_days: usize,
    pub dirs: usize,
}

/// 一次提交（跨目录去重后的一条版本），统计的所有图形都由它派生
#[derive(Clone, Debug)]
pub struct Commit {
    pub rev: String,
    /// 首次读到它的目录（只为汇总「覆盖目录数」，同一次提交不会重复计）
    pub dir: usize,
    pub day: NaiveDate,
    /// 本地时间 "HH:MM:SS"
    pub time: String,
    pub paths: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeatMode {
    Day,
    Month,
}

impl Default for HeatMode {
    fn default() -> Self {
        Self::Day
    }
}

#[derive(Clone, Debug)]
pub struct HeatCell {
    pub col: usize,
    pub row: usize,
    pub files: usize,
    pub revs: usize,
    /// 悬停时显示在提示里的日期 / 月份
    pub title: String,
    /// 这一格覆盖的日期区间（含两端）：日模式就是那一天，月模式是该月（被统计区间裁过）。
    /// 点格子下钻到「提交记录」页时按它读服务器，不能靠解析 `title`（月模式的标题是中文）。
    pub from: NaiveDate,
    pub to: NaiveDate,
}

/// 火力图的格子表：纯函数算好行列，painter 只负责按格子画方块
#[derive(Clone, Debug, Default)]
pub struct Heat {
    pub mode: HeatMode,
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<HeatCell>,
    pub col_labels: Vec<(usize, String)>,
    pub row_labels: Vec<(usize, String)>,
    pub max: usize,
}

#[derive(Clone, Debug, Default)]
pub struct StatsView {
    pub buckets: Vec<Bucket>,
    pub gran: Granularity,
    pub totals: Totals,
    pub heat: Heat,
    /// 已回 / 应回的目录数（分开算，「全部目录」时能看出还在等几个）
    pub received: usize,
    pub expected: usize,
    /// 时间字段读不出日期的记录数（svn 抽风时不至于静默少算）
    pub skipped: usize,
}

#[derive(Clone, Debug)]
pub struct StatsPage {
    /// `ALL_DIRS` 表示全部目录合并
    pub dir: usize,
    pub range: RangePreset,
    pub custom_from: String,
    pub custom_to: String,
    pub chart: ChartKind,
    /// 查询代号：换条件时 +1，晚到的旧结果凭它丢弃
    pub epoch: u64,
    pub expected: Vec<usize>,
    pub done: Vec<(usize, Vec<LogEntry>)>,
    pub failed: Vec<(usize, String)>,
    pub error: String,
    /// 这次查询实际生效的日期区间（None = 所有）。结果回来按它裁剪，
    /// 用户在等待期间再改输入框也不会把已经在跑的这批结果读歪。
    pub span: Option<(NaiveDate, NaiveDate)>,
    /// 拼给 svn 的 `-r` 串与作者，只为把「这批数据是怎么来的」显示出来
    pub revspec: String,
    pub author: String,
    pub view: StatsView,
}

impl StatsPage {
    pub fn new() -> Self {
        Self {
            dir: ALL_DIRS,
            range: RangePreset::Month,
            custom_from: String::new(),
            custom_to: String::new(),
            chart: ChartKind::Bar,
            epoch: 0,
            expected: Vec::new(),
            done: Vec::new(),
            failed: Vec::new(),
            error: String::new(),
            span: None,
            revspec: String::new(),
            author: String::new(),
            view: StatsView::default(),
        }
    }

    /// 当前条件对应的日期区间；`所有` 为 None，自定义区间填错时返回可直接显示的原因
    pub fn resolve(&self, today: NaiveDate) -> Result<Option<(NaiveDate, NaiveDate)>, String> {
        resolve_range(self.range, &self.custom_from, &self.custom_to, today)
    }
}

/// 预设 / 自定义输入 → 实际日期区间（`None` = 不限日期）。
/// 统计页与历史页共用：两边的区间语义必须一致，否则从统计图跳到「提交记录」页会看到另一批日期。
pub fn resolve_range(
    preset: RangePreset,
    from: &str,
    to: &str,
    today: NaiveDate,
) -> Result<Option<(NaiveDate, NaiveDate)>, String> {
    if let Some(back) = preset.back_days() {
        return Ok(Some((today - TimeDelta::days(back), today)));
    }
    if preset != RangePreset::Custom {
        return Ok(None);
    }
    let start = parse_date(from).ok_or_else(|| {
        format!(
            "起始日期「{}」看不懂，要 YYYY-MM-DD 这种写法，例如 {}",
            from.trim(),
            today.format("%Y-%m-%d")
        )
    })?;
    let end = match to.trim() {
        "" => today,
        text => parse_date(text)
            .ok_or_else(|| format!("结束日期「{text}」看不懂，要 YYYY-MM-DD；留空表示到今天"))?,
    };
    if end < start {
        return Err(format!(
            "结束日期 {} 早于起始日期 {start}",
            to.trim()
        ));
    }
    Ok(Some((start, end)))
}

pub fn parse_date(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d").ok()
}

/// svn `-r` 的日期区间串。两端各向外让一两天，一次解决三个坑：
/// `{YYYY-MM-DD}` 到底按本地还是 UTC 解释（不同 svn 版本行为不一致，让出来两种都对）、
/// 当天这种 `{D}:{D}` 退化区间会查不到东西、以及区间边界上的提交被切掉。
/// 多取的部分在 `collect` 里按 `span` 裁掉，图形上的数字始终只算用户要的那几天。
pub fn revspec(span: Option<(NaiveDate, NaiveDate)>) -> String {
    match span {
        None => "HEAD:1".to_owned(),
        Some((from, to)) => {
            let lo = from - TimeDelta::days(1);
            let hi = to + TimeDelta::days(2);
            // 新 → 旧：正序区间配 -l 会保留区间里最旧的 N 条，倒着写才不会反过来
            format!("{{{hi}}}:{{{lo}}}")
        }
    }
}

// ------------------------------------------------------------------------ 纯汇总

/// 各目录读回的记录 → 去重后的提交清单。
/// 跨目录合并必须先去重：同一个仓库挂在两个目录行下（或目录互为父子）时，
/// 一次提交会在两份 log 里各出现一次，不去重文件数直接翻倍。
pub fn collect(
    all: &[(usize, Vec<LogEntry>)],
    span: Option<(NaiveDate, NaiveDate)>,
) -> (Vec<Commit>, usize) {
    let mut index_of: HashMap<String, usize> = HashMap::new();
    let mut commits: Vec<Commit> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut skipped = 0usize;
    for (dir, entries) in all {
        for entry in entries {
            // date 已由 format_svn_date 转成本地时间 "%Y-%m-%d %H:%M:%S"，
            // 但解析失败时它会原样返回甚至给空串，所以用 get 而不是切片
            let Some(day) = entry.date.get(..10).and_then(parse_date) else {
                skipped += 1;
                continue;
            };
            if span.is_some_and(|(from, to)| day < from || day > to) {
                continue;
            }
            let index = match index_of.get(&entry.revision) {
                Some(index) => *index,
                None => {
                    commits.push(Commit {
                        rev: entry.revision.clone(),
                        dir: *dir,
                        day,
                        time: entry.date.get(11..).unwrap_or("").to_owned(),
                        paths: Vec::new(),
                    });
                    index_of.insert(entry.revision.clone(), commits.len() - 1);
                    commits.len() - 1
                }
            };
            for path in &entry.paths {
                if seen.insert((entry.revision.clone(), path.path.clone())) {
                    commits[index].paths.push(path.path.clone());
                }
            }
        }
    }
    commits.sort_by(|a, b| {
        (a.day, a.time.as_str(), a.rev.as_str()).cmp(&(b.day, b.time.as_str(), b.rev.as_str()))
    });
    (commits, skipped)
}

pub fn totals(commits: &[Commit]) -> Totals {
    Totals {
        revs: commits.len(),
        files: commits.iter().map(|c| c.paths.len()).sum(),
        distinct_files: commits
            .iter()
            .flat_map(|c| c.paths.iter())
            .collect::<HashSet<&String>>()
            .len(),
        active_days: commits.iter().map(|c| c.day).collect::<HashSet<_>>().len(),
        dirs: commits.iter().map(|c| c.dir).collect::<HashSet<_>>().len(),
    }
}

/// 把提交摊到 [from, to] 的连续格子上：一格提交都没有也要留着一个 0，
/// 否则折线会把空档连成一条直线、直方图的柱子也会挤在一起看不出疏密。
pub fn bucketize(commits: &[Commit], from: NaiveDate, to: NaiveDate) -> (Vec<Bucket>, Granularity) {
    let gran = Granularity::pick(to.signed_duration_since(from).num_days() + 1);
    let mut map: HashMap<String, (usize, usize)> = HashMap::new();
    for commit in commits {
        let key = match gran {
            // 只有单天区间才会用小时格，所以 key 就是 "HH"
            Granularity::Hour => commit.time.get(..2).unwrap_or("00").to_owned(),
            Granularity::Day => commit.day.format("%Y-%m-%d").to_string(),
            Granularity::Month => commit.day.format("%Y-%m").to_string(),
        };
        let slot = map.entry(key).or_default();
        slot.0 += commit.paths.len();
        slot.1 += 1;
    }
    let one_year = from.year() == to.year();
    let mut buckets = Vec::new();
    match gran {
        Granularity::Hour => {
            for hour in 0..24 {
                let key = format!("{hour:02}");
                let (files, revs) = map.get(&key).copied().unwrap_or((0, 0));
                buckets.push(Bucket {
                    key,
                    label: format!("{hour}时"),
                    files,
                    revs,
                });
            }
        }
        Granularity::Day => {
            let mut day = from;
            while day <= to {
                let key = day.format("%Y-%m-%d").to_string();
                let (files, revs) = map.get(&key).copied().unwrap_or((0, 0));
                buckets.push(Bucket {
                    key,
                    label: format!("{:02}-{:02}", day.month(), day.day()),
                    files,
                    revs,
                });
                day += TimeDelta::days(1);
            }
        }
        Granularity::Month => {
            let (mut year, mut month) = (from.year(), from.month());
            let (last_year, last_month) = (to.year(), to.month());
            loop {
                let key = format!("{year:04}-{month:02}");
                let (files, revs) = map.get(&key).copied().unwrap_or((0, 0));
                buckets.push(Bucket {
                    label: if one_year {
                        format!("{month}月")
                    } else {
                        format!("{year:04}-{month:02}")
                    },
                    key,
                    files,
                    revs,
                });
                if year == last_year && month == last_month {
                    break;
                }
                if month == 12 {
                    year += 1;
                    month = 1;
                } else {
                    month += 1;
                }
            }
        }
    }
    (buckets, gran)
}

/// 某年某月的首日与末日（含两端）。
pub fn month_bounds(year: i32, month: u32) -> (NaiveDate, NaiveDate) {
    let first = NaiveDate::from_ymd_opt(year, month, 1).expect("1 号总是合法日期");
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let next = NaiveDate::from_ymd_opt(next_year, next_month, 1).expect("下月 1 号总是合法日期");
    (first, next - TimeDelta::days(1))
}

/// 月历格子：从该月 1 号所在那一周的周一起，固定铺满 6 周（42 格），不属于本月的给 `None`。
/// 固定 6 周是为了跨月翻看不管哪个月都不会上下跳动。
pub fn calendar_days(year: i32, month: u32) -> Vec<Option<NaiveDate>> {
    let (first, _) = month_bounds(year, month);
    let start = first - TimeDelta::days(row_of(first) as i64);
    (0..42)
        .map(|offset| {
            let day = start + TimeDelta::days(offset);
            (day.year() == year && day.month() == month).then_some(day)
        })
        .collect()
}

/// 往前 / 往后翻一个月（`month` 传某月 1 号）。
fn shift_month(month: NaiveDate, delta: i32) -> NaiveDate {
    let index = month.year() * 12 + month.month() as i32 - 1 + delta;
    let year = index.div_euclid(12);
    let month = (index.rem_euclid(12) + 1) as u32;
    month_bounds(year, month).0
}

const WEEKDAY_NAMES: [&str; 7] = ["一", "二", "三", "四", "五", "六", "日"];

/// 月历里允许点哪些天，用来落实「起始不能晚于结束」：起始框的日历把上限设成已填的结束日，
/// 结束框反之。两端都可以不限。
#[derive(Clone, Copy, Default)]
pub struct DayLimits {
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
}

impl DayLimits {
    fn allows(self, day: NaiveDate) -> bool {
        self.from.is_none_or(|from| day >= from) && self.to.is_none_or(|to| day <= to)
    }
}

/// 两个框都填了合法日期却填反了：立刻给一句行内提示，不用等点「查询」才发现。
pub fn reversed_hint(from: &str, to: &str) -> Option<String> {
    let (Some(from), Some(to)) = (parse_date(from), parse_date(to)) else {
        return None;
    };
    (to < from).then(|| format!("起始日期 {from} 晚于结束日期 {to}，两者要对调"))
}

/// 日期输入框 + 月历选择器：手输仍然有效（`YYYY-MM-DD`，不补零也认），
/// 点「日历」弹一个月历挑一天写回。选完不自动查询，仍由「查询」按钮决定何时发请求。
pub fn date_input(
    ui: &mut Ui,
    id: Id,
    text: &mut String,
    hint: &str,
    today: NaiveDate,
    limits: DayLimits,
) {
    ui.add_sized(
        Vec2::new(96.0, 22.0),
        TextEdit::singleline(text).hint_text(hint),
    );
    let button = ui
        .button("日历")
        .on_hover_text("从月历里点一天，不用手打；输入框照样可以直接改");
    // 刚点开时以框里那天为准重置翻月游标，否则 ←/→ 会被输入框的值盖回去
    if button.clicked() {
        let shown = parse_date(text).unwrap_or(today);
        set_month_cursor(ui, id, month_bounds(shown.year(), shown.month()).0);
    }
    let chosen = egui::Popup::menu(&button)
        .id(id.with("popup"))
        // 不写死宽度：月历 7 列比任何固定值都宽，写小了右侧的「→」会被裁掉、点不动
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| month_calendar(ui, id, text.as_str(), today, limits));
    if let Some(Some(day)) = chosen.map(|shown| shown.inner) {
        *text = day.format("%Y-%m-%d").to_string();
        // 点外面才会关，所以选完这一天要主动收起来，不然弹层一直挂着
        egui::Popup::close_all(ui.ctx());
    }
}

/// 月历本体：返回被点中的那一天。
fn month_calendar(
    ui: &mut Ui,
    id: Id,
    current: &str,
    today: NaiveDate,
    limits: DayLimits,
) -> Option<NaiveDate> {
    let shown = month_cursor(ui, id, today);
    let (year, month) = (shown.year(), shown.month());
    let selected = parse_date(current);
    let mut chosen = None;
    // ← / 月份 / → 一律从左到右排：`Popup::menu` 是 top_down_justified 布局，弹层宽度未定之前
    // 靠右对齐顶到边缘的「→」会被裁到区域外，画不出来也点不到
    ui.horizontal(|ui| {
        let prev = ui
            .add_sized(Vec2::new(28.0, 24.0), egui::Button::new("←"))
            .on_hover_text("上一月");
        ui.add_sized(
            Vec2::new(150.0, 24.0),
            Label::new(RichText::new(format!("{year}年{month}月")).strong().size(13.0))
                .halign(Align::Center),
        );
        let next = ui
            .add_sized(Vec2::new(28.0, 24.0), egui::Button::new("→"))
            .on_hover_text("下一月");
        if prev.clicked() {
            set_month_cursor(ui, id, shift_month(shown, -1));
        }
        if next.clicked() {
            set_month_cursor(ui, id, shift_month(shown, 1));
        }
    });
    Grid::new(id.with("grid"))
        .num_columns(7)
        .min_col_width(30.0)
        .show(ui, |ui| {
            for name in WEEKDAY_NAMES {
                ui.add_sized(Vec2::new(30.0, 16.0), Label::new(RichText::new(name).weak().size(11.0)));
            }
            ui.end_row();
            for (index, day) in calendar_days(year, month).into_iter().enumerate() {
                if let Some(day) = day {
                    let allowed = limits.allows(day);
                    let mut label = RichText::new(format!("{}", day.day())).size(12.0);
                    if !allowed {
                        label = label.weak();
                    }
                    let mut cell = egui::Button::new(label).min_size(Vec2::new(30.0, 22.0));
                    if selected == Some(day) {
                        cell = cell.fill(ink(ui, Color32::from_rgb(90, 160, 240)));
                    } else if day == today {
                        // 今天只描边，不跟「已选」抢颜色
                        cell =
                            cell.stroke(Stroke::new(1.5, ink(ui, Color32::from_rgb(90, 160, 240))));
                    }
                    let hit = ui.add(cell);
                    if hit.clicked() && allowed {
                        chosen = Some(day);
                    } else if !allowed {
                        hit.on_hover_text("受另一个日期框的限制，这天不能选");
                    }
                } else {
                    // 月初前 / 月末后的空位：占一格，否则整月会左右错位
                    ui.add_sized(Vec2::new(30.0, 22.0), Label::new(" "));
                }
                // Grid 不会按 num_columns 自动换行（那参数只管条纹与对齐），一周结束要自己收尾
                if index % 7 == 6 {
                    ui.end_row();
                }
            }
        });
    chosen
}

/// 正在看哪一个月：存在 egui 内存里按控件分开，两个日期框各翻各的月。
fn month_cursor(ui: &Ui, id: Id, today: NaiveDate) -> NaiveDate {
    ui.data(|data| data.get_temp::<NaiveDate>(id.with("month")))
        .unwrap_or_else(|| month_bounds(today.year(), today.month()).0)
}

fn set_month_cursor(ui: &Ui, id: Id, month: NaiveDate) {
    ui.data_mut(|data| data.insert_temp(id.with("month"), month));
}

/// 点中图上的一格 → 这一格覆盖的日期区间，用来跳去「提交记录」页按天读服务器。
/// 小时格只出现在「当天」区间里，那一天就是页面区间的两端，`key` 里的 "HH" 不再拆日期。
pub fn bucket_span(
    gran: Granularity,
    key: &str,
    page_span: Option<(NaiveDate, NaiveDate)>,
) -> Option<(NaiveDate, NaiveDate)> {
    match gran {
        Granularity::Hour => page_span,
        Granularity::Day => parse_date(key).map(|day| (day, day)),
        Granularity::Month => {
            let (year, month) = key.split_once('-')?;
            let (year, month) = (year.parse::<i32>().ok()?, month.parse::<u32>().ok()?);
            Some(month_bounds(year, month))
        }
    }
}

/// 火力图：默认「列 = 周、行 = 周一到周日」的日历格，跨度太大时换成「列 = 年、行 = 月」。
/// 空格子也要占一格（带 title），不然鼠标停在没有提交的那天什么提示都没有。
pub fn build_heat(commits: &[Commit], from: NaiveDate, to: NaiveDate) -> Heat {
    let days = to.signed_duration_since(from).num_days() + 1;
    if days <= HEATMAP_DAY_LIMIT {
        day_heat(commits, from, to)
    } else {
        month_heat(commits, from, to)
    }
}

/// 周一为 0（GitHub 那种排法）；chrono 的 weekday() 从周日开始数，所以要挪一位
fn row_of(day: NaiveDate) -> usize {
    ((day.weekday().num_days_from_sunday() + 6) % 7) as usize
}

fn day_heat(commits: &[Commit], from: NaiveDate, to: NaiveDate) -> Heat {
    let mut files: HashMap<NaiveDate, (usize, usize)> = HashMap::new();
    for commit in commits {
        let slot = files.entry(commit.day).or_default();
        slot.0 += commit.paths.len();
        slot.1 += 1;
    }
    // 首列要整周，所以往前补齐到周一；末列同理补到周日
    let start = from - TimeDelta::days(row_of(from) as i64);
    let end = to + TimeDelta::days(6 - row_of(to) as i64);
    let cols = (end.signed_duration_since(start).num_days() / 7 + 1) as usize;
    let mut cells = Vec::new();
    let mut col_labels = Vec::new();
    let mut last_month = 0;
    for col in 0..cols {
        let head = start + TimeDelta::days(col as i64 * 7);
        if head.month() != last_month {
            last_month = head.month();
            col_labels.push((col, format!("{}月", last_month)));
        }
        for row in 0..7 {
            let day = head + TimeDelta::days(row);
            if day < from || day > to {
                continue;
            }
            let (f, r) = files.get(&day).copied().unwrap_or((0, 0));
            cells.push(HeatCell {
                col,
                row: row as usize,
                files: f,
                revs: r,
                title: day.format("%Y-%m-%d").to_string(),
                from: day,
                to: day,
            });
        }
    }
    let max = cells.iter().map(|cell| cell.files).max().unwrap_or(0);
    Heat {
        mode: HeatMode::Day,
        cols,
        rows: 7,
        cells,
        col_labels,
        row_labels: vec![
            (0, "一".to_owned()),
            (2, "三".to_owned()),
            (4, "五".to_owned()),
        ],
        max,
    }
}

fn month_heat(commits: &[Commit], from: NaiveDate, to: NaiveDate) -> Heat {
    let mut sums: HashMap<(i32, u32), (usize, usize)> = HashMap::new();
    for commit in commits {
        let key = (commit.day.year(), commit.day.month());
        let slot = sums.entry(key).or_default();
        slot.0 += commit.paths.len();
        slot.1 += 1;
    }
    let cols = (to.year() - from.year() + 1) as usize;
    let mut cells = Vec::new();
    for col in 0..cols as i32 {
        let year = from.year() + col;
        for month in 1..=12 {
            if (year, month) < (from.year(), from.month()) || (year, month) > (to.year(), to.month())
            {
                continue;
            }
            let (f, r) = sums.get(&(year, month)).copied().unwrap_or((0, 0));
            // 用统计区间裁过：点这一格跳过去的日期范围，要和它统计的范围一致
            let (first, last) = month_bounds(year, month);
            cells.push(HeatCell {
                col: col as usize,
                row: (month - 1) as usize,
                files: f,
                revs: r,
                title: format!("{year}年{month}月"),
                from: first.max(from),
                to: last.min(to),
            });
        }
    }
    let max = cells.iter().map(|cell| cell.files).max().unwrap_or(0);
    Heat {
        mode: HeatMode::Month,
        cols,
        rows: 12,
        cells,
        col_labels: (0..cols).map(|col| (col, format!("{}", from.year() + col as i32))).collect(),
        row_labels: (1..=12).map(|month| (month - 1, format!("{month}月"))).collect(),
        max,
    }
}

/// 把最大值收成 1/2/5 阶梯上的整数，y 轴四格刻度才不会写出 37.5 这种数
fn y_step(max: usize) -> usize {
    let target = max.max(1);
    let mut power = 1usize;
    for _ in 0..12 {
        for base in [1, 2, 5] {
            let step = base * power;
            if step * 4 >= target {
                return step;
            }
        }
        power *= 10;
    }
    target.div_ceil(4).max(1)
}

/// 主题相关的取色：强调色一律走 `ink`（浅色主题压暗，否则白底上看不清）
struct Palette {
    grid: Color32,
    axis: Color32,
    text: Color32,
    files: Color32,
    revs: Color32,
}

fn palette(ui: &Ui) -> Palette {
    Palette {
        grid: if ui.visuals().dark_mode {
            Color32::from_gray(58)
        } else {
            Color32::from_gray(222)
        },
        axis: if ui.visuals().dark_mode {
            Color32::from_gray(105)
        } else {
            Color32::from_gray(165)
        },
        text: if ui.visuals().dark_mode {
            Color32::from_gray(165)
        } else {
            Color32::from_gray(95)
        },
        files: ink(ui, Color32::from_rgb(90, 160, 240)),
        revs: ink(ui, Color32::from_rgb(240, 175, 70)),
    }
}

fn mix(from: Color32, to: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let a = from.to_array();
    let b = to.to_array();
    let at = |i: usize| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t).round() as u8;
    Color32::from_rgb(at(0), at(1), at(2))
}

/// 颜色强度用开方压一下：文件数长尾很重，线性映射会让绝大多数格子都是浅色
fn heat_color(ui: &Ui, value: usize, max: usize) -> Color32 {
    let (base, peak) = if ui.visuals().dark_mode {
        (Color32::from_rgb(38, 52, 66), Color32::from_rgb(96, 190, 255))
    } else {
        (Color32::from_rgb(228, 236, 245), Color32::from_rgb(28, 110, 200))
    };
    if value == 0 {
        return if ui.visuals().dark_mode {
            Color32::from_gray(46)
        } else {
            Color32::from_rgb(235, 235, 235)
        };
    }
    let t = (value as f32 / max.max(1) as f32).sqrt();
    ink(ui, mix(base, peak, t))
}

fn legend(ui: &mut Ui, items: &[(&str, Color32)]) {
    let font = FontId::proportional(11.5);
    let (response, painter) =
        ui.allocate_painter(Vec2::new(ui.available_width().max(120.0), 14.0), Sense::hover());
    let rect = response.rect;
    let mut x = rect.left();
    for (text, color) in items {
        painter.rect_filled(
            Rect::from_min_size(Pos2::new(x, rect.center().y - 4.5), Vec2::new(9.0, 9.0)),
            2.0,
            *color,
        );
        // Painter::text 返回真正落好的矩形：中文字宽没法按字数估，只能拿它推进
        let laid = painter.text(
            Pos2::new(x + 13.0, rect.center().y),
            Align2::LEFT_CENTER,
            *text,
            font.clone(),
            palette(ui).text,
        );
        x = laid.max.x + 18.0;
    }
}

/// 汇总卡里的一格：上面小标签、下面大数字，整块挂口径说明
fn metric(ui: &mut Ui, title: &str, value: usize, hint: &str) {
    let response = ui
        .vertical(|ui| {
            ui.label(RichText::new(title).size(11.5).weak());
            ui.label(RichText::new(value.to_string()).size(19.0).strong());
        })
        .response;
    response.on_hover_text(hint);
}

/// 火力图图例：一句「一格是什么」+ 五档色阶 + 少/多
fn heat_legend(ui: &mut Ui, heat: &Heat) {
    let pal = palette(ui);
    let font = FontId::proportional(11.5);
    let (response, painter) =
        ui.allocate_painter(Vec2::new(ui.available_width().max(240.0), 16.0), Sense::hover());
    let rect = response.rect;
    let y = rect.center().y;
    let note = if heat.mode == HeatMode::Day {
        "一格 = 一天，颜色 = 当天文件条目数，点一格跳到那天的提交记录"
    } else {
        "跨度太大，已按「年 × 月」汇总：一格 = 一个月，点一格跳到那个月"
    };
    let laid = painter.text(
        Pos2::new(rect.left(), y),
        Align2::LEFT_CENTER,
        note,
        font.clone(),
        pal.text,
    );
    let mut x = laid.max.x + 18.0;
    painter.text(
        Pos2::new(x, y),
        Align2::LEFT_CENTER,
        "少",
        font.clone(),
        pal.text,
    );
    x += 16.0;
    for step in 0..5 {
        let value = if step == 0 {
            0
        } else {
            heat.max * step / 4
        };
        painter.rect_filled(
            Rect::from_min_size(Pos2::new(x, y - 5.0), Vec2::new(11.0, 11.0)),
            2.0,
            heat_color(ui, value, heat.max),
        );
        x += 14.0;
    }
    painter.text(
        Pos2::new(x, y),
        Align2::LEFT_CENTER,
        "多",
        font.clone(),
        pal.text,
    );
}

/// 坐标系：四条水平网格 + 左轴数字 + 一根 y 轴线
fn draw_grid(painter: &egui::Painter, plot: Rect, step: usize, pal: &Palette) {
    let font = FontId::proportional(11.0);
    for line in 0..=4 {
        let value = step * line;
        let y = plot.bottom() - plot.height() * line as f32 / 4.0;
        painter.hline(
            Rangef::new(plot.left(), plot.right()),
            y,
            Stroke::new(1.0, pal.grid),
        );
        painter.text(
            Pos2::new(plot.left() - 7.0, y),
            Align2::RIGHT_CENTER,
            value.to_string(),
            font.clone(),
            pal.text,
        );
    }
    painter.line_segment(
        [
            Pos2::new(plot.left(), plot.top()),
            Pos2::new(plot.left(), plot.bottom()),
        ],
        Stroke::new(1.0, pal.axis),
    );
}

/// 折线图与直方图共用一套坐标系与位置反算，只有中间那几笔不一样。
/// 返回被点中那一格覆盖的日期区间（供调用方跳去「提交记录」页）。
fn trend_chart(
    ui: &mut Ui,
    buckets: &[Bucket],
    kind: ChartKind,
    gran: Granularity,
    page_span: Option<(NaiveDate, NaiveDate)>,
) -> Option<(NaiveDate, NaiveDate)> {
    let pal = palette(ui);
    let font = FontId::proportional(11.0);
    let n = buckets.len();
    let max_files = buckets.iter().map(|b| b.files).max().unwrap_or(0);
    let max_revs = buckets.iter().map(|b| b.revs).max().unwrap_or(0);
    let step_files = y_step(max_files);
    let top_files = (step_files * 4).max(1);
    let top_revs = (y_step(max_revs) * 4).max(1);

    let width = ui.available_width().max(320.0);
    // click 也带 hover：提示照旧出，多了「点一格跳那天」的能力
    let (response, painter) = ui.allocate_painter(Vec2::new(width, 268.0), Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let rect = response.rect;
    let plot = Rect::from_min_max(
        Pos2::new(rect.left() + 44.0, rect.top() + 10.0),
        Pos2::new(rect.right() - 10.0, rect.bottom() - 28.0),
    );
    draw_grid(&painter, plot, step_files, &pal);

    let slot = (plot.width() / n as f32).max(1.0);
    let center = |index: usize| Pos2::new(plot.left() + slot * (index as f32 + 0.5), 0.0);
    let label_stride = (n as f32 / (plot.width() / 66.0).max(1.0).floor()).ceil().max(1.0) as usize;

    if kind == ChartKind::Bar {
        let bar_w = (slot * 0.34).max(1.5);
        for (index, bucket) in buckets.iter().enumerate() {
            let x = center(index).x;
            let h_files = plot.height() * bucket.files as f32 / top_files as f32;
            let h_revs = plot.height() * bucket.revs as f32 / top_revs as f32;
            for (offset, height, color) in [
                (-bar_w - 0.5, h_files, pal.files),
                (0.5, h_revs, pal.revs),
            ] {
                if height <= 0.4 {
                    continue;
                }
                painter.rect_filled(
                    Rect::from_min_max(
                        Pos2::new(x + offset, plot.bottom() - height),
                        Pos2::new(x + offset + bar_w, plot.bottom()),
                    ),
                    1.0,
                    color,
                );
            }
        }
    } else {
        let points = |pick: &dyn Fn(&Bucket) -> usize, top: usize| -> Vec<Pos2> {
            buckets
                .iter()
                .enumerate()
                .map(|(index, bucket)| {
                    let y = plot.bottom() - plot.height() * pick(bucket) as f32 / top as f32;
                    Pos2::new(center(index).x, y)
                })
                .collect()
        };
        let file_points = points(&|b| b.files, top_files);
        painter.line(points(&|b| b.revs, top_revs), Stroke::new(1.4, pal.revs));
        painter.line(file_points, Stroke::new(1.8, pal.files));
        if n <= 62 {
            for point in points(&|b| b.files, top_files) {
                painter.circle_filled(point, 2.4, pal.files);
            }
        }
    }

    for (index, bucket) in buckets.iter().enumerate() {
        if index % label_stride != 0 {
            continue;
        }
        painter.text(
            Pos2::new(center(index).x, plot.bottom() + 9.0),
            Align2::CENTER_TOP,
            &bucket.label,
            font.clone(),
            pal.text,
        );
    }

    if let Some(index) = response.hover_pos().and_then(|pos| index_at(pos, plot, slot, n)) {
        let bucket = &buckets[index];
        painter.vline(
            center(index).x,
            Rangef::new(plot.top(), plot.bottom()),
            Stroke::new(1.0, pal.axis),
        );
        chart_tooltip(
            &response,
            bucket.label.clone(),
            &[
                format!("文件条目数：{}", bucket.files),
                format!("提交次数：{}", bucket.revs),
                "点击可跳到这一时段的提交记录".to_owned(),
            ],
        );
    }
    response
        .clicked()
        .then(|| response.interact_pointer_pos())
        .flatten()
        .and_then(|pos| index_at(pos, plot, slot, n))
        .and_then(|index| bucket_span(gran, &buckets[index].key, page_span))
}

/// 指针位置 → 命中的第几格。悬停提示和点击下钻共用，两边算出来的格子才不会不一致。
fn index_at(pos: Pos2, plot: Rect, slot: f32, n: usize) -> Option<usize> {
    if pos.x < plot.left() || pos.x > plot.right() || pos.y < plot.top() || pos.y > plot.bottom()
    {
        return None;
    }
    let index = ((pos.x - plot.left()) / slot).floor();
    if index < 0.0 {
        return None;
    }
    let index = index as usize;
    Some(index.min(n.saturating_sub(1)))
}

/// 图上悬浮提示：标签和数字必须待在同一行。提示框默认按 `spacing.tooltip_width` 排版，
/// 中文标签会被折到第二行，所以每行都显式改成 Extend（只extend不换行）。
fn chart_tooltip(response: &Response, title: String, rows: &[String]) {
    egui::Tooltip::for_widget(response)
        .at_pointer()
        .show(|ui| {
            ui.add(
                egui::Label::new(RichText::new(title).strong().size(12.0))
                    .wrap_mode(TextWrapMode::Extend),
            );
            for row in rows {
                ui.add(egui::Label::new(row.clone()).wrap_mode(TextWrapMode::Extend));
            }
        });
}

/// 火力图：一格一天（跨度太大时一格一月）。返回被点中那一格覆盖的日期区间。
fn heatmap_chart(ui: &mut Ui, heat: &Heat) -> Option<(NaiveDate, NaiveDate)> {
    let pal = palette(ui);
    let font = FontId::proportional(11.0);
    let day = heat.mode == HeatMode::Day;
    let cell = if day { 13.0 } else { 26.0 };
    let gap = 3.0;
    let pitch = cell + gap;
    let left = if day { 20.0 } else { 34.0 };
    let top = 16.0;
    let width = left + heat.cols as f32 * pitch + 6.0;
    let height = top + heat.rows as f32 * pitch + 4.0;
    let mut clicked = None;

    ScrollArea::horizontal()
        .id_salt("stats_heatmap")
        .show(ui, |ui| {
            let (response, painter) = ui.allocate_painter(
                Vec2::new(width.max(ui.available_width()), height),
                Sense::click(),
            );
            let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
            let rect = response.rect;
            let origin = Pos2::new(rect.left() + left, rect.top() + top);
            for (row, label) in &heat.row_labels {
                painter.text(
                    Pos2::new(rect.left() + left - 5.0, origin.y + pitch * *row as f32 + cell / 2.0),
                    Align2::RIGHT_CENTER,
                    label,
                    font.clone(),
                    pal.text,
                );
            }
            for (col, label) in &heat.col_labels {
                painter.text(
                    Pos2::new(origin.x + pitch * *col as f32, rect.top() + 2.0),
                    Align2::LEFT_TOP,
                    label,
                    font.clone(),
                    pal.text,
                );
            }
            for item in &heat.cells {
                let min = Pos2::new(
                    origin.x + pitch * item.col as f32,
                    origin.y + pitch * item.row as f32,
                );
                let box_rect = Rect::from_min_size(min, Vec2::new(cell, cell));
                painter.rect_filled(box_rect, 2.0, heat_color(ui, item.files, heat.max));
            }
            if let Some(item) = response.hover_pos().and_then(|pos| cell_at(pos, origin, pitch, cell, heat)) {
                let min = Pos2::new(
                    origin.x + pitch * item.col as f32,
                    origin.y + pitch * item.row as f32,
                );
                let box_rect = Rect::from_min_size(min, Vec2::new(cell, cell)).expand(1.0);
                painter.rect_stroke(
                    box_rect,
                    2.0,
                    Stroke::new(1.2, pal.text),
                    StrokeKind::Inside,
                );
                chart_tooltip(
                    &response,
                    item.title.clone(),
                    &[
                        if item.files == 0 {
                            "没有提交".to_owned()
                        } else {
                            format!("文件条目数：{}", item.files)
                        },
                        format!("提交次数：{}", item.revs),
                        "点击可跳到这几天的提交记录".to_owned(),
                    ],
                );
            }
            if response.clicked() {
                if let Some(pos) = response.interact_pointer_pos() {
                    if let Some(item) = cell_at(pos, origin, pitch, cell, heat) {
                        clicked = Some((item.from, item.to));
                    }
                }
            }
        });
    clicked
}

/// 指针位置 → 命中的格子。悬停高亮和点击下钻共用，两边算出来的格子才不会不一样。
fn cell_at<'a>(
    pos: Pos2,
    origin: Pos2,
    pitch: f32,
    cell: f32,
    heat: &'a Heat,
) -> Option<&'a HeatCell> {
    let col = ((pos.x - origin.x) / pitch).floor();
    let row = ((pos.y - origin.y) / pitch).floor();
    if col < 0.0 || row < 0.0 || col >= heat.cols as f32 || row >= heat.rows as f32 {
        return None;
    }
    // 指针落在格子的间隙里就不算命中，避免整列都亮起来
    let in_x = pos.x - origin.x - col * pitch;
    let in_y = pos.y - origin.y - row * pitch;
    if in_x > cell || in_y > cell {
        return None;
    }
    let (col, row) = (col as usize, row as usize);
    heat.cells.iter().find(|item| item.col == col && item.row == row)
}

// ------------------------------------------------------------------------ 页面

impl SvnApp {
    pub fn open_stats(&mut self) {
        if self.svn_or_none().is_none() {
            return;
        }
        if self.cfg.dirs.is_empty() {
            self.hint("目录列表是空的，先添加一个 SVN 工作副本目录");
            return;
        }
        if self.svn_user.trim().is_empty() {
            // 统计口径就是「本人提交」，没有登录人就没法查：补一次探测，探测到后自动开查
            self.spawn_auth_user();
            self.hint("正在探测本机 svn 登录人，探测到后会自动开始统计");
        }
        self.stats = Some(StatsPage::new());
        self.page = Page::Stats;
        self.spawn_stats();
    }

    /// 探测到登录人时补跑统计（open_stats 时还没探到就会这样）
    pub fn stats_after_user_probe(&mut self) {
        if self.page == Page::Stats && self.stats.as_ref().is_some_and(|page| page.expected.is_empty()) {
            self.spawn_stats();
        }
    }

    pub fn spawn_stats(&mut self) {
        let Some(svn) = self.svn_or_none() else { return };
        let Some(page) = self.stats.clone() else { return };
        let today = Local::now().date_naive();
        let span = match page.resolve(today) {
            Ok(span) => span,
            Err(message) => {
                self.hint(message.clone());
                if let Some(current) = self.stats.as_mut() {
                    current.error = message;
                    current.expected = Vec::new();
                    current.done = Vec::new();
                    current.failed = Vec::new();
                    current.view = StatsView::default();
                }
                return;
            }
        };
        let author = self.svn_user.trim().to_owned();
        // 登录人为空时不能查：不带 --search 就等于把所有人的提交都算成自己
        if author.is_empty() {
            let Some(current) = self.stats.as_mut() else {
                return;
            };
            current.expected = Vec::new();
            current.done = Vec::new();
            current.failed = Vec::new();
            current.view = StatsView::default();
            current.revspec = String::new();
            return;
        }
        let targets: Vec<usize> = if page.dir == ALL_DIRS {
            (0..self.cfg.dirs.len()).collect()
        } else if page.dir < self.cfg.dirs.len() {
            vec![page.dir]
        } else {
            Vec::new()
        };
        let spec = revspec(span);
        let epoch = page.epoch + 1;
        let mut page = page;
        page.epoch = epoch;
        page.span = span;
        page.revspec = spec.clone();
        page.author = author.clone();
        page.error.clear();
        page.expected = targets.clone();
        page.done.clear();
        page.failed.clear();
        page.view = StatsView {
            expected: targets.len(),
            ..StatsView::default()
        };
        // 先回写再生效：spawn 的闭包要读 self.cfg / self.svn_user，
        // 而 apply_stats 也要按 self.stats 里的 epoch 核对结果
        self.stats = Some(page);
        for index in &targets {
            let index = *index;
            let Some(path) = self.dir_path(index) else { continue };
            let label = self.dir_label(index);
            let svn = svn.clone();
            let author = author.clone();
            let spec = spec.clone();
            self.pool.spawn(
                Kind::Stats,
                ALL_DIRS,
                format!("{label} 提交统计"),
                move |sink| {
                    // @HEAD 显式指定 peg：不带的话工作副本的隐式 peg 是 BASE，
                    // 本地落后时按日期找版本会取不到最新那些提交（svn.rs 里读记录同一个坑）
                    let target = format!("{}@HEAD", path.to_string_lossy());
                    sink.line(format!(
                        "$ svn log -v -r {spec} --search {author} --xml \"{target}\""
                    ));
                    let (entries, run) = svn.log_in(&target, &spec, 0, &author);
                    sink.line(format!("→ {} 条本人提交", entries.len()));
                    Data::Stats {
                        dir: index,
                        epoch,
                        entries,
                        ok: run.ok,
                        message: if run.ok { String::new() } else { run.summary() },
                    }
                },
            );
        }
        if targets.is_empty() {
            self.hint("没有可统计的目录");
        } else {
            self.hint(format!("正在统计 {} 个目录的本人提交 …", targets.len()));
        }
    }

    /// 一个目录的统计结果回来了。epoch 对不上说明用户中途换了条件，直接丢。
    pub fn apply_stats(
        &mut self,
        dir: usize,
        epoch: u64,
        entries: Vec<LogEntry>,
        ok: bool,
        message: String,
    ) {
        if let Some(page) = self.stats.as_mut() {
            if page.epoch != epoch {
                return;
            }
            if ok {
                page.done.push((dir, entries));
            } else {
                page.failed.push((dir, message));
            }
            let today = Local::now().date_naive();
            recalc(page, today);
        }
        let (received, expected) = self
            .stats
            .as_ref()
            .map(|page| (page.view.received, page.view.expected))
            .unwrap_or((0, 0));
        if expected > 0 && received >= expected {
            self.hint(format!(
                "提交统计完成：{received} 个目录，共 {} 条文件记录",
                self.stats
                    .as_ref()
                    .map(|page| page.view.totals.files)
                    .unwrap_or(0)
            ));
        }
    }

    pub fn stats_page(&mut self, ui: &mut Ui) {
        let Some(mut page) = self.stats.clone() else {
            self.back_to_main();
            return;
        };
        let loading = self.pool.has(Kind::Stats, ALL_DIRS);
        let dirs = self.cfg.dirs.len();
        let user = self.svn_user.clone();
        let mut relaunch = false;
        let mut quit = false;
        // 点中一格 → 要下钻去的日期区间（在 ScrollArea 闭包里赋值，闭包外再跳转）
        let mut drill: Option<(NaiveDate, NaiveDate)> = None;

        ui.horizontal(|ui| {
            if ui.button("← 返回目录").clicked() {
                quit = true;
            }
            ui.label(RichText::new("提交统计").size(17.0).strong());
            ui.label(
                RichText::new(if user.trim().is_empty() {
                    "本机 svn 登录人待探测".to_owned()
                } else {
                    format!("本人：{user}")
                })
                .size(12.5)
                .weak(),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if loading {
                    ui.spinner();
                }
                if ui
                    .button("刷新")
                    .on_hover_text("按当前条件重新读一遍服务器记录")
                    .clicked()
                {
                    relaunch = true;
                }
                ui.label(
                    RichText::new(format!("已回 {} / {} 个目录", page.view.received, page.view.expected))
                        .size(11.5)
                        .weak(),
                );
            });
        });
        if quit {
            self.back_to_main();
            self.stats = None;
            return;
        }
        ui.separator();

        // 没读回来的目录名要在借出 self 之前先算好，下面的 UI 闭包里只读本地 page
        let failures: Vec<String> = page
            .failed
            .iter()
            .map(|(index, message)| format!("{}：{message}", self.dir_label(*index)))
            .collect();

        ScrollArea::vertical()
            .id_salt("stats_page")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("区间").strong());
                    for preset in RangePreset::ALL {
                        if ui
                            .selectable_label(page.range == preset, preset.label())
                            .on_hover_text(preset.hover())
                            .clicked()
                        {
                            let changed = page.range != preset;
                            page.range = preset;
                            // 自定义要等日期填完点「查询」，不然每敲一个字就发一次请求
                            relaunch |= changed && preset != RangePreset::Custom;
                        }
                    }
                    if page.range == RangePreset::Custom {
                        let today = Local::now().date_naive();
                        let from = parse_date(&page.custom_from);
                        let to = parse_date(&page.custom_to);
                        date_input(
                            ui,
                            Id::new("stats_custom_from"),
                            &mut page.custom_from,
                            "2026-09-01",
                            today,
                            DayLimits { from: None, to },
                        );
                        ui.label("至");
                        date_input(
                            ui,
                            Id::new("stats_custom_to"),
                            &mut page.custom_to,
                            "留空=今天",
                            today,
                            DayLimits { from, to: None },
                        );
                        if ui.button("查询").clicked() {
                            relaunch = true;
                        }
                    }
                });
                if page.range == RangePreset::Custom {
                    if let Some(warn) = reversed_hint(&page.custom_from, &page.custom_to) {
                        ui.label(
                            RichText::new(warn)
                                .size(11.5)
                                .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                        );
                    }
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new("目录").strong());
                    let current = if page.dir == ALL_DIRS {
                        "全部目录（合并）".to_owned()
                    } else {
                        self.dir_label(page.dir)
                    };
                    egui::ComboBox::from_id_salt("stats_dir")
                        .selected_text(current)
                        .width(220.0)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_value(&mut page.dir, ALL_DIRS, "全部目录（合并）")
                                .changed()
                            {
                                relaunch = true;
                            }
                            for index in 0..dirs {
                                let label = self.dir_label(index);
                                if ui.selectable_value(&mut page.dir, index, label).changed() {
                                    relaunch = true;
                                }
                            }
                        });
                    ui.add_space(6.0);
                    ui.label(RichText::new("图").strong());
                    for kind in ChartKind::ALL {
                        if ui
                            .selectable_label(page.chart == kind, kind.label())
                            .on_hover_text(kind.hover())
                            .clicked()
                        {
                            // 换图只是换画法，数据已经在手里，不用重查
                            page.chart = kind;
                        }
                    }
                });
                ui.add_space(2.0);

                if !page.error.is_empty() {
                    note_card(ui, &page.error.clone(), true);
                }
                if user.trim().is_empty() {
                    note_card(
                        ui,
                        "统计口径是「本人提交」，还没探测到本机 svn 登录人就没法过滤。\n\
                         已在后台探测，探测到会自动出图；也可以点右上角「刷新」重试。",
                        false,
                    );
                }
                if !failures.is_empty() {
                    let text = format!(
                        "{} 个目录没读到记录：\n{}",
                        failures.len(),
                        failures.join("\n")
                    );
                    note_card(ui, &text, true);
                }

                let view = page.view.clone();
                let (fill, stroke) = card_colors(ui);
                Frame::new()
                    .inner_margin(8.0)
                    .corner_radius(6.0)
                    .fill(fill)
                    .stroke(Stroke::new(1.0, stroke))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            metric(
                                ui,
                                "文件条目数",
                                view.totals.files,
                                "各次提交涉及的 path 累加（含目录），同一文件提交三次算三条。\n\
                                 与「提交记录」页的「涉及文件（N 项）」同一口径。",
                            );
                            ui.separator();
                            metric(
                                ui,
                                "提交次数",
                                view.totals.revs,
                                "区间内的版本个数；跨目录合并时同一个版本只算一次。",
                            );
                            ui.separator();
                            metric(
                                ui,
                                "去重文件数",
                                view.totals.distinct_files,
                                "按仓库路径去重后，区间内被本人碰过的文件个数。",
                            );
                            ui.separator();
                            metric(
                                ui,
                                "活跃天数",
                                view.totals.active_days,
                                "有提交的自然日天数。",
                            );
                            ui.separator();
                            metric(
                                ui,
                                "覆盖目录数",
                                view.totals.dirs,
                                "这批数据里真正出过提交的目录个数（合并按仓库路径去重，\n\
                                 同一仓库挂在两个目录行下也只算一次文件）。",
                            );
                            if view.skipped > 0 {
                                ui.separator();
                                metric(
                                    ui,
                                    "时间异常",
                                    view.skipped,
                                    "svn 返回的时间字段解析不出日期，没有计入统计。",
                                );
                            }
                        });
                    });
                ui.add_space(6.0);

                if loading && view.received == 0 {
                    ui.label(RichText::new("正在读取服务器提交记录 …").weak().size(12.5));
                } else if view.buckets.is_empty() {
                    ui.label(
                        RichText::new("还没有可画的数据：等目录读回来，或换个区间再查。")
                            .weak()
                            .size(12.5),
                    );
                } else if view.totals.files == 0 && page.chart != ChartKind::Heatmap {
                    ui.label(
                        RichText::new(
                            "这个区间内没有本人的提交：可以换更宽的区间，或选「全部目录（合并）」。",
                        )
                        .weak()
                        .size(12.5),
                    );
                }

                // 一格都还没有时不进坐标系：按格子数除宽会得到 inf，位置反算还会去取不存在的第一格
                let clicked = if !view.buckets.is_empty() {
                    match page.chart {
                        ChartKind::Line | ChartKind::Bar => {
                            let pal = palette(ui);
                            let top = y_step(
                                view.buckets.iter().map(|bucket| bucket.files).max().unwrap_or(0),
                            ) * 4;
                            let note = format!(
                                "{}（{} 格，左轴上限 {}，橙色{}按自身峰值归一化）",
                                view.gran.label(),
                                view.buckets.len(),
                                top,
                                if page.chart == ChartKind::Bar { "柱子" } else { "折线" }
                            );
                            legend(
                                ui,
                                &[
                                    ("文件条目数", pal.files),
                                    ("提交次数", pal.revs),
                                    (note.as_str(), pal.axis),
                                ],
                            );
                            trend_chart(ui, &view.buckets, page.chart, view.gran, page.span)
                        }
                        ChartKind::Heatmap => {
                            heat_legend(ui, &view.heat);
                            heatmap_chart(ui, &view.heat)
                        }
                    }
                } else {
                    None
                };
                if clicked.is_some() {
                    drill = clicked;
                }

                if page.chart != ChartKind::Heatmap && !view.buckets.is_empty() {
                    let rows: Vec<&Bucket> = view
                        .buckets
                        .iter()
                        .filter(|bucket| bucket.files > 0)
                        .collect();
                    ui.add_space(2.0);
                    ui.collapsing(
                        format!("明细（{}，只列有提交的 {} 格）", view.gran.label(), rows.len()),
                        |ui| {
                            if rows.is_empty() {
                                ui.label(RichText::new("区间内没有提交").weak().size(12.0));
                            }
                            ScrollArea::vertical()
                                .id_salt("stats_detail")
                                .max_height(190.0)
                                .show(ui, |ui| {
                                    Grid::new("stats_detail_grid")
                                        .num_columns(3)
                                        .striped(true)
                                        .min_col_width(70.0)
                                        .show(ui, |ui| {
                                            for title in ["时段", "文件条目数", "提交次数"] {
                                                ui.label(RichText::new(title).strong().size(11.5));
                                            }
                                            ui.end_row();
                                            for row in rows {
                                                ui.label(row.label.clone());
                                                ui.label(row.files.to_string());
                                                ui.label(row.revs.to_string());
                                                ui.end_row();
                                            }
                                        });
                                });
                        },
                    );
                }

                if !page.revspec.is_empty() {
                    let days = match page.span {
                        Some((from, to)) if from == to => format!("{from}"),
                        Some((from, to)) => format!("{from} ~ {to}"),
                        None => "全部提交史".to_owned(),
                    };
                    let line = format!(
                        "统计区间 {days}    $ svn log -v -r {} --search {} --xml <目录>@HEAD",
                        page.revspec,
                        if page.author.is_empty() { "（无人）" } else { &page.author }
                    );
                    ui.label(RichText::new(line).size(11.0).monospace().weak())
                        .on_hover_text(
                            "命令里的 -r 比统计区间宽 1~2 天是有意的：`{日期}` 按本地还是 UTC 解释，\n\
                             不同 svn 版本不一致，两端各让一天再按统计区间裁回来，两种解释都不会漏。\n\
                             图上的数字只算上面那个区间内的提交。",
                        );
                }
            });

        // 先回写再重查：spawn_stats 读的是 self.stats（含刚改的条件），反过来会被这里的回写盖掉
        self.stats = Some(page);
        if relaunch {
            self.spawn_stats();
        }
        if let Some(span) = drill {
            self.drill_into(span);
        }
    }

    /// 图上点中一格 → 打开对应目录的「提交记录」页，并按那一格覆盖的日期读服务器。
    /// 合并视图一天可能好几个目录都有提交，跳不到多个，就挑当天文件条目数最多的那个
    /// （并列取目录序号小的），并在提示里说清楚挑了谁、当天还有谁。
    fn drill_into(&mut self, span: (NaiveDate, NaiveDate)) {
        let Some(page) = self.stats.clone() else {
            return;
        };
        let mut index = page.dir;
        let mut note = String::new();
        if index == ALL_DIRS {
            let (commits, _) = collect(&page.done, Some(span));
            let mut files: HashMap<usize, usize> = HashMap::new();
            for commit in &commits {
                *files.entry(commit.dir).or_default() += commit.paths.len();
            }
            let mut ranked: Vec<(usize, usize)> = files.into_iter().collect();
            ranked.sort_by_key(|(dir, _)| *dir);
            let mut best: Option<(usize, usize)> = None;
            for item in ranked {
                if best.is_none_or(|(_, count)| item.1 > count) {
                    best = Some(item);
                }
            }
            let Some((winner, _)) = best else {
                self.hint("这一格里没有本人的提交，没有可跳转的记录。");
                return;
            };
            index = winner;
            note = "当天多个目录都有提交，已跳到文件最多的那个。".to_owned();
        }
        let label = self.dir_label(index);
        let (from, to) = span;
        self.open_history_in(index, Some(span));
        let days = if from == to {
            format!("{from}")
        } else {
            format!("{from} ~ {to}")
        };
        self.hint(if note.is_empty() {
            format!("已打开「{label}」的提交记录：{days}")
        } else {
            format!("已打开「{label}」的提交记录：{days}（{note}）")
        });
    }
}

/// 结果落地或换条件后重算图形（事件驱动，不进每帧渲染路径）
fn recalc(page: &mut StatsPage, today: NaiveDate) {
    let (commits, skipped) = collect(&page.done, page.span);
    let (from, to) = match page.span {
        Some(span) => span,
        None => {
            let lo = commits.iter().map(|c| c.day).min().unwrap_or(today);
            let hi = commits.iter().map(|c| c.day).max().unwrap_or(today);
            (lo, hi)
        }
    };
    let (buckets, gran) = bucketize(&commits, from, to);
    let totals = totals(&commits);
    let heat = build_heat(&commits, from, to);
    page.view = StatsView {
        buckets,
        gran,
        totals,
        heat,
        received: page.done.len() + page.failed.len(),
        expected: page.expected.len(),
        skipped,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::svn::LogPath;

    fn date(text: &str) -> NaiveDate {
        parse_date(text).expect("测试里的日期要合法")
    }

    fn entry(revision: &str, when: &str, paths: &[&str]) -> LogEntry {
        LogEntry {
            revision: revision.to_owned(),
            author: "zhangsan".to_owned(),
            date: when.to_owned(),
            message: String::new(),
            paths: paths
                .iter()
                .map(|path| LogPath {
                    action: 'M',
                    kind: "file".to_owned(),
                    path: (*path).to_owned(),
                })
                .collect(),
        }
    }

    /// 同一个仓库挂在两个目录行下（或目录互为父子）时，一次提交会在两份 log 里各出现一次
    #[test]
    fn same_commit_read_from_two_dirs_counts_once() {
        let shared = entry("10", "2026-09-07 10:00:00", &["/a/x.java", "/a/y.java"]);
        let all = vec![(0, vec![shared.clone()]), (1, vec![shared])];
        let (commits, skipped) = collect(&all, None);
        assert_eq!(skipped, 0);
        assert_eq!(commits.len(), 1, "同一个版本只能算一次提交");
        let totals = totals(&commits);
        assert_eq!(totals.files, 2, "重复读到的 path 不能翻倍");
        assert_eq!(totals.distinct_files, 2);
    }

    /// 查询两端各放宽了两天，回来必须按用户要的区间裁掉，否则数字会比表面条件多
    #[test]
    fn widened_query_window_is_pruned_back_to_the_requested_span() {
        let all = vec![(
            0,
            vec![
                entry("8", "2026-08-30 18:00:00", &["/old"]),
                entry("9", "2026-09-01 00:30:00", &["/first"]),
                entry("10", "2026-09-07 23:30:00", &["/last"]),
                entry("11", "2026-09-09 09:00:00", &["/next"]),
            ],
        )];
        let (commits, _) = collect(&all, Some((date("2026-09-01"), date("2026-09-07"))));
        let revs: Vec<&str> = commits.iter().map(|c| c.rev.as_str()).collect();
        assert_eq!(revs, ["9", "10"], "两端各一天的余量不能混进统计");
    }

    /// svn 偶尔会给看不懂的时间字段：只能少算并记数，不能让切片 panic
    #[test]
    fn unparsable_dates_are_skipped_not_fatal() {
        let all = vec![(
            0,
            vec![entry("9", "", &["/a"]), entry("10", "昨天", &["/b"]), entry("11", "2026-09-07 09:00:00", &["/c"])],
        )];
        let (commits, skipped) = collect(&all, None);
        assert_eq!(skipped, 2);
        assert_eq!(commits.len(), 1);
    }

    #[test]
    fn daily_buckets_keep_empty_days() {
        let all = vec![(0, vec![entry("9", "2026-09-02 09:00:00", &["/a", "/b"])])];
        let (commits, _) = collect(&all, None);
        let (buckets, gran) = bucketize(&commits, date("2026-09-01"), date("2026-09-03"));
        assert_eq!(gran, Granularity::Day);
        let files: Vec<usize> = buckets.iter().map(|b| b.files).collect();
        assert_eq!(files, vec![0, 2, 0], "空档那天也要留一格");
        assert_eq!(buckets[1].label, "09-02");
    }

    /// 只查当天时按天只有一根柱子，所以要退到 24 个小时格
    #[test]
    fn single_day_splits_into_hours() {
        let all = vec![
            (0, vec![entry("9", "2026-09-07 09:12:00", &["/a"])]),
            (0, vec![entry("10", "2026-09-07 09:58:00", &["/b", "/c"])]),
        ];
        let (commits, _) = collect(&all, None);
        let (buckets, gran) = bucketize(&commits, date("2026-09-07"), date("2026-09-07"));
        assert_eq!(gran, Granularity::Hour);
        assert_eq!(buckets.len(), 24);
        assert_eq!(buckets[9].files, 3);
        assert_eq!(buckets[9].revs, 2);
        assert_eq!(buckets[9].label, "9时");
    }

    #[test]
    fn long_spans_roll_up_to_months() {
        let all = vec![
            (0, vec![entry("9", "2026-03-07 09:00:00", &["/a"])]),
            (0, vec![entry("10", "2026-11-07 09:00:00", &["/b"])]),
        ];
        let (commits, _) = collect(&all, None);
        let (buckets, gran) = bucketize(&commits, date("2026-01-01"), date("2026-12-31"));
        assert_eq!(gran, Granularity::Month);
        assert_eq!(buckets.len(), 12);
        assert_eq!(buckets[2].label, "3月", "同一年内只显示月份就够");
        assert_eq!(buckets[2].files, 1);
        assert_eq!(buckets[10].files, 1);
        assert_eq!(buckets[0].files, 0);
    }

    /// 跨年的月份要带年份，不然两个「3月」分不清
    #[test]
    fn month_labels_carry_the_year_when_spanning_years() {
        let all = vec![(0, vec![entry("9", "2025-03-07 09:00:00", &["/a"])])];
        let (commits, _) = collect(&all, None);
        let (buckets, _) = bucketize(&commits, date("2025-11-01"), date("2026-02-01"));
        assert_eq!(buckets[0].label, "2025-11");
        assert_eq!(buckets.len(), 4);
    }

    #[test]
    fn revspec_reverses_and_widens_the_day_range() {
        assert_eq!(revspec(None), "HEAD:1");
        // 两端各让一天以上：既躲开 {日期} 按 UTC 还是本地解释的分歧，也避免 {D}:{D} 查不到东西
        assert_eq!(
            revspec(Some((date("2026-09-01"), date("2026-09-01")))),
            "{2026-09-03}:{2026-08-31}"
        );
    }

    #[test]
    fn presets_cover_the_expected_days() {
        let today = date("2026-09-07");
        let mut page = StatsPage::new();
        for (preset, from) in [
            (RangePreset::Today, "2026-09-07"),
            (RangePreset::Week, "2026-09-01"),
            (RangePreset::Month, "2026-08-09"),
            (RangePreset::Year, "2025-09-08"),
        ] {
            page.range = preset;
            let span = page.resolve(today).unwrap().expect("预设区间总有边界");
            assert_eq!((span.0, span.1), (date(from), today), "{:?}", preset.label());
        }
        page.range = RangePreset::All;
        assert_eq!(page.resolve(today).unwrap(), None);
    }

    #[test]
    fn custom_range_reports_what_is_wrong() {
        let today = date("2026-09-07");
        let mut page = StatsPage::new();
        page.range = RangePreset::Custom;

        page.custom_from = "2026/09/01".to_owned();
        page.custom_to = String::new();
        assert!(page.resolve(today).unwrap_err().contains("起始日期"));

        page.custom_from = "2026-09-05".to_owned();
        page.custom_to = "2026-09-01".to_owned();
        assert!(page.resolve(today).unwrap_err().contains("早于起始日期"));

        // 结束日期留空 = 到今天
        page.custom_to = String::new();
        assert_eq!(page.resolve(today).unwrap(), Some((date("2026-09-05"), today)));
        // 没补零也认：chrono 的 %Y-%m-%d 能吃下 2026-9-1，用户少打两个 0 不必报错
        page.custom_from = "2026-9-1".to_owned();
        assert_eq!(page.resolve(today).unwrap(), Some((date("2026-09-01"), today)));
    }

    /// 首列要凑满一整周，否则「行 = 周一到周日」会整体错位
    #[test]
    fn heatmap_grid_is_aligned_to_weeks_and_zero_filled() {
        let all = vec![(0, vec![entry("9", "2026-09-04 09:00:00", &["/a", "/b"])])];
        let (commits, _) = collect(&all, None);
        let heat = build_heat(&commits, date("2026-09-02"), date("2026-09-08"));
        assert_eq!(heat.mode, HeatMode::Day);
        assert_eq!(heat.rows, 7);
        assert_eq!(heat.cols, 2, "周三到次周周二跨两周");
        assert_eq!(heat.cells.len(), 7, "区间内每天都有格子，没提交的那天也要有");
        assert_eq!(heat.max, 2);
        let wed = heat.cells.iter().find(|c| c.title == "2026-09-02").unwrap();
        assert_eq!((wed.col, wed.row, wed.files), (0, 2, 0), "9-02 是周三，落在第三行");
        let fri = heat.cells.iter().find(|c| c.title == "2026-09-04").unwrap();
        assert_eq!((fri.col, fri.row, fri.files), (0, 4, 2), "9-04 是周五");
        assert_eq!(heat.row_labels[0], (0, "一".to_owned()));
    }

    #[test]
    fn heatmap_degrades_to_months_before_it_gets_unreadable() {
        let all = vec![(0, vec![entry("9", "2024-05-04 09:00:00", &["/a"])])];
        let (commits, _) = collect(&all, None);
        let heat = build_heat(&commits, date("2023-01-01"), date("2025-12-31"));
        assert_eq!(heat.mode, HeatMode::Month);
        assert_eq!((heat.rows, heat.cols), (12, 3));
        assert_eq!(heat.cells.len(), 36);
        let may = heat.cells.iter().find(|c| c.title == "2024年5月").unwrap();
        assert_eq!((may.col, may.row, may.files), (1, 4, 1));
    }

    /// 查询窗口两端各放宽了 1~2 天（`revspec`），统计出来的横轴必须正好是用户要的那几天，
    /// 边界外的记录一条都不能混进来——否则就是「选了 6 天，图上是 9 天」。
    #[test]
    fn custom_range_chart_covers_exactly_the_requested_days() {
        let today = date("2026-09-11");
        let span = resolve_range(RangePreset::Custom, "2026-08-20", "2026-08-25", today)
            .unwrap()
            .expect("自定义区间要能解析出边界");
        assert_eq!(span, (date("2026-08-20"), date("2026-08-25")));
        assert_eq!(revspec(Some(span)), "{2026-08-27}:{2026-08-19}");

        // 服务器按放宽后的窗口回记录，边界外的那几天也会一起回来
        let all = vec![(
            0,
            vec![
                entry("1", "2026-08-19 10:00:00", &["/before"]),
                entry("2", "2026-08-20 10:00:00", &["/a", "/b"]),
                entry("3", "2026-08-23 10:00:00", &["/c"]),
                entry("4", "2026-08-25 23:00:00", &["/d"]),
                entry("5", "2026-08-26 10:00:00", &["/after"]),
                entry("6", "2026-08-27 10:00:00", &["/after2"]),
            ],
        )];
        let (commits, _) = collect(&all, Some(span));
        let revs: Vec<&str> = commits.iter().map(|commit| commit.rev.as_str()).collect();
        assert_eq!(revs, ["2", "3", "4"], "边界外的版本要裁掉");

        let (buckets, gran) = bucketize(&commits, span.0, span.1);
        assert_eq!(gran, Granularity::Day);
        assert_eq!(buckets.len(), 6, "横轴只能有用户选的那 6 格");
        let labels: Vec<&str> = buckets.iter().map(|bucket| bucket.label.as_str()).collect();
        assert_eq!(labels, ["08-20", "08-21", "08-22", "08-23", "08-24", "08-25"]);
        let files: Vec<usize> = buckets.iter().map(|bucket| bucket.files).collect();
        assert_eq!(files, vec![2, 0, 0, 1, 0, 1]);
    }

    #[test]
    fn axis_steps_stay_on_round_numbers() {
        assert_eq!(y_step(0) * 4, 4);
        assert_eq!(y_step(37) * 4, 40);
        assert_eq!(y_step(101) * 4, 200);
        assert_eq!(y_step(200) * 4, 200);
        assert_eq!(y_step(2000) * 4, 2000);
    }

    /// 点一格跳到「提交记录」页，跳的是哪几天全看这个换算，错一天用户就会看到别的提交
    #[test]
    fn clicking_a_bucket_maps_back_to_its_dates() {
        assert_eq!(
            bucket_span(
                Granularity::Day,
                "2026-09-04",
                Some((date("2026-08-09"), date("2026-09-07")))
            ),
            Some((date("2026-09-04"), date("2026-09-04")))
        );
        assert_eq!(
            bucket_span(Granularity::Month, "2026-02", None),
            Some((date("2026-02-01"), date("2026-02-28"))),
            "平年 2 月 28 天"
        );
        assert_eq!(
            bucket_span(Granularity::Month, "2024-02", None),
            Some((date("2024-02-01"), date("2024-02-29"))),
            "闰年 2 月 29 天"
        );
        assert_eq!(
            bucket_span(Granularity::Month, "2026-12", None),
            Some((date("2026-12-01"), date("2026-12-31"))),
            "12 月不能翻到下一年"
        );
        // 小时格只出现在「当天」区间里，跳过去就是那一天整
        assert_eq!(
            bucket_span(
                Granularity::Hour,
                "09",
                Some((date("2026-09-07"), date("2026-09-07")))
            ),
            Some((date("2026-09-07"), date("2026-09-07")))
        );
        assert_eq!(bucket_span(Granularity::Day, "上周", None), None);
        assert_eq!(bucket_span(Granularity::Month, "2026", None), None);
    }

    /// 提示和高亮、点击下钻用的是同一套反算，坐标算错两格会显示一套、跳去另一套
    #[test]
    fn pointer_hits_the_bucket_the_tooltip_shows() {
        let plot = Rect::from_min_max(Pos2::new(44.0, 10.0), Pos2::new(1044.0, 240.0));
        let slot = 100.0;
        for index in 0..10 {
            let x = plot.left() + slot * (index as f32 + 0.5);
            assert_eq!(index_at(Pos2::new(x, 100.0), plot, slot, 10), Some(index));
        }
        assert_eq!(
            index_at(Pos2::new(10.0, 100.0), plot, slot, 10),
            None,
            "y 轴标签那块区域不算命中"
        );
        assert_eq!(index_at(Pos2::new(600.0, 300.0), plot, slot, 10), None, "画布外不算命中");
    }

    #[test]
    fn heatmap_hits_only_the_cell_under_the_pointer() {
        let (commits, _) = collect(
            &[(0, vec![entry("9", "2026-09-02 09:00:00", &["/a"])])],
            None,
        );
        let heat = build_heat(&commits, date("2026-09-01"), date("2026-09-08"));
        let (origin, pitch, cell) = (Pos2::new(20.0, 16.0), 16.0, 13.0);
        let hit = heat.cells.iter().find(|item| item.title == "2026-09-02").unwrap();
        let center = Pos2::new(
            origin.x + pitch * hit.col as f32 + cell / 2.0,
            origin.y + pitch * hit.row as f32 + cell / 2.0,
        );
        let got = cell_at(center, origin, pitch, cell, &heat).expect("格子正中要命中");
        assert_eq!((got.col, got.row), (hit.col, hit.row));
        assert_eq!(
            (got.from, got.to),
            (date("2026-09-02"), date("2026-09-02")),
            "日格就是那一天"
        );
        assert!(
            cell_at(Pos2::new(center.x + cell + 1.5, center.y), origin, pitch, cell, &heat)
                .is_none(),
            "落在格子间隙里不算命中，否则整列都会亮"
        );
    }

    /// 月格跳过去的日期范围要和它统计过的一致，不能跑到用户没查的月份里
    #[test]
    fn month_cells_carry_clamped_bounds() {
        let (commits, _) = collect(
            &[(0, vec![entry("9", "2023-06-04 09:00:00", &["/a"])])],
            None,
        );
        let heat = build_heat(&commits, date("2023-01-15"), date("2025-03-20"));
        assert_eq!(heat.mode, HeatMode::Month);
        let first = heat.cells.iter().find(|c| c.title == "2023年1月").unwrap();
        assert_eq!((first.from, first.to), (date("2023-01-15"), date("2023-01-31")));
        let last = heat.cells.iter().find(|c| c.title == "2025年3月").unwrap();
        assert_eq!((last.from, last.to), (date("2025-03-01"), date("2025-03-20")));
        let mid = heat.cells.iter().find(|c| c.title == "2024年2月").unwrap();
        assert_eq!((mid.from, mid.to), (date("2024-02-01"), date("2024-02-29")));
    }

    /// 月历第一列必须是周一：排错一天，用户点的 15 号就变成 16 号
    #[test]
    fn calendar_grid_starts_on_monday_and_keeps_six_weeks() {
        let days = calendar_days(2026, 9);
        assert_eq!(days.len(), 42, "固定 6 周，翻月才不会上下跳");
        // 2026-09-01 是周二，所以首格是 8/31 的占位空
        assert_eq!(days[0], None, "不属于本月的格子留空");
        assert_eq!(days[1], Some(date("2026-09-01")));
        assert_eq!(days[6], Some(date("2026-09-06")), "第一行最后一格是周日");
        let in_month: Vec<NaiveDate> = days.iter().flatten().copied().collect();
        assert_eq!(in_month.len(), 30, "9 月 30 天，不多不少");
        assert_eq!(in_month.first(), Some(&date("2026-09-01")));
        assert_eq!(in_month.last(), Some(&date("2026-09-30")));
        assert_eq!(calendar_days(2024, 2).iter().flatten().count(), 29, "闰年 2 月");
        assert_eq!(calendar_days(2026, 2).iter().flatten().count(), 28, "平年 2 月");
        assert_eq!(
            calendar_days(2026, 12).into_iter().flatten().last(),
            Some(date("2026-12-31")),
            "12 月不能溢出到下一年"
        );
    }

    #[test]
    fn month_shifting_wraps_across_the_year() {
        assert_eq!(shift_month(date("2026-01-01"), -1), date("2025-12-01"));
        assert_eq!(shift_month(date("2026-12-01"), 1), date("2027-01-01"));
        assert_eq!(shift_month(date("2026-03-01"), -1), date("2026-02-01"));
    }

    /// 一帧的输入：给了坐标就在该点按一次左键（先移动再按下再抬起，否则 egui 认不出这是点击）
    fn frame(click: Option<Pos2>) -> egui::RawInput {
        let mut events = Vec::new();
        if let Some(pos) = click {
            let press = |pressed: bool| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            events.push(egui::Event::PointerMoved(pos));
            events.push(press(true));
            events.push(press(false));
        }
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 700.0))),
            focused: true,
            events,
            ..Default::default()
        }
    }

    /// 月历点「← / →」翻月时弹层不能被关掉——`Popup::menu` 默认的 CloseOnClick 会因为
    /// 「弹层内任意一次点击」收起，那样除了第一天以外根本翻不动。
    #[test]
    fn calendar_survives_month_navigation_and_picks_a_day() {
        let ctx = egui::Context::default();
        crate::fonts::install_cjk(&ctx);
        let today = date("2026-09-11");
        let mut text = "2026-09-04".to_owned();
        let id = Id::new("calendar_popup_test");
        let center_of = |shapes: &[egui::epaint::ClippedShape], label: &str| {
            painted_text(shapes)
                .into_iter()
                .find(|(text, _)| text == label)
                .map(|(_, rect)| rect.center())
        };
        let mut step = |click: Option<Pos2>| -> Vec<egui::epaint::ClippedShape> {
            let mut output = ctx.run_ui(frame(click), |ui| {
                date_input(ui, id, &mut text, "2026-09-01", today, DayLimits::default());
            });
            output.textures_delta.clear();
            output.shapes
        };

        // 1. 未点开：没有月历
        let shapes = step(None);
        assert!(center_of(&shapes, "2026年9月").is_none(), "没点开不该有月历");

        // 2. 点「日历」→ 月历出现，停在输入框那一月
        //    （Popup 的开关状态存在内存里，点下去那一帧还不画弹层，要再跑一帧）
        let button = center_of(&shapes, "日历").expect("没有「日历」按钮");
        step(Some(button));
        let shapes = step(None);
        assert!(center_of(&shapes, "2026年9月").is_some(), "点开后应该显示 9 月");

        // 3. 点「→」→ 弹层还在，且翻到 10 月（弹层宽度按内容走，右侧按钮才不会被裁掉）
        let next = center_of(&shapes, "→").expect("月历里没有「下一月」按钮");
        step(Some(next));
        let shapes = step(None);
        assert!(
            center_of(&shapes, "2026年10月").is_some(),
            "点下一月之后弹层该留着并翻到 10 月"
        );

        // 4. 连点两次「←」→ 回到 8 月
        let back = center_of(&shapes, "←").expect("月历里没有「上一月」按钮");
        step(Some(back));
        let shapes = step(None);
        step(Some(center_of(&shapes, "←").expect("翻回来时按钮该还在")));
        let shapes = step(None);
        assert!(center_of(&shapes, "2026年8月").is_some(), "连翻两月应该回到 8 月");

        // 5. 点 8 月 17 号 → 写回输入框，弹层收起
        let day = center_of(&shapes, "17").expect("8 月该有 17 号这一格");
        step(Some(day));
        let shapes = step(None);
        assert_eq!(text, "2026-08-17", "点中的日期要写回输入框");
        assert!(center_of(&shapes, "←").is_none(), "选完一天就该收起弹层");
    }

    /// 起始框的日历不能选到结束日之后，反之亦然——「开始大于结束」要在选的时候就被挡住
    #[test]
    fn day_limits_block_the_reversed_side() {
        let limits = DayLimits { from: None, to: Some(date("2026-09-05")) };
        assert!(limits.allows(date("2026-09-05")), "上限当天本身要能选");
        assert!(!limits.allows(date("2026-09-06")), "上限之后就点不了");
        let limits = DayLimits { from: Some(date("2026-09-05")), to: None };
        assert!(!limits.allows(date("2026-09-04")));
        assert!(DayLimits::default().allows(date("2026-09-04")), "不设限时什么都能点");
    }

    #[test]
    fn reversed_dates_are_flagged_without_waiting_for_query() {
        assert!(reversed_hint("2026-09-10", "2026-09-01").is_some_and(|warn| warn.contains("晚于")));
        assert!(reversed_hint("2026-09-01", "2026-09-10").is_none());
        assert!(reversed_hint("2026-09-01", "").is_none(), "结束日留空 = 到今天，不该报错");
        assert!(reversed_hint("乱填", "2026-09-10").is_none(), "读不出日期时交给查询去报");
    }

    /// 月历是手排的网格，画一帧才知道星期表头和日期格子有没有挤在一起
    #[test]
    fn calendar_paints_weekday_row_and_every_day() {
        let today = date("2026-09-11");
        let shapes = paint(|ui| {
            month_calendar(ui, Id::new("test_cal"), "2026-09-04", today, DayLimits::default());
        });
        let texts = painted_text(&shapes);
        let digits: Vec<&String> = texts
            .iter()
            .map(|(text, _)| text)
            .filter(|text| text.parse::<u32>().is_ok())
            .collect();
        assert_eq!(digits.len(), 30, "九月该有 30 个日期格子：{digits:?}");
        for name in WEEKDAY_NAMES {
            assert!(
                texts.iter().any(|(text, _)| text == name),
                "星期表头少了 {name}"
            );
        }
        assert!(
            texts.iter().any(|(text, _)| text == "2026年9月"),
            "没标出在看哪个月"
        );
        // 只数个数抓不出「排成一长条」：按 y 分行，九月应该铺开成 5 行、每行最多 7 格
        let mut rows: std::collections::BTreeMap<i32, usize> = std::collections::BTreeMap::new();
        for (text, rect) in &texts {
            if text.parse::<u32>().is_ok() {
                *rows.entry((rect.top() * 2.0).round() as i32).or_default() += 1;
            }
        }
        assert_eq!(rows.len(), 5, "九月 30 天该铺成 5 行，实际 {rows:?}");
        assert!(
            rows.values().all(|count| *count <= 7),
            "有一行超过 7 格，说明没按周换行：{rows:?}"
        );
        for (left, (first, first_rect)) in texts.iter().enumerate() {
            for (second, second_rect) in texts[left + 1..].iter() {
                assert!(
                    first_rect.intersect(*second_rect).area() < 1.0,
                    "月历里 {first:?} 和 {second:?} 叠在一起"
                );
            }
        }
    }

    // ---- 下面是渲染级测试 ----
    //
    // 图是 Painter 一笔笔画出来的，只测汇总数据看不出「轴标签挤成一片」「格子漏画」这类
    // 画歪的问题。所以按 tests/side_by_side_diff.rs 的做法，直接检查 egui 这一帧真正
    // 产出的 shapes，而不是布局回报的矩形。

    /// 用系统中文字体跑一帧，返回画面上出现的所有图形
    fn paint(draw: impl FnMut(&mut Ui)) -> Vec<egui::epaint::ClippedShape> {
        let ctx = egui::Context::default();
        crate::fonts::install_cjk(&ctx);
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    Pos2::ZERO,
                    Vec2::new(1200.0, 700.0),
                )),
                focused: true,
                ..Default::default()
            },
            draw,
        );
        // 测试里没有渲染器，字体增量不应用就得显式丢弃，否则 epaint 在 Drop 时 panic
        output.textures_delta.clear();
        output.shapes
    }

    fn painted_text(shapes: &[egui::epaint::ClippedShape]) -> Vec<(String, Rect)> {
        shapes
            .iter()
            .filter_map(|item| match &item.shape {
                egui::Shape::Text(text) => Some((
                    text.galley.text().to_string(),
                    item.shape.visual_bounding_rect(),
                )),
                _ => None,
            })
            .filter(|(text, _)| !text.trim().is_empty())
            .collect()
    }

    /// 按「提交时间 + 该次提交涉及几个文件」造一批记录，再走真实的分格流程
    fn buckets_for(items: &[(&str, usize)], from: &str, to: &str) -> Vec<Bucket> {
        let entries: Vec<LogEntry> = items
            .iter()
            .enumerate()
            .map(|(index, (when, count))| {
                entry(&(index + 1).to_string(), when, &vec!["/f"; *count])
            })
            .collect();
        let (commits, _) = collect(&[(0, entries)], None);
        bucketize(&commits, date(from), date(to)).0
    }

    #[test]
    fn bar_and_line_charts_actually_paint_geometry() {
        let files = [3usize, 0, 12, 5, 0, 8, 1, 0];
        let days: Vec<String> = files
            .iter()
            .enumerate()
            .map(|(day, _)| format!("2026-09-0{} 09:00:00", day + 1))
            .collect();
        let items: Vec<(&str, usize)> = days
            .iter()
            .zip(files.iter())
            .map(|(when, count)| (when.as_str(), *count))
            .collect();
        let buckets = buckets_for(&items, "2026-09-01", "2026-09-08");
        assert_eq!(buckets.len(), 8);

        let bars = paint(|ui| {
            trend_chart(ui, &buckets, ChartKind::Bar, Granularity::Day, None);
        });
        let rects = bars
            .iter()
            .filter(|item| matches!(item.shape, egui::Shape::Rect(_)))
            .count();
        assert!(
            rects >= files.iter().filter(|count| **count > 0).count(),
            "八格里只画出了 {rects} 个矩形"
        );
        let lines = paint(|ui| {
            trend_chart(ui, &buckets, ChartKind::Line, Granularity::Day, None);
        });
        assert!(
            lines.iter().any(|item| matches!(item.shape, egui::Shape::Path(_))),
            "折线图一个路径都没有画"
        );
    }

    /// 轴标签、图例、刻度数字叠在一起就等于读不出来，整帧里任何两块文字都不许重合
    #[test]
    fn nothing_in_the_chart_is_painted_over() {
        let hours: Vec<String> = (0..24).map(|hour| format!("2026-09-07 {hour:02}:10:00")).collect();
        let days: Vec<String> = (0..30)
            .map(|offset| {
                format!(
                    "{} 09:00:00",
                    (date("2026-08-09") + TimeDelta::days(offset)).format("%Y-%m-%d")
                )
            })
            .collect();
        let months: Vec<String> = (1..=12)
            .map(|month| format!("2026-{month:02}-03 09:00:00"))
            .collect();

        for (times, from, to, spans) in [
            (&hours, "2026-09-07", "2026-09-07", 24),
            (&days, "2026-08-09", "2026-09-07", 30),
            (&months, "2026-01-01", "2026-12-31", 12),
        ] {
            let items: Vec<(&str, usize)> =
                times.iter().map(|when| (when.as_str(), 7)).collect();
            let buckets = buckets_for(&items, from, to);
            assert_eq!(buckets.len(), spans, "{from}..{to} 分格数不对");
            let shapes = paint(|ui| {
                legend(
                    ui,
                    &[
                        ("文件条目数", Color32::from_rgb(90, 160, 240)),
                        ("提交次数", Color32::from_rgb(240, 175, 70)),
                        ("按天（30 格，左轴上限 40）", Color32::GRAY),
                    ],
                );
                trend_chart(ui, &buckets, ChartKind::Bar, Granularity::Day, None);
            });
            let texts = painted_text(&shapes);
            assert!(texts.len() > 4, "这一帧几乎什么都没画：{texts:?}");
            for (left, (first, first_rect)) in texts.iter().enumerate() {
                for (second, second_rect) in texts[left + 1..].iter() {
                    let overlap = first_rect.intersect(*second_rect).area();
                    assert!(
                        overlap < 1.0,
                        "{from}..{to} 的两块文字叠在一起 {overlap:.1}px：{first:?} / {second:?}"
                    );
                }
            }
        }
    }

    /// 提示框默认按 `spacing.tooltip_width` 排版，中文标签会被挤到第二行；
    /// 这里直接把提示画出来，按文字矩形的高度判断有没有折行。
    /// （不走悬停：无头跑一帧时 `Response::hovered` 拿不到，实机是能出的。）
    #[test]
    fn tooltip_keeps_label_and_number_on_one_line() {
        let ctx = egui::Context::default();
        crate::fonts::install_cjk(&ctx);
        let input = || egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 700.0))),
            focused: true,
            // at_pointer 的提示没有指针位置就不画，先给一个
            events: vec![egui::Event::PointerMoved(Pos2::new(300.0, 300.0))],
            ..Default::default()
        };
        let show = |ui: &mut Ui| {
            let (response, _) = ui.allocate_painter(Vec2::new(600.0, 100.0), Sense::hover());
            chart_tooltip(
                &response,
                "08-30".to_owned(),
                &["文件条目数：128".to_owned(), "提交次数：12".to_owned()],
            );
        };
        // Popup 的打开状态存在内存里，单帧 run_ui 当帧还画不出提示，第二帧才有
        let mut output = ctx.run_ui(input(), |ui| show(ui));
        output.textures_delta.clear();
        let mut output = ctx.run_ui(input(), |ui| show(ui));
        output.textures_delta.clear();
        let rows: Vec<(String, f32)> = painted_text(&output.shapes)
            .into_iter()
            .filter(|(text, _)| text.contains("条目数") || text.contains("提交次数"))
            .map(|(text, rect)| (text, rect.height()))
            .collect();
        assert_eq!(rows.len(), 2, "提示里两行内容没都画出来：{rows:?}");
        for (text, height) in &rows {
            assert!(*height < 20.0, "提示里的 {text:?} 被折成了多行（高 {height:.1}px）");
        }
    }

    /// 图里的文字都要有中文字体支撑，缺一个字形用户看到的就是方块
    #[test]
    fn chart_text_has_no_missing_glyphs() {
        let buckets = buckets_for(
            &[("2026-09-01 09:00:00", 4), ("2026-09-03 09:00:00", 6)],
            "2026-09-01",
            "2026-09-03",
        );
        let (commits, _) = collect(
            &[(0, vec![entry("9", "2026-09-02 09:00:00", &["/a"])])],
            None,
        );
        let heat = build_heat(&commits, date("2026-09-01"), date("2026-09-03"));
        let shapes = paint(|ui| {
            legend(ui, &[("文件条目数", Color32::GRAY), ("提交次数", Color32::BLUE)]);
            trend_chart(ui, &buckets, ChartKind::Line, Granularity::Day, None);
            heat_legend(ui, &heat);
            heatmap_chart(ui, &heat);
        });
        let ctx = egui::Context::default();
        crate::fonts::install_cjk(&ctx);
        // 先空跑一帧，字体就绪后才查得到字形
        let mut warm = ctx.run_ui(egui::RawInput::default(), |_| {});
        warm.textures_delta.clear();
        let font = FontId::proportional(11.5);
        let mut missing: Vec<char> = Vec::new();
        ctx.fonts_mut(|fonts| {
            for (text, _) in painted_text(&shapes) {
                for ch in text.chars() {
                    if !ch.is_ascii() && !fonts.has_glyph(&font, ch) && !missing.contains(&ch) {
                        missing.push(ch);
                    }
                }
            }
        });
        assert!(missing.is_empty(), "这些字符在正文里没有字形（会显示成方块）：{missing:?}");
    }
}

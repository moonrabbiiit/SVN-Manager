//! 日期区间与月历选择器。
//!
//! 「提交记录」页顶上那排「当天 / 一周 / 一个月 / 一年 / 所有 / 自定义」，以及自定义那
//! 两个日期框（手输 + 点「日历」弹月历）都在这里。`resolve_range` 把页面上的选择定成一段
//! 日期区间，`revspec` 再把它拼成 svn 的 `-r` 串：两边必须同一套口径，否则页面上筛出来的
//! 和服务器上读回来的对不上。

use chrono::{Datelike, NaiveDate, TimeDelta};
use egui::{Align, Color32, Grid, Id, Label, RichText, Stroke, TextEdit, Ui, Vec2};

use crate::ink;
use crate::ui::{AlignedRow, field_margin};

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

/// 预设 / 自定义输入 → 实际日期区间（`None` = 不限日期）。
/// 「提交记录」页的日期筛选只走这一个函数：页面上写的区间和发给 svn 的区间必须同源。
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
/// 多取的部分由「提交记录」页按同一个 `span` 裁回去，列表里始终只算用户要的那几天。
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

// ------------------------------------------------------------------------ 月历与日期框

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
    ui.add(
        TextEdit::singleline(text)
            .hint_text(hint)
            .margin(field_margin(ui))
            .desired_width(96.0),
    );
    let button = ui
        .button("日历")
        .on_hover_text("在日历中选择日期，也可直接在输入框中输入");
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
    ui.aligned_row(|ui| {
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
                        hit.on_hover_text("受另一日期框限制，无法选择该日期");
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

/// 周一为 0（GitHub 那种排法）；chrono 的 weekday() 从周日开始数，所以要挪一位
fn row_of(day: NaiveDate) -> usize {
    ((day.weekday().num_days_from_sunday() + 6) % 7) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Pos2, Rect};

    fn date(text: &str) -> NaiveDate {
        parse_date(text).expect("测试里的日期要合法")
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

    /// 用系统中文字体跑一帧，返回画面上出现的所有图形
    fn paint(draw: impl FnMut(&mut Ui)) -> Vec<egui::epaint::ClippedShape> {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, crate::fonts::AUTO);
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
        for (preset, from) in [
            (RangePreset::Today, "2026-09-07"),
            (RangePreset::Week, "2026-09-01"),
            (RangePreset::Month, "2026-08-09"),
            (RangePreset::Year, "2025-09-08"),
        ] {
            let span = resolve_range(preset, "", "", today).unwrap().expect("预设区间总有边界");
            assert_eq!((span.0, span.1), (date(from), today), "{:?}", preset.label());
        }
        assert_eq!(resolve_range(RangePreset::All, "", "", today).unwrap(), None);
    }

    #[test]
    fn custom_range_reports_what_is_wrong() {
        let today = date("2026-09-07");
        let mut from;
        let mut to;

        from = "2026/09/01".to_owned();
        to = String::new();
        assert!(resolve_range(RangePreset::Custom, &from, &to, today)
            .unwrap_err()
            .contains("起始日期"));

        from = "2026-09-05".to_owned();
        to = "2026-09-01".to_owned();
        assert!(resolve_range(RangePreset::Custom, &from, &to, today)
            .unwrap_err()
            .contains("早于起始日期"));

        // 结束日期留空 = 到今天
        to = String::new();
        assert_eq!(resolve_range(RangePreset::Custom, &from, &to, today)
            .unwrap(),
            Some((date("2026-09-05"), today)));
        // 没补零也认：chrono 的 %Y-%m-%d 能吃下 2026-9-1，用户少打两个 0 不必报错
        from = "2026-9-1".to_owned();
        assert_eq!(resolve_range(RangePreset::Custom, &from, &to, today)
            .unwrap(),
            Some((date("2026-09-01"), today)));
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

    /// 月历点「← / →」翻月时弹层不能被关掉——`Popup::menu` 默认的 CloseOnClick 会因为
    /// 「弹层内任意一次点击」收起，那样除了第一天以外根本翻不动。
    #[test]
    fn calendar_survives_month_navigation_and_picks_a_day() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, crate::fonts::AUTO);
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
}

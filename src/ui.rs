//! 各处共用的绘制小工具。`main.rs` 把它们 `pub use` 出去，所以其它模块照旧写 `crate::ink`。

use egui::text::LayoutJob;
use egui::{Color32, FontId, TextFormat, Ui, Vec2};

/// 样式微调：`SvnApp::new` 对深浅两套样式各调一次（两套各自独立，主题切换后照样生效）。
///
/// 排版部分与主题无关。文字颜色只动深色主题：egui 默认的深色正文是 gray(140)、弱化系数
/// 0.6（≈gray(84)），而面板底色是 gray(27)，弱化文字只有 2.4:1 的对比度——输出区的普通行、
/// 各处说明文字这些灰字在深色下基本看不清。这里把正文抬到 gray(160)、系数抬到 0.8
/// （≈gray(128)，4.4:1）：层次还在，两种字都读得下来。
/// 浅色主题的正文是 gray(80) 打底（弱化后 12:1 以上），本来就够，不动。
pub fn tune_style(style: &mut egui::Style) {
    style.spacing.item_spacing = Vec2::new(7.0, 5.0);
    style.spacing.button_padding = Vec2::new(8.0, 3.0);
    if style.visuals.dark_mode {
        style.visuals.widgets.noninteractive.fg_stroke.color = Color32::from_gray(160);
        style.visuals.weak_text_alpha = 0.8;
    }
}

/// 强调色都是按深色背景调的，浅色主题下压暗一点，保证白底上也能看清。
pub fn ink(ui: &Ui, color: Color32) -> Color32 {
    if ui.visuals().dark_mode {
        color
    } else {
        // 只压暗 RGB，不能动 alpha（gamma_multiply 会连 alpha 一起乘，字会变成半透明）
        let [r, g, b, a] = color.to_array();
        Color32::from_rgba_unmultiplied(
            (r as f32 * 0.55 + 0.5) as u8,
            (g as f32 * 0.55 + 0.5) as u8,
            (b as f32 * 0.55 + 0.5) as u8,
            a,
        )
    }
}

/// 搜索高亮：把命中的 `needle` 片段用醒目底色标出来，其余文字保持原样。
/// 忽略 ASCII 大小写；按字节匹配是安全的——UTF-8 的后续字节都落在 0x80..=0xBF，
/// 可用作首字节的一定 >= 0xC2，所以命中位置必然在字符边界上，不会切坏中文。
pub fn highlight(ui: &Ui, text: &str, needle: &str, font: FontId, color: Color32) -> LayoutJob {
    let plain = TextFormat::simple(font, color);
    let mut job = LayoutJob::default();
    let key = needle.trim().as_bytes();
    if key.is_empty() {
        job.append(text, 0.0, plain);
        return job;
    }
    let dark = ui.visuals().dark_mode;
    let mark = mark_format(dark, &plain);
    let bytes = text.as_bytes();
    let mut cursor = 0usize;
    while cursor + key.len() <= bytes.len() {
        let found = bytes[cursor..].windows(key.len()).position(|window| {
            window
                .iter()
                .zip(key)
                .all(|(left, right)| left.eq_ignore_ascii_case(right))
        });
        let Some(offset) = found else { break };
        let start = cursor + offset;
        job.append(&text[cursor..start], 0.0, plain.clone());
        job.append(&text[start..start + key.len()], 0.0, mark.clone());
        cursor = start + key.len();
    }
    if cursor < bytes.len() {
        job.append(&text[cursor..], 0.0, plain);
    }
    job
}

/// 标记命中片段的样式：深浅两套主题各一组，字色与底色配套。
fn mark_format(dark: bool, plain: &TextFormat) -> TextFormat {
    TextFormat {
        color: if dark {
            Color32::from_rgb(255, 216, 100)
        } else {
            Color32::from_rgb(64, 44, 0)
        },
        background: if dark {
            Color32::from_rgb(96, 76, 18)
        } else {
            Color32::from_rgb(255, 234, 140)
        },
        ..plain.clone()
    }
}

/// 整词的边界：前后不能是字母、数字或下划线（中文算字母，所以整词对中文串很严格）。
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// 逐字符比较的相等判断。不用 `to_lowercase`：它会把字节数改掉（`İ` 展开成两个字节），
/// 拿折叠后的偏移回原文定位会把高亮标错位。
fn slice_eq(a: &str, b: &str, case: bool) -> bool {
    if case {
        return a == b;
    }
    a.chars().flat_map(char::to_lowercase).eq(b.chars().flat_map(char::to_lowercase))
}

/// 一段搜索条件：普通子串 / 大小写敏感 / 正则 / 整词。
///
/// 正则在构建时就编译好，逐条记录判定时只是查一次；放在循环里编译的话，六千多条记录
/// 能把这一帧拖成几秒。
#[derive(Clone)]
pub struct Search {
    /// 界面上原样保留的输入（已去首尾空格）
    pub needle: String,
    regex: Option<regex::Regex>,
    /// 正则写错时的说明，界面直接显示出来
    pub error: String,
    case: bool,
    word: bool,
}

impl Search {
    /// 空输入 = 不过滤也不高亮。正则模式下表达式不合法时 `error` 有说明、且一律不命中。
    pub fn build(needle: &str, case: bool, regex: bool, word: bool) -> Self {
        let needle = needle.trim().to_owned();
        if needle.is_empty() || !regex {
            return Self { needle, regex: None, error: String::new(), case, word };
        }
        // 整词 + 正则：先把用户的式子包成 `(?:…)` 再两头加 `\b`。直接拼在最外层会被
        // 式子里的 `|` 抢走结合性（`add|del` 变成「整词 add」或「del」）
        let pattern = if word {
            format!(r"\b(?:{needle})\b")
        } else {
            needle.clone()
        };
        match regex::RegexBuilder::new(&pattern)
            .case_insensitive(!case)
            .build()
        {
            Ok(re) => Self { needle, regex: Some(re), error: String::new(), case, word },
            Err(e) => Self {
                needle,
                regex: None,
                error: format!("正则不合法：{e}"),
                case,
                word,
            },
        }
    }

    pub fn is_empty(&self) -> bool {
        self.needle.is_empty()
    }

    /// 命中区间（原文字节偏移）。判定与高亮都走这里，两边看到的永远是同一批字。
    pub fn ranges(&self, text: &str) -> Vec<(usize, usize)> {
        if self.needle.is_empty() {
            return Vec::new();
        }
        match &self.regex {
            Some(re) => re
                .find_iter(text)
                .map(|m| (m.start(), m.end()))
                .filter(|(start, end)| !self.word || self.boundary_ok(text, *start, *end))
                .collect(),
            // 表达式写错时不命中任何东西（也不能当成「全部命中」把列表清空）
            None if !self.error.is_empty() => Vec::new(),
            None => self.literal_ranges(text),
        }
    }

    /// 是否命中；空输入算命中（不过滤）。
    pub fn hits(&self, text: &str) -> bool {
        if self.needle.is_empty() {
            return true;
        }
        // 表达式写错不等于「谁都不匹配」：列表保持原样，界面上浮一条说明就好。
        // 高亮那边 ranges 返回空，所以不会有任何涂色，两边不矛盾
        if !self.error.is_empty() {
            return true;
        }
        match &self.regex {
            // 正则且没开整词：`is_match` 找到第一处就返回，不必把命中全数出来
            Some(re) if !self.word => re.is_match(text),
            _ => !self.ranges(text).is_empty(),
        }
    }

    /// 子串模式：从每个字符边界起试一次，忽略大小写时按 Unicode 简单折叠比较。
    fn literal_ranges(&self, text: &str) -> Vec<(usize, usize)> {
        let bytes = text.as_bytes();
        let width = self.needle.len();
        let mut out = Vec::new();
        let mut start = 0;
        while start + width <= bytes.len() {
            // 起点落在字符中间时 `get` 给 None，跳过就是
            if let Some(span) = text.get(start..start + width) {
                let end = start + width;
                if slice_eq(span, &self.needle, self.case)
                    && (!self.word || self.boundary_ok(text, start, end))
                {
                    out.push((start, end));
                }
            }
            let step = text[start..].chars().next().map_or(1, |c| c.len_utf8());
            start += step;
        }
        out
    }

    fn boundary_ok(&self, text: &str, start: usize, end: usize) -> bool {
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char)
    }
}

/// 按 `Search` 实际命中的区间高亮：开关一开，标出来的就该是真正匹配上的那几段，
/// 而不是不分大小写的同一串字。
pub fn highlight_with(
    ui: &Ui,
    text: &str,
    search: &Search,
    font: FontId,
    color: Color32,
) -> LayoutJob {
    let plain = TextFormat::simple(font, color);
    let mark = mark_format(ui.visuals().dark_mode, &plain);
    let mut job = LayoutJob::default();
    let mut cursor = 0usize;
    for (start, end) in search.ranges(text) {
        if start < cursor || end > text.len() {
            continue;
        }
        job.append(&text[cursor..start], 0.0, plain.clone());
        job.append(&text[start..end], 0.0, mark.clone());
        cursor = end;
    }
    job.append(&text[cursor..], 0.0, plain);
    job
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Context, RawInput, ThemePreference};

    /// 跑一遍高亮，把结果拆成「未命中的原文」与「命中的片段」，方便断言。
    fn split(dark: bool, text: &str, needle: &str) -> (String, Vec<String>) {
        let ctx = Context::default();
        ctx.set_theme(if dark {
            ThemePreference::Dark
        } else {
            ThemePreference::Light
        });
        let mut plain = String::new();
        let mut marks: Vec<String> = Vec::new();
        let mut output = ctx.run_ui(RawInput::default(), |ui| {
            let job = highlight(ui, text, needle, FontId::proportional(12.0), Color32::WHITE);
            for section in &job.sections {
                let range = section.byte_range.start.0..section.byte_range.end.0;
                let piece = &job.text[range];
                if piece.is_empty() {
                    continue;
                }
                if section.format.background == Color32::TRANSPARENT {
                    plain.push_str(piece);
                } else {
                    marks.push(piece.to_owned());
                }
            }
        });
        // 测试不渲染纹理，显式丢弃字体增量，否则 epaint 在 Drop 时按未应用增量 panic
        output.textures_delta.clear();
        (plain, marks)
    }

    #[test]
    fn empty_needle_keeps_text_untouched() {
        let (plain, marks) = split(true, "svn log -r HEAD:1", "   ");
        assert_eq!(plain, "svn log -r HEAD:1");
        assert!(marks.is_empty(), "{marks:?}");
    }

    #[test]
    fn marks_every_match_case_insensitively() {
        let (plain, marks) = split(true, "SVN svn svn/HRP.Hr", "svn");
        assert_eq!(marks, vec!["SVN", "svn", "svn"]);
        assert_eq!(plain, "  /HRP.Hr");
    }

    /// 相对亮度（WCAG 2.x）：对比度断言用，和视觉上的明暗感受一致
    fn luminance(color: Color32) -> f32 {
        let [r, g, b, _] = color.to_array();
        let channel = |value: u8| {
            let c = value as f32 / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    }

    /// 前景与底色的对比度（1 ~ 21）
    fn contrast(fg: Color32, bg: Color32) -> f32 {
        let (fg, bg) = (luminance(fg), luminance(bg));
        let (hi, lo) = if fg > bg { (fg, bg) } else { (bg, fg) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// 深色主题的灰字要看得清：调过样式后，弱化文字与正文对面板底色都得到 4:1 以上，
    /// 且正文仍比弱文字亮（层次不能丢）。浅色主题本来就够，不许动。
    #[test]
    fn dark_gray_text_is_readable() {
        let ctx = Context::default();
        let before = ctx.style_of(egui::Theme::Dark).visuals.weak_text_color();
        ctx.style_mut_of(egui::Theme::Dark, tune_style);
        ctx.style_mut_of(egui::Theme::Light, tune_style);
        let dark = ctx.style_of(egui::Theme::Dark).visuals.clone();
        let weak = dark.weak_text_color();
        assert!(
            contrast(weak, dark.panel_fill) >= 4.0,
            "深色主题的弱化文字看不清：{weak:?} 对 {:?} 只有 {:.1}:1",
            dark.panel_fill,
            contrast(weak, dark.panel_fill)
        );
        assert!(
            contrast(dark.text_color(), dark.panel_fill) >= 5.0,
            "深色主题的正文对比度不够"
        );
        assert!(
            luminance(weak) < luminance(dark.text_color()),
            "弱化文字该比正文暗，层次别拉平"
        );
        assert!(
            contrast(before, dark.panel_fill) < 3.0,
            "egui 默认的灰字本来就低（2.3:1），这条是这次要改的理由"
        );
        let light = ctx.style_of(egui::Theme::Light).visuals.clone();
        assert_eq!(light.weak_text_alpha, 0.6, "浅色主题的弱化系数不该被改");
        assert!(
            contrast(light.weak_text_color(), light.panel_fill) >= 5.0,
            "浅色主题的灰字本来就够看"
        );
    }

    #[test]
    fn multibyte_needle_splits_on_char_boundaries() {
        // 前后夹着中文，确认按字节匹配不会把 UTF-8 序列切坏（切坏会直接 panic）
        let (plain, marks) = split(false, "修复 药品明细 保存 药品", "药品");
        assert_eq!(marks, vec!["药品", "药品"]);
        assert_eq!(plain, "修复 明细 保存 ");
    }
}

#[cfg(test)]
mod search_tests {
    use super::{highlight_with, Search};
    use egui::{Color32, Context, FontId, RawInput, ThemePreference};

    fn build(needle: &str, case: bool, regex: bool, word: bool) -> Search {
        let search = Search::build(needle, case, regex, word);
        assert!(search.error.is_empty(), "不该报错：{}", search.error);
        search
    }

    #[test]
    fn plain_text_search_ignores_case_until_switched_on() {
        assert!(build("fix", false, false, false).hits("Fixed 药品"));
        assert!(
            !build("fix", true, false, false).hits("Fixed 药品"),
            "开了大小写敏感就不该认 fix"
        );
        assert!(build("FIX", true, false, false).hits("FIX 掉了"), "同写法大小写敏感照样命中");
        assert!(!build("FIX", true, false, false).hits("Fixed 药品"), "大小写敏感时 FIX 不认 Fixed");
        assert!(build("Fix", true, false, false).hits("Fixed 药品"));
        // 中文没有大小写，两种开关状态下都按原样比
        assert!(build("药品", true, false, false).hits("修复 药品 明细"));
    }

    #[test]
    fn whole_word_skips_partial_matches() {
        assert!(build("add", false, false, true).hits("please add it"));
        assert!(
            !build("add", false, false, true).hits("address book"),
            "address 里的 add 不算整词"
        );
        assert!(build("add", false, false, true).hits("add-on"), "连字符算词边界");
        // 每个汉字都算「字母」，所以整词对中文子串更严格，标点隔开才算
        assert!(!build("药品", false, false, true).hits("修复药品明细"));
        assert!(build("药品", false, false, true).hits("修复「药品」明细"));
    }

    #[test]
    fn regex_mode_follows_the_same_two_switches() {
        assert!(build(r"r\d+", false, true, false).hits("本次 r1208 提交"));
        assert!(!build(r"r\d+", true, true, false).hits("本次 R1208 提交"));
        assert!(build(r"R\d+", true, true, false).hits("本次 R1208 提交"));
        assert!(build("药品|明细", false, true, false).hits("保存明细"));
        // 交替式配整词时 `\b` 必须包住整个式子：否则只剩「整词 药品」这一半管用。
        // 汉字都算词字符，所以「保存明细」里的明细没有词边界，被标点隔开才算整词
        assert!(build("药品|明细", false, true, true).hits("保存「明细」"));
        assert!(!build("药品|明细", false, true, true).hits("保存明细"));
        assert!(!build("add|del", false, true, true).hits("delete all"));
        assert_eq!(build(r"r\d+", false, true, false).ranges("本次 r1208 提交").len(), 1);
    }

    #[test]
    fn broken_regex_says_why_and_filters_nothing() {
        let search = Search::build("([", false, true, false);
        assert!(!search.error.is_empty(), "写错的表达式要给说明");
        // 列表保持原样（谁都不被筛掉），但也不会有任何涂色
        assert!(search.hits("任意文本"), "写错不等于「谁都不匹配」");
        assert!(search.ranges("任意文本").is_empty(), "写错时不该高亮任何东西");
        // 空输入不过滤
        assert!(Search::build("   ", false, false, false).is_empty());
        assert!(Search::build("", false, true, true).hits("任意文本"));
    }

    /// 高亮标的必须正是命中的那几段：大小写敏感时不能把另一种写法也涂上色
    #[test]
    fn highlight_marks_exactly_the_hits() {
        let ctx = Context::default();
        ctx.set_theme(ThemePreference::Dark);
        let search = build("fix", true, false, false);
        let mut marked = Vec::new();
        let mut output = ctx.run_ui(RawInput::default(), |ui| {
            let job = highlight_with(
                ui,
                "fix Fix FIX",
                &search,
                FontId::proportional(12.0),
                Color32::WHITE,
            );
            for section in &job.sections {
                let range = section.byte_range.start.0..section.byte_range.end.0;
                if section.format.background != Color32::TRANSPARENT {
                    marked.push(job.text[range].to_owned());
                }
            }
        });
        output.textures_delta.clear();
        assert_eq!(marked, vec!["fix".to_owned()], "实际标了 {marked:?}");
    }
}

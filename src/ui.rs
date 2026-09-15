//! 各处共用的绘制小工具。`main.rs` 把它们 `pub use` 出去，所以其它模块照旧写 `crate::ink`。

use egui::text::LayoutJob;
use egui::{Color32, FontId, TextFormat, Ui};

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
    let mark = TextFormat {
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
    };
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

    #[test]
    fn multibyte_needle_splits_on_char_boundaries() {
        // 前后夹着中文，确认按字节匹配不会把 UTF-8 序列切坏（切坏会直接 panic）
        let (plain, marks) = split(false, "修复 药品明细 保存 药品", "药品");
        assert_eq!(marks, vec!["药品", "药品"]);
        assert_eq!(plain, "修复 明细 保存 ");
    }
}

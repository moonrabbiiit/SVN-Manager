//! Regression test for the side-by-side diff rows rendered inside
//! `RepoApp::file_diff_window`: every row is
//! [old line no][old text cell][new line no][new text cell], and hunk headers
//! (`@@ -461,7 +461,7 @@`) are whole-width rows of their own.
//!
//! The assertions look at the text egui actually *painted* (`FullOutput::shapes`),
//! not at the rects the layout handed back.  That distinction is the whole point:
//! the numbers used to be painted by hand with `slot.max` as the anchor, which is the
//! *bottom* of the number cell -- every line number therefore landed one row lower than
//! its code, and the last number of a hunk was drawn straight through the blue `@@`
//! header below it.  Rect-based assertions stayed green while the window looked broken.
//!
//! Pinned down here:
//! * a line number is painted on its own code line (same top edge), even when that code
//!   line wraps;
//! * no two painted pieces of text overlap -- a header row must not be printed over;
//! * the text cells are laid out with `allocate_ui_with_layout(vec2(col, 0.0), ..)`,
//!   which asks for zero height and must grow to fit its content, otherwise every row
//!   collapses onto the previous one;
//! * a child UI only reports the width it actually *used*, so each cell has to pad up to
//!   `col`, otherwise the right column drifts with the length of the line above it.

use egui::{
    Align, Context, Event, FontId, Layout, Pos2, RawInput, Rect, RichText, ScrollArea,
    TextWrapMode, Vec2,
};

/// Width of one diff column, mirroring the `col` computed in the window.
const COL: f32 = 260.0;

/// One rendered line: either a left/right pair (with its own line numbers) or a
/// whole-width hint row such as a `@@` hunk header.
enum Line {
    Pair(String, String),
    Span(String),
}

fn input() -> RawInput {
    RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 900.0))),
        focused: true,
        events: vec![Event::PointerMoved(Pos2::new(500.0, 500.0))],
        ..Default::default()
    }
}

/// A context line, a removal paired with an addition, a hunk header, and a 600-char line.
fn lines() -> Vec<Line> {
    vec![
        Line::Pair("context old".to_owned(), "context new".to_owned()),
        Line::Pair("removed line".to_owned(), "added line".to_owned()),
        Line::Span("@@ -461,7 +461,7 @@".to_owned()),
        Line::Pair(String::new(), "x".repeat(600)),
    ]
}

/// A piece of text and where it ended up on screen.
#[derive(Debug, Clone)]
struct Painted {
    text: String,
    rect: Rect,
}

/// Lays out `lines` the way the diff window does and returns every painted text.
fn pass(ctx: &Context, lines: &[Line]) -> Vec<Painted> {
    let mut full = ctx.run_ui(input(), |ui| {
        {
            ScrollArea::vertical()
                .id_salt("file_diff_y")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for (index, line) in lines.iter().enumerate() {
                        match line {
                            Line::Span(text) => {
                                ui.monospace(RichText::new(text.as_str()).size(12.0));
                            }
                            Line::Pair(left, right) => {
                                ui.horizontal_top(|ui| {
                                    for (marker, text) in [
                                        (format!("L{index}"), left.as_str()),
                                        (format!("R{index}"), right.as_str()),
                                    ] {
                                        let weak = ui.visuals().weak_text_color();
                                        let galley = ui.fonts_mut(|fonts| {
                                            fonts.layout_no_wrap(
                                                marker,
                                                FontId::monospace(12.0),
                                                weak,
                                            )
                                        });
                                        ui.add_sized(
                                            Vec2::new(44.0, galley.size().y),
                                            egui::Label::new(galley).halign(Align::Max),
                                        );
                                        let cell = ui.allocate_ui_with_layout(
                                            Vec2::new(COL, 0.0),
                                            Layout::top_down(Align::Min),
                                            |ui| {
                                                ui.add(
                                                    egui::Label::new(
                                                        RichText::new(if text.is_empty() {
                                                            " "
                                                        } else {
                                                            text
                                                        })
                                                        .monospace()
                                                        .size(12.0),
                                                    )
                                                    .wrap_mode(TextWrapMode::Wrap)
                                                    .selectable(true),
                                                );
                                            },
                                        );
                                        // 子 UI 只按实际用掉的宽度回报，短行要补齐，否则右列会跑位
                                        let pad = COL - cell.response.rect.width();
                                        if pad > 0.0 {
                                            ui.add_space(pad);
                                        }
                                    }
                                });
                            }
                        }
                    }
                });
        }
    });
    let mut painted: Vec<Painted> = full
        .shapes
        .iter()
        .filter_map(|clipped| match &clipped.shape {
            egui::Shape::Text(text) => Some(Painted {
                text: text.galley.text().to_owned(),
                rect: Rect::from_min_size(text.pos, text.galley.size()),
            }),
            _ => None,
        })
        .collect();
    painted.sort_by_key(|item| (item.rect.min.y as i64, item.rect.min.x as i64));
    // 测试不渲染纹理，显式丢弃字体增量，否则 epaint 在 Drop 时按未应用增量 panic
    full.textures_delta.clear();
    painted
}

fn only(painted: &[Painted], text: &str) -> Rect {
    let found: Vec<Rect> = painted
        .iter()
        .filter(|item| item.text == text)
        .map(|item| item.rect)
        .collect();
    assert_eq!(found.len(), 1, "expected exactly one painted {text:?}, got {found:?}");
    found[0]
}

#[test]
fn every_line_number_sits_on_its_own_code_line() {
    let ctx = Context::default();
    let painted = pass(&ctx, &lines());
    for (index, (number, code)) in [
        ("L0", "context old"),
        ("R0", "context new"),
        ("L1", "removed line"),
        ("R1", "added line"),
        ("L3", " "),
        ("R3", &"x".repeat(600)),
    ]
    .into_iter()
    .enumerate()
    {
        let number = only(&painted, number);
        let code = only(&painted, code);
        assert!(
            (number.min.y - code.min.y).abs() < 0.5,
            "row {index}: line number {number:?} is not painted on its code line {code:?}"
        );
        assert!(
            number.max.y <= code.max.y + 0.5,
            "row {index}: line number {number:?} hangs below its code line {code:?}"
        );
    }
}

#[test]
fn nothing_is_painted_over_anything_else() {
    let ctx = Context::default();
    let painted = pass(&ctx, &lines());
    for (index, first) in painted.iter().enumerate() {
        for second in painted.iter().skip(index + 1) {
            let overlap = first.rect.intersect(second.rect);
            assert!(
                overlap.width() < 1.0 || overlap.height() < 1.0,
                "{:?} is drawn on top of {:?} (overlap {overlap:?})",
                first.text,
                second.text
            );
        }
    }
    // 行号压在蓝色 @@ 表头上，就是「表头挡住行号」的那个现象
    let header = only(&painted, "@@ -461,7 +461,7 @@");
    assert!(
        painted
            .iter()
            .filter(|item| item.text.starts_with('L') || item.text.starts_with('R'))
            .all(|item| item.rect.intersect(header).height() < 1.0),
        "a line number is painted through the hunk header {header:?}"
    );
}

#[test]
fn rows_stack_downwards_instead_of_collapsing() {
    let ctx = Context::default();
    let painted = pass(&ctx, &lines());
    let mut previous: Option<Rect> = None;
    // 自上而下：第 0 行、第 1 行、@@ 表头、第 3 行的行号
    for text in ["context old", "removed line", "@@ -461,7 +461,7 @@", "L3"] {
        let rect = only(&painted, text);
        assert!(rect.height() >= 12.0, "{text:?} collapsed: {rect:?}");
        if let Some(previous) = previous {
            assert!(
                rect.min.y >= previous.max.y - 0.5,
                "{text:?} is drawn on top of the previous row: {rect:?} vs {previous:?}"
            );
        }
        previous = Some(rect);
    }
}

#[test]
fn columns_start_at_the_same_place_on_every_row() {
    let ctx = Context::default();
    let painted = pass(&ctx, &lines());
    let (first_left, first_right) = (only(&painted, "context old"), only(&painted, "context new"));
    for text in ["removed line", "added line"] {
        let rect = only(&painted, text);
        let expected = if rect.min.x < first_right.min.x { first_left } else { first_right };
        assert!(
            (rect.min.x - expected.min.x).abs() < 0.5,
            "{text:?} drifted: {rect:?} vs {expected:?}"
        );
    }
    // 行号栏右边缘各行一致，数字才不会左右跳（第 3 行是 @@ 表头，没有行号）
    for index in [1, 3] {
        for side in ['L', 'R'] {
            let marker = format!("{side}{index}");
            let rect = only(&painted, &marker);
            let anchor = only(&painted, &format!("{side}0"));
            assert!(
                (rect.max.x - anchor.max.x).abs() < 0.5,
                "{marker} line number column drifted: {rect:?} vs {anchor:?}"
            );
        }
    }
    assert!(
        first_right.min.x - first_left.min.x > COL,
        "right column should start after a whole {COL}px cell: {first_left:?} {first_right:?}"
    );
}

#[test]
fn long_lines_wrap_inside_their_column() {
    let ctx = Context::default();
    let painted = pass(&ctx, &lines());
    let long = only(&painted, &"x".repeat(600));
    let plain = only(&painted, "context new");
    assert!(
        long.width() <= COL + 1.0,
        "a 600-char line must wrap inside its column, not widen it: {long:?}"
    );
    assert!(
        long.height() > 3.0 * plain.height(),
        "a wrapped line should need several text lines: {long:?} vs {plain:?}"
    );
}
//! Regression test for the hover-only 「查看提交记录」 button on the rows of the
//! 「涉及文件」 list in `RepoApp::history_page`.
//!
//! The button is placed with `Ui::put` *after* the row has been laid out, so three
//! things can silently break: it can land outside the row (unreachable), the hover
//! test can be satisfied by only part of the row (egui's `Response::hovered()` is
//! exclusive, so a label inside the row steals it and the button never shows), or
//! reserving the button slot can shift the row height. The slot is always reserved,
//! and visibility follows the pointer being anywhere inside the row rectangle.

use egui::{
    Button, Context, Event, Modifiers, PointerButton, Pos2, RawInput, Rect, RichText, ScrollArea,
    Sense, Ui, Vec2,
};

fn input(events: Vec<Event>) -> RawInput {
    RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 700.0))),
        focused: true,
        events,
        ..Default::default()
    }
}

#[derive(Clone, Copy)]
struct Frame {
    row: Rect,
    label: Rect,
    button: Option<Rect>,
    clicked: bool,
}

/// One frame mirroring a 「涉及文件」 row.
fn pass(ctx: &Context, events: Vec<Event>) -> Frame {
    let mut out = Frame { row: Rect::NOTHING, label: Rect::NOTHING, button: None, clicked: false };
    let mut output = ctx.run_ui(input(events), |ui: &mut Ui| {
        ScrollArea::vertical()
            .id_salt("history_detail")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let row = ui.horizontal(|ui| {
                    let text = ui.add(
                        egui::Label::new(
                            "code/HRP.FA/config/mapper/asset/assetInstockDetail.xml",
                        )
                        .sense(Sense::click()),
                    );
                    // 行右侧始终留出按钮位，只有悬停时才画按钮
                    let spare = ui.available_width() - 96.0;
                    if spare > 0.0 {
                        ui.add_space(spare);
                    }
                    out.label = text.rect;
                    text
                });
                out.row = row.response.rect;
                let rect = Rect::from_min_size(
                    Pos2::new(row.response.rect.max.x - 94.0, row.response.rect.center().y - 9.0),
                    Vec2::new(90.0, 18.0),
                );
                if ui.rect_contains_pointer(row.response.rect) {
                    let response = ui.put(
                        rect,
                        Button::new(RichText::new("查看提交记录").size(11.5)).small(),
                    );
                    out.clicked = response.clicked();
                    out.button = Some(rect);
                }
            });
    });
    // 测试不渲染纹理，显式丢弃字体增量，否则 epaint 在 Drop 时按未应用增量 panic
    output.textures_delta.clear();
    out
}

/// Lay out one frame so we know where the row ended up.
fn warm(ctx: &Context) -> Frame {
    pass(ctx, vec![])
}

#[test]
fn button_is_hidden_until_the_row_is_hovered() {
    let ctx = Context::default();
    let frame = warm(&ctx);
    let far = pass(&ctx, vec![Event::PointerMoved(Pos2::new(20.0, frame.row.max.y + 200.0))]);
    assert!(far.button.is_none(), "button shown while the row is not hovered");
    let on_text = pass(&ctx, vec![Event::PointerMoved(frame.label.center())]);
    assert!(on_text.button.is_some(), "button missing while the row is hovered");
}

#[test]
fn button_sits_at_the_right_end_of_its_row() {
    let ctx = Context::default();
    let frame = warm(&ctx);
    let hovered = pass(&ctx, vec![Event::PointerMoved(frame.label.center())]);
    let rect = hovered.button.expect("button drawn while hovering");
    assert!(
        hovered.row.contains(rect.center()),
        "button outside the row: row {:?} button {rect:?}",
        hovered.row
    );
    assert!(
        rect.max.x <= hovered.row.max.x + 1.0,
        "button past the panel edge: button {rect:?} row {:?}",
        hovered.row
    );
    assert!(
        rect.min.x > frame.label.max.x - 1.0,
        "button overlaps the path text: button {rect:?} label {:?}",
        frame.label
    );
}

#[test]
fn button_stays_visible_and_clickable_under_the_pointer() {
    let ctx = Context::default();
    let frame = warm(&ctx);
    let shown = pass(&ctx, vec![Event::PointerMoved(frame.label.center())]);
    let rect = shown.button.expect("button drawn while hovering");
    // 指针从路径移到按钮上：按钮不能消失，否则永远点不中
    let on_button = pass(
        &ctx,
        vec![
            Event::PointerMoved(rect.center()),
            Event::PointerButton {
                pos: rect.center(),
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: rect.center(),
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
    );
    assert!(on_button.button.is_some(), "button vanished when the pointer moved onto it");
    assert!(on_button.clicked, "clicking the button did nothing");
}

#[test]
fn hovering_does_not_move_the_row() {
    let ctx = Context::default();
    let frame = warm(&ctx);
    let idle = pass(&ctx, vec![Event::PointerMoved(Pos2::new(20.0, frame.row.max.y + 200.0))]);
    let hovered = pass(&ctx, vec![Event::PointerMoved(frame.label.center())]);
    assert_eq!(idle.row, hovered.row, "row rect changed just because of a hover");
}
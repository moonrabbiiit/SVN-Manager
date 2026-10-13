//! Regression test for the whole-row click area of the directory list:
//! clicking anywhere in a row must select it, while the buttons inside the
//! row (update / commit / history / open folder / more / remove) keep working.
//!
//! `RepoApp::dir_row` registers a click-sensing scope around each row and turns
//! off `selectable_labels` inside it, because egui labels sense clicks by
//! default and would otherwise swallow the row click.

use egui::{
    Align, Context, Event, Frame, Id, Layout, Modifiers, PointerButton, Pos2, RawInput, Rect,
    Sense, Ui, UiBuilder, Vec2,
};

fn input(events: Vec<Event>) -> RawInput {
    RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 900.0))),
        focused: true,
        events,
        ..Default::default()
    }
}

fn click(pos: Pos2) -> Vec<Event> {
    vec![
        Event::PointerMoved(pos),
        Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::NONE,
        },
        Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::NONE,
        },
    ]
}

#[derive(Default)]
struct Hit {
    row: bool,
    update: bool,
    remove: bool,
}

/// One frame of a row that mirrors `RepoApp::dir_row`.
/// Returns (row, label, update button, remove button) rectangles.
fn pass(ctx: &Context, events: Vec<Event>, out: &mut Hit) -> (Rect, Rect, Rect, Rect) {
    let mut row_rect = Rect::NOTHING;
    let mut label_rect = Rect::NOTHING;
    let mut update_rect = Rect::NOTHING;
    let mut remove_rect = Rect::NOTHING;
    let mut output = ctx.run_ui(input(events), |ui: &mut Ui| {
        let scope = ui.scope_builder(
            UiBuilder::new().id(Id::new(("dir_row", 0))).sense(Sense::click()),
            |ui| {
                Frame::new().inner_margin(7.0).show(ui, |ui| {
                    ui.style_mut().interaction.selectable_labels = false;
                    ui.horizontal(|ui| {
                        label_rect = ui.label("D:\\some\\long\\working\\copy\\path").rect;
                        let update = ui.button("update");
                        out.update = update.clicked();
                        update_rect = update.rect;
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let remove = ui.button("remove");
                            out.remove = remove.clicked();
                            remove_rect = remove.rect;
                        });
                    });
                });
            },
        );
        out.row = scope.response.clicked();
        row_rect = scope.response.rect;
    });
    // 测试不渲染纹理，显式丢弃字体增量，否则 epaint 在 Drop 时按未应用增量 panic
    output.textures_delta.clear();
    (row_rect, label_rect, update_rect, remove_rect)
}

#[test]
fn whole_row_is_selectable_without_stealing_button_clicks() {
    let ctx = Context::default();
    let mut warm = Hit::default();
    let (row_rect, label_rect, update_rect, remove_rect) = pass(&ctx, vec![], &mut warm);
    assert!(row_rect.is_positive(), "row rect: {row_rect:?}");
    assert!(row_rect.width() > 900.0, "row should span the width: {row_rect:?}");
    assert!(row_rect.contains(label_rect.center()), "label outside row");
    assert!(row_rect.contains(update_rect.center()), "button outside row");
    assert!(row_rect.contains(remove_rect.center()), "button outside row");

    for (name, pos, expect_row) in [
        ("frame padding", Pos2::new(row_rect.min.x + 2.0, row_rect.center().y), true),
        ("over a label", label_rect.center(), true),
        ("update button", update_rect.center(), false),
        ("remove button", remove_rect.center(), false),
    ] {
        let mut hit = Hit::default();
        pass(&ctx, click(pos), &mut hit);
        assert_eq!(hit.row, expect_row, "{name}: wrong row selection");
        assert!(!(hit.update && hit.remove), "{name}: two buttons fired at once");
        if expect_row {
            assert!(!hit.update && !hit.remove, "{name}: fired a button");
        } else {
            assert!(hit.update || hit.remove, "{name}: no button fired");
        }
    }
}

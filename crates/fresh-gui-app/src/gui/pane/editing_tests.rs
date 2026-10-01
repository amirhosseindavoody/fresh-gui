use super::{AdeCmd, AdeHandle, EditSync, EditorPanel, TabStripMetrics};
use crate::gui::{
    commands::{CommandContext, CommandRegistry},
    connect::parse_connect_target,
    editing_commands,
    workspace::Workspace,
};
use fresh_gui_protocol::{CAP_EDITOR_SMART_EDITING, EditorAction};
use gpui::TestAppContext;
use gpui::{AppContext, Context, Focusable, Window};

fn editor_panel(window: &mut Window, cx: &mut Context<EditorPanel>, ade: AdeHandle) -> EditorPanel {
    let workspace =
        cx.new(|cx| Workspace::new_for_test(parse_connect_target("ws://", None), window, cx));
    EditorPanel::new(
        "buffer".into(),
        "test.txt".into(),
        None,
        None,
        None,
        true,
        None,
        ade,
        workspace.downgrade(),
        TabStripMetrics::default(),
        window,
        cx,
    )
}

#[gpui::test]
fn native_carets_type_unicode_and_tabs_across_accepted_acknowledgements(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (ade, _) = AdeHandle::test_channel();
    let (panel, test_cx) = cx.add_window_view(|window, cx| editor_panel(window, cx, ade));

    panel.update_in(test_cx, |panel, window, cx| {
        panel.configure_range_edits(true, cx);
        panel.edit_sync = Some(EditSync::new("ab\ncd".into(), 4));
        panel.rev = 4;
        panel.editor.update(cx, |editor, cx| {
            editor.set_value("ab\ncd", window, cx);
            editor.set_selected_range(1..1, cx);
            editor.focus(window, cx);
        });
        window.refresh();
    });
    test_cx.run_until_parked();
    panel.update_in(test_cx, |panel, window, cx| {
        panel.editor.read(cx).focus_handle(cx).dispatch_action(
            &gpui_kit::base::input::AddCursorBelow,
            window,
            cx,
        );
    });

    test_cx.simulate_input("猫");
    panel.update_in(test_cx, |panel, window, cx| {
        let draft = panel.current_text(cx).to_string();
        assert_eq!(draft, "a猫b\nc猫d");

        // An accepted ordinary range-edit acknowledgement must not restore the
        // scalar primary selection over the native editor's complete caret set.
        let selection = panel.byte_selection(cx);
        panel.edit_request_id = Some("ordinary-edit".into());
        panel.edit_sent_selection = Some(selection);
        panel.apply_edit_result(
            "ordinary-edit",
            &panel.view_id.clone(),
            5,
            draft.clone(),
            selection,
            true,
            true,
            window,
            cx,
        );
    });

    test_cx.simulate_keystrokes("tab");
    test_cx.simulate_input("x");
    panel.read_with(test_cx, |panel, cx| {
        let text = panel.current_text(cx).to_string();
        assert_eq!(text.matches("猫  x").count(), 2, "{text:?}");
    });
}

#[gpui::test]
fn fresh_action_waits_for_draft_ack_and_old_daemons_hide_it(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (ade, commands) = AdeHandle::test_channel();
    let (panel, test_cx) = cx.add_window_view(|window, cx| editor_panel(window, cx, ade));

    panel.update_in(test_cx, |panel, window, cx| {
        panel.configure_range_edits(true, cx);
        panel.edit_sync = Some(EditSync::new("before".into(), 8));
        panel.rev = 8;
        panel.editor.update(cx, |editor, cx| {
            editor.set_value("after", window, cx);
        });

        assert!(panel.request_editor_action(EditorAction::SmartHome, cx));
    });

    let (request_id, view_id, text, selection) = match commands.try_recv().unwrap() {
        AdeCmd::RangeEdit {
            request_id,
            view_id,
            edits,
            selection,
            ..
        } => {
            assert_eq!(edits.len(), 1);
            (request_id, view_id, "after".to_string(), selection)
        }
        command => panic!("draft range edit must precede Fresh action: {command:?}"),
    };

    panel.update_in(test_cx, |panel, window, cx| {
        panel.apply_edit_result(
            &request_id,
            &view_id,
            9,
            text,
            selection,
            true,
            true,
            window,
            cx,
        );
    });
    assert!(matches!(
        commands.try_recv().unwrap(),
        AdeCmd::ActionBuffer {
            action: EditorAction::SmartHome,
            ..
        }
    ));

    let registry = CommandRegistry::with_builtins();
    let old_daemon = registry.available(Some(CommandContext::Editor), &[]);
    assert!(
        editing_commands::descriptors()
            .iter()
            .filter(|descriptor| descriptor.capability.as_deref() == Some(CAP_EDITOR_SMART_EDITING))
            .all(|descriptor| !old_daemon
                .iter()
                .any(|available| available.id == descriptor.id))
    );
}

#[gpui::test]
fn rectangular_selection_types_across_acknowledgements(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, point, px};

    cx.update(gpui_kit::init);
    let (ade, _) = AdeHandle::test_channel();
    let (panel, test_cx) = cx.add_window_view(|window, cx| editor_panel(window, cx, ade));

    panel.update_in(test_cx, |panel, window, cx| {
        panel.configure_range_edits(true, cx);
        panel.edit_sync = Some(EditSync::new("abcd\nabcd\nabcd".into(), 4));
        panel.rev = 4;
        panel.editor.update(cx, |editor, cx| {
            editor.set_value("abcd\nabcd\nabcd", window, cx);
            editor.focus(window, cx);
        });
        window.refresh();
    });
    test_cx.run_until_parked();

    // GPUI's native Alt+Shift drag makes one selection per row. Derive the
    // pointer positions from public rendered-range bounds so this exercises
    // the actual editor hit testing rather than mutating hidden cursor state.
    let (start, end) = panel.read_with(test_cx, |panel, cx| {
        let editor = panel.editor.read(cx);
        let start = editor
            .range_to_bounds(&(1..2))
            .expect("first row is laid out");
        let end = editor
            .range_to_bounds(&(12..13))
            .expect("third row is laid out");
        (
            point(
                start.origin.x + px(1.),
                start.origin.y + start.size.height / 2.,
            ),
            point(
                end.origin.x + end.size.width - px(1.),
                end.origin.y + end.size.height / 2.,
            ),
        )
    });
    let modifiers = Modifiers {
        alt: true,
        shift: true,
        ..Default::default()
    };
    test_cx.simulate_mouse_down(start, MouseButton::Left, modifiers);
    test_cx.simulate_mouse_move(end, MouseButton::Left, modifiers);
    test_cx.simulate_mouse_up(end, MouseButton::Left, modifiers);

    test_cx.simulate_input("猫");
    panel.update_in(test_cx, |panel, window, cx| {
        let draft = panel.current_text(cx).to_string();
        assert_eq!(draft, "a猫d\na猫d\na猫d");

        // The ordinary accepted range-edit acknowledgement must leave the
        // native rectangular caret set available for the next coordinated edit.
        let selection = panel.byte_selection(cx);
        panel.edit_request_id = Some("rectangular-edit".into());
        panel.edit_sent_selection = Some(selection);
        panel.apply_edit_result(
            "rectangular-edit",
            &panel.view_id.clone(),
            5,
            draft,
            selection,
            true,
            true,
            window,
            cx,
        );
    });
    test_cx.simulate_input("x");
    panel.read_with(test_cx, |panel, cx| {
        assert_eq!(panel.current_text(cx).to_string(), "a猫xd\na猫xd\na猫xd");
    });
}

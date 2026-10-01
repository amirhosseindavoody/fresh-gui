use super::*;
use core::prelude::v1::test;
use gpui::TestAppContext;

fn panel(cx: &mut TestAppContext) -> (Entity<EditorPanel>, &mut VisualTestContext) {
    cx.update(gpui_kit::init);
    let (workspace, cx) = cx.add_window_view(|window, cx| {
        crate::gui::workspace::Workspace::new_for_test(
            crate::gui::connect::parse_connect_target("ws://", None),
            window,
            cx,
        )
    });
    let (ade, _) = AdeHandle::test_channel();
    let panel = cx.update(|window, cx| {
        cx.new(|cx| {
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
        })
    });
    (panel, cx)
}

fn diagnostic() -> BufferDiagnostic {
    BufferDiagnostic {
        start_line: 0,
        start_character: 3,
        end_line: 0,
        end_character: 4,
        severity: "error".into(),
        message: "bad".into(),
        source: Some("TY".into()),
        related_information: Vec::new(),
    }
}

#[gpui::test]
fn diagnostics_decorate_current_unicode_snapshot_and_clear_on_local_edit(cx: &mut TestAppContext) {
    let (panel, cx) = panel(cx);
    panel.update_in(cx, |panel, window, cx| {
        panel.set_editor_text_and_selection("a😀b", None, window, cx);
        panel.configure_range_edits(true, cx);
        panel.edit_sync = Some(EditSync::new("a😀b".into(), 4));
        panel.rev = 4;
        panel.set_lsp_state(4, None, vec![diagnostic()], None, window, cx);
        assert_eq!(panel.problems().len(), 1);
        let mark = panel
            .editor
            .read(cx)
            .diagnostics()
            .unwrap()
            .iter()
            .next()
            .unwrap();
        assert_eq!(mark.range, 5..6);
        panel.set_editor_text_and_selection("local", None, window, cx);
        panel.on_lsp_text_change(cx);
        assert!(panel.problems().is_empty());
        assert!(panel.editor.read(cx).diagnostics().unwrap().is_empty());
        panel.set_lsp_state(4, None, vec![diagnostic()], None, window, cx);
        assert!(
            panel.problems().is_empty(),
            "stale offsets cannot decorate newer drafts"
        );
    });
}

#[gpui::test]
fn format_on_save_installs_snapshot_and_protects_newer_local_draft(cx: &mut TestAppContext) {
    let (panel, cx) = panel(cx);
    panel.update_in(cx, |panel, window, cx| {
        panel.set_editor_text_and_selection("bad", None, window, cx);
        panel.edit_sync = Some(EditSync::new("bad".into(), 4));
        panel.rev = 4;
        panel.save_request_id = Some("save".into());
        panel.save_sent_text = Some("bad".into());
        panel.finish_save(
            "save",
            "test.txt".into(),
            5,
            fresh_gui_protocol::SaveOutcome {
                text: Some("good".into()),
                ..Default::default()
            },
            window,
            cx,
        );
        assert_eq!(panel.current_text(cx), "good");
        assert!(!panel.dirty);
        assert_eq!(
            panel.edit_sync.as_ref().unwrap().acknowledged(),
            ("good", 5)
        );
        panel.save_request_id = Some("save-2".into());
        panel.save_sent_text = Some("good".into());
        panel.set_editor_text_and_selection("local change", None, window, cx);
        panel.finish_save(
            "save-2",
            "test.txt".into(),
            6,
            fresh_gui_protocol::SaveOutcome {
                text: Some("formatted".into()),
                ..Default::default()
            },
            window,
            cx,
        );
        assert_eq!(panel.current_text(cx), "local change");
        assert!(panel.dirty);
        assert!(
            panel.conflict,
            "overlapping save formatting must require review"
        );
    });
}

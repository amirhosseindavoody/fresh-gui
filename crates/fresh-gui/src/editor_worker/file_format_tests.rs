use super::*;
use fresh_gui_protocol::{BufferFileControl, FileControlOperation};

fn encoded(text: &str, encoding: Encoding) -> Vec<u8> {
    let mut bytes = encoding.bom_bytes().unwrap_or_default().to_vec();
    bytes.extend(fresh::model::encoding::convert_from_utf8(
        text.as_bytes(),
        encoding,
    ));
    bytes
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

async fn control(
    editor: &EditorHandle,
    id: &str,
    rev: u64,
    operation: FileControlOperation,
) -> fresh_gui_protocol::BufferFileState {
    editor
        .file_control(BufferFileControl {
            request_id: "fixture".into(),
            buffer_id: id.into(),
            base_rev: rev,
            operation,
        })
        .await
        .unwrap()
}

#[test]
fn encoded_crlf_fixtures_round_trip_edits_save_as_and_eol_conversion() {
    let root =
        std::env::temp_dir().join(format!("fresh-format-roundtrip-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let editor = EditorHandle::spawn(root.clone(), crate::config::Config::default()).unwrap();
    runtime().block_on(async {
        for (index, encoding) in [
            Encoding::Utf8,
            Encoding::Utf8Bom,
            Encoding::Utf16Le,
            Encoding::Utf16Be,
            Encoding::Windows1251,
        ]
        .into_iter()
        .enumerate()
        {
            let path = root.join(format!("fixture-{index}.txt"));
            std::fs::write(&path, encoded("Привет\r\nмир\r\n", encoding)).unwrap();
            let opened = editor.open(path.clone(), false).await.unwrap();
            assert_eq!(opened.text, "Привет\nмир\n");
            let state = control(
                &editor,
                &opened.buffer_id,
                opened.rev,
                FileControlOperation::Inspect,
            )
            .await;
            assert_eq!(state.metadata.encoding, encoding.display_name());
            assert_eq!(state.metadata.bom, encoding.has_bom());
            assert_eq!(state.metadata.line_ending, "CRLF");
            let rev = editor
                .edit(
                    opened.buffer_id.clone(),
                    state.rev,
                    "Привет\nизменение\n".into(),
                )
                .await
                .unwrap();
            let saved = editor
                .save_with_actions(opened.buffer_id.clone(), rev, None, true)
                .await
                .unwrap();
            assert!(
                !saved.outcome.dirty,
                "{} remains clean after encoded save",
                encoding.display_name()
            );
            assert_eq!(
                std::fs::read(&path).unwrap(),
                encoded("Привет\r\nизменение\r\n", encoding)
            );
            let state = control(
                &editor,
                &opened.buffer_id,
                saved.rev,
                FileControlOperation::SetLineEnding {
                    line_ending: "LF".into(),
                },
            )
            .await;
            let copy = root.join(format!("copy-{index}.txt"));
            let saved = editor
                .save_with_actions(
                    opened.buffer_id.clone(),
                    state.rev,
                    Some(copy.clone()),
                    true,
                )
                .await
                .unwrap();
            assert!(!saved.outcome.dirty);
            assert_eq!(
                std::fs::read(copy).unwrap(),
                encoded("Привет\nизменение\n", encoding)
            );
        }
    });
    drop(editor);
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn read_only_save_retains_draft_and_save_as_preserves_source() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("fresh-readonly-save-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("source.txt");
    std::fs::write(&path, "source\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    let editor = EditorHandle::spawn(root.clone(), crate::config::Config::default()).unwrap();
    runtime().block_on(async {
        let opened = editor.open(path.clone(), false).await.unwrap();
        let state = control(
            &editor,
            &opened.buffer_id,
            opened.rev,
            FileControlOperation::Inspect,
        )
        .await;
        assert!(state.metadata.read_only);
        let rev = editor
            .edit(opened.buffer_id.clone(), state.rev, "draft\n".into())
            .await
            .unwrap();
        assert!(
            editor
                .save(opened.buffer_id.clone(), rev, None)
                .await
                .unwrap_err()
                .to_string()
                .contains("read-only")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "source\n");
        assert_eq!(
            editor.draft_list("default".into()).await.unwrap()[0].text,
            "draft\n"
        );
        let copy = root.join("copy.txt");
        editor
            .save(opened.buffer_id, rev, Some(copy.clone()))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(copy).unwrap(), "draft\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "source\n");
        assert!(
            editor
                .draft_list("default".into())
                .await
                .unwrap()
                .is_empty()
        );
    });
    drop(editor);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn save_cleanup_precedes_formatter_and_legacy_formatter_is_explicitly_skipped() {
    if !fresh::services::lsp::command_exists("python3") {
        return;
    }
    let root = std::env::temp_dir().join(format!("fresh-save-order-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let formatter = root.join("formatter.py");
    std::fs::write(
        &formatter,
        "import sys\nsys.stdout.write(sys.stdin.read().replace('bad\\n', 'good\\n'))\n",
    )
    .unwrap();
    let config = crate::config::Config::parse(&serde_json::json!({
        "editor": {"trim_trailing_whitespace_on_save": true, "ensure_final_newline_on_save": true},
        "languages": {"python": {"formatter": {"command": "python3", "args": [formatter.display().to_string()], "stdin": true}, "format_on_save": true}}
    }).to_string()).unwrap();
    let editor = EditorHandle::spawn(root.clone(), config).unwrap();
    runtime().block_on(async {
        for (index, encoding, expected) in [
            (0, Encoding::Utf8Bom, "good\n"),
            (1, Encoding::Windows1251, "bad\n"),
        ] {
            let path = root.join(format!("source-{index}.py"));
            std::fs::write(&path, "original\n").unwrap();
            let opened = editor.open(path.clone(), false).await.unwrap();
            let state = control(
                &editor,
                &opened.buffer_id,
                opened.rev,
                FileControlOperation::SetEncoding {
                    encoding: encoding.display_name().into(),
                },
            )
            .await;
            let rev = editor
                .edit(opened.buffer_id.clone(), state.rev, "bad  ".into())
                .await
                .unwrap();
            let saved = editor
                .save_with_actions(opened.buffer_id, rev, None, true)
                .await
                .unwrap();
            assert_eq!(saved.outcome.text.as_deref(), Some(expected));
            assert_eq!(std::fs::read(path).unwrap(), encoded(expected, encoding));
            if encoding == Encoding::Windows1251 {
                assert!(
                    saved
                        .outcome
                        .status
                        .unwrap()
                        .contains("formatter was skipped")
                );
            }
        }
    });
    drop(editor);
    let _ = std::fs::remove_dir_all(root);
}

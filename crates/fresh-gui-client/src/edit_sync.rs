//! Small host-side state machine for revisioned editor synchronization.
//!
//! All offsets returned by this module are UTF-8 byte offsets. LSP uses UTF-16
//! columns, so the conversion helpers are intentionally explicit.

use fresh_gui_protocol::{ByteSelection, RangeEdit};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictSnapshot {
    pub base_rev: u64,
    pub base_text: String,
    pub rev: u64,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotReconciliation {
    /// Local draft matched the prior base or the authoritative snapshot.
    Adopted,
    /// The snapshot did not conflict with local work; send the retained draft.
    KeepDraft,
    /// Both sides changed from the same base; caller retains its draft and
    /// exposes `conflict()` for explicit resolution.
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditSync {
    acknowledged_text: String,
    revision: u64,
    sent_text: Option<String>,
    conflict: Option<ConflictSnapshot>,
}

impl EditSync {
    pub fn new(text: String, revision: u64) -> Self {
        Self {
            acknowledged_text: text,
            revision,
            sent_text: None,
            conflict: None,
        }
    }

    /// Initialize a view whose first authoritative snapshot has arrived.
    /// Typing before the snapshot gives us no shared base, so preserve both
    /// versions for explicit resolution instead of treating the draft as a
    /// replacement for text the user has not seen yet.
    pub fn from_initial_snapshot(
        text: String,
        revision: u64,
        draft: &str,
        has_local_changes: bool,
    ) -> (Self, SnapshotReconciliation) {
        if has_local_changes && draft != text {
            let mut sync = Self::new(String::new(), 0);
            sync.finish(false, revision, text);
            (sync, SnapshotReconciliation::Conflict)
        } else {
            (Self::new(text, revision), SnapshotReconciliation::Adopted)
        }
    }

    pub fn acknowledged(&self) -> (&str, u64) {
        (&self.acknowledged_text, self.revision)
    }
    pub fn conflict(&self) -> Option<&ConflictSnapshot> {
        self.conflict.as_ref()
    }
    pub fn is_in_flight(&self) -> bool {
        self.sent_text.is_some()
    }
    pub fn sent_text(&self) -> Option<&str> {
        self.sent_text.as_deref()
    }

    /// Produce one contiguous edit from the latest acknowledged text to the
    /// draft. At most one request may be outstanding; later keystrokes remain
    /// in the caller's draft and are diffed after the acknowledgement.
    pub fn begin_edit(&mut self, draft: &str) -> Option<(u64, Vec<RangeEdit>)> {
        if self.sent_text.is_some() || self.conflict.is_some() {
            return None;
        }
        let edits = contiguous_diff(&self.acknowledged_text, draft);
        if edits.is_empty() {
            return None;
        }
        self.sent_text = Some(draft.to_owned());
        Some((self.revision, edits))
    }

    pub fn finish(&mut self, accepted: bool, revision: u64, authoritative_text: String) {
        let base_text = self.acknowledged_text.clone();
        let base_rev = self.revision;
        self.sent_text = None;
        if accepted {
            self.acknowledged_text = authoritative_text;
            self.revision = revision;
            self.conflict = None;
        } else {
            self.conflict = Some(ConflictSnapshot {
                base_rev,
                base_text,
                rev: revision,
                text: authoritative_text,
            });
        }
    }

    /// Explicit user resolution: accept the saved server head as the next
    /// edit base. The caller chooses whether to keep its draft or install the
    /// returned text; no text is discarded by this method.
    pub fn resolve_conflict(&mut self) -> Option<ConflictSnapshot> {
        let snapshot = self.conflict.take()?;
        self.acknowledged_text = snapshot.text.clone();
        self.revision = snapshot.rev;
        self.sent_text = None;
        Some(snapshot)
    }

    /// Reconcile a server snapshot without discarding local work. The caller
    /// keeps `draft` unchanged in every outcome; a divergent snapshot is saved
    /// with the old base and new server version for explicit resolution.
    pub fn reconcile_snapshot(
        &mut self,
        revision: u64,
        authoritative_text: String,
        draft: &str,
    ) -> SnapshotReconciliation {
        let base_text = self.acknowledged_text.clone();
        let base_rev = self.revision;
        let sent_text = self.sent_text.take();

        if draft == base_text || draft == authoritative_text {
            self.acknowledged_text = authoritative_text;
            self.revision = revision;
            self.conflict = None;
            return SnapshotReconciliation::Adopted;
        }

        if authoritative_text == base_text {
            self.revision = revision;
            self.conflict = None;
            return SnapshotReconciliation::KeepDraft;
        }

        if sent_text.as_deref() == Some(authoritative_text.as_str()) {
            // The request committed but its acknowledgement was lost. Keep
            // any newer local draft and advance its base to the sent text.
            self.acknowledged_text = authoritative_text;
            self.revision = revision;
            self.conflict = None;
            return SnapshotReconciliation::KeepDraft;
        }

        self.conflict = Some(ConflictSnapshot {
            base_rev,
            base_text,
            rev: revision,
            text: authoritative_text,
        });
        SnapshotReconciliation::Conflict
    }
}

pub fn contiguous_diff(old: &str, new: &str) -> Vec<RangeEdit> {
    if old == new {
        return Vec::new();
    }
    let mut prefix = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(prefix) || !new.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let mut old_end = old.len();
    let mut new_end = new.len();
    while old_end > prefix
        && new_end > prefix
        && old.as_bytes()[old_end - 1] == new.as_bytes()[new_end - 1]
    {
        old_end -= 1;
        new_end -= 1;
    }
    while !old.is_char_boundary(old_end) {
        old_end += 1;
    }
    while !new.is_char_boundary(new_end) {
        new_end += 1;
    }
    vec![RangeEdit {
        start: prefix,
        end: old_end,
        text: new[prefix..new_end].to_owned(),
    }]
}

pub fn utf16_column_to_byte(line: &str, column: u32) -> Option<usize> {
    let mut units = 0u32;
    for (byte, ch) in line.char_indices() {
        if units == column {
            return Some(byte);
        }
        units += ch.len_utf16() as u32;
        if units > column {
            return None;
        }
    }
    (units == column).then_some(line.len())
}

pub fn byte_to_utf16_column(line: &str, byte: usize) -> Option<u32> {
    if !line.is_char_boundary(byte) {
        return None;
    }
    Some(line[..byte].encode_utf16().count() as u32)
}

pub fn selection(anchor: usize, head: usize) -> ByteSelection {
    ByteSelection { anchor, head }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_snapshot_preserves_typing_without_a_shared_base() {
        let (clean, outcome) = EditSync::from_initial_snapshot("server".into(), 3, "", false);
        assert_eq!(outcome, SnapshotReconciliation::Adopted);
        assert_eq!(clean.acknowledged(), ("server", 3));
        for draft in ["typed while loading", ""] {
            let (mut sync, outcome) =
                EditSync::from_initial_snapshot("server".into(), 3, draft, true);
            assert_eq!(outcome, SnapshotReconciliation::Conflict);
            assert_eq!(sync.conflict().unwrap().text, "server");
            assert!(sync.begin_edit(draft).is_none());
        }
    }

    #[test]
    fn diff_respects_utf8_boundaries_with_emoji_and_cjk() {
        let edits = contiguous_diff("a🦀你好z", "a🦀世界z");
        assert_eq!(
            edits,
            vec![RangeEdit {
                start: 5,
                end: 11,
                text: "世界".into()
            }]
        );
    }

    #[test]
    fn utf16_columns_convert_around_non_bmp_characters() {
        let line = "a🦀你";
        assert_eq!(utf16_column_to_byte(line, 0), Some(0));
        assert_eq!(utf16_column_to_byte(line, 1), Some(1));
        assert_eq!(utf16_column_to_byte(line, 2), None);
        assert_eq!(utf16_column_to_byte(line, 3), Some(5));
        assert_eq!(byte_to_utf16_column(line, 5), Some(3));
    }

    #[test]
    fn rejected_edit_keeps_local_draft_available_for_rebase() {
        let mut sync = EditSync::new("base".into(), 1);
        let draft = "draft";
        assert!(sync.begin_edit(draft).is_some());
        sync.finish(false, 2, "remote".into());
        assert_eq!(
            sync.conflict(),
            Some(&ConflictSnapshot {
                base_rev: 1,
                base_text: "base".into(),
                rev: 2,
                text: "remote".into()
            })
        );
        assert!(sync.begin_edit(draft).is_none());
        assert_eq!(
            sync.reconcile_snapshot(2, "remote".into(), draft),
            SnapshotReconciliation::Conflict
        );
    }

    #[test]
    fn rapid_changes_wait_for_ack_then_diff_from_acknowledged_text() {
        let mut sync = EditSync::new("base".into(), 4);
        assert!(sync.begin_edit("first").is_some());
        assert_eq!(sync.sent_text(), Some("first"));
        assert!(sync.begin_edit("newer draft").is_none());
        sync.finish(true, 5, "first".into());
        assert_eq!(sync.begin_edit("newer draft").unwrap().0, 5);
    }

    #[test]
    fn snapshot_reconciliation_handles_clean_dirty_lost_ack_and_conflict() {
        let mut clean = EditSync::new("base".into(), 1);
        assert_eq!(
            clean.reconcile_snapshot(2, "server".into(), "base"),
            SnapshotReconciliation::Adopted
        );
        assert_eq!(clean.acknowledged(), ("server", 2));

        let mut dirty = EditSync::new("base".into(), 1);
        assert_eq!(
            dirty.reconcile_snapshot(2, "base".into(), "draft"),
            SnapshotReconciliation::KeepDraft
        );
        assert_eq!(dirty.acknowledged(), ("base", 2));
        assert!(dirty.begin_edit("draft").is_some());

        let mut lost_ack = EditSync::new("base".into(), 1);
        lost_ack.begin_edit("sent");
        assert_eq!(
            lost_ack.reconcile_snapshot(2, "sent".into(), "newer"),
            SnapshotReconciliation::KeepDraft
        );
        assert_eq!(lost_ack.acknowledged(), ("sent", 2));
        assert!(lost_ack.begin_edit("newer").is_some());

        let mut divergent = EditSync::new("base".into(), 1);
        assert_eq!(
            divergent.reconcile_snapshot(2, "server".into(), "draft"),
            SnapshotReconciliation::Conflict
        );
        assert_eq!(divergent.conflict().unwrap().base_text, "base");
        assert_eq!(divergent.conflict().unwrap().text, "server");
        assert!(divergent.begin_edit("draft").is_none());
    }

    #[test]
    fn explicit_keep_draft_conflict_resolution_rebases_to_server_head() {
        let mut sync = EditSync::new("base".into(), 3);
        let draft = "my edits";
        assert!(sync.begin_edit(draft).is_some());
        sync.finish(false, 4, "server edits".into());
        let conflict = sync.resolve_conflict().expect("saved conflict");
        assert_eq!(conflict.base_text, "base");
        assert_eq!((conflict.rev, conflict.text.as_str()), (4, "server edits"));
        assert_eq!(sync.acknowledged(), ("server edits", 4));
        assert!(sync.begin_edit(draft).is_some());
    }

    #[test]
    fn contiguous_diff_reconstructs_diverse_unicode_text() {
        let samples = [
            ("", "🦀"),
            ("🦀", ""),
            ("a你z", "a🦀z"),
            ("αβγ", "α🙂γ"),
            ("👩‍💻x", "👨‍👩‍👧‍👦x"),
            ("same suffix 你好", "new suffix 世界"),
        ];
        for (old, new) in samples {
            let edits = contiguous_diff(old, new);
            assert_eq!(edits.len(), 1, "{old:?} -> {new:?}");
            let edit = &edits[0];
            assert!(old.is_char_boundary(edit.start));
            assert!(old.is_char_boundary(edit.end));
            let mut actual = old.to_owned();
            actual.replace_range(edit.start..edit.end, &edit.text);
            assert_eq!(actual, new, "{old:?} -> {new:?}");
        }
    }
}

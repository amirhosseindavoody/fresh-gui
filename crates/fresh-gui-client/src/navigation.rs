//! Reusable editor location history for definition and symbol jumps.
//!
//! The owner scopes a history to its workspace and authority. Offsets are
//! absolute bytes, including for paged files.

/// A cursor location in an editor view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditorLocation {
    pub path: String,
    /// Optional daemon buffer identity, useful when the same path is reopened.
    pub buffer_id: Option<String>,
    /// Stable identity for the view or pane that initiated the jump.
    pub view_id: String,
    /// Absolute byte offset in the complete file.
    pub offset: usize,
}

impl EditorLocation {
    pub fn new(path: impl Into<String>, view_id: impl Into<String>, offset: usize) -> Self {
        Self { path: path.into(), buffer_id: None, view_id: view_id.into(), offset }
    }

    pub fn with_buffer_id(mut self, buffer_id: impl Into<String>) -> Self {
        self.buffer_id = Some(buffer_id.into());
        self
    }
}

/// Bounded back/forward history. Instantiate per workspace/authority.
#[derive(Clone, Debug)]
pub struct NavigationHistory {
    entries: Vec<EditorLocation>,
    index: Option<usize>,
    capacity: usize,
}

impl Default for NavigationHistory {
    fn default() -> Self { Self::with_capacity(100) }
}

impl NavigationHistory {
    pub fn new() -> Self { Self::default() }

    pub fn with_capacity(capacity: usize) -> Self {
        Self { entries: Vec::new(), index: None, capacity: capacity.max(1) }
    }

    pub fn can_go_back(&self) -> bool { self.index.is_some_and(|index| index > 0) }
    pub fn can_go_forward(&self) -> bool {
        self.index.is_some_and(|index| index + 1 < self.entries.len())
    }
    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }

    /// Record a semantic jump, retaining both its origin and destination.
    /// Jumps made after going back discard the old forward branch.
    pub fn record_jump(&mut self, origin: EditorLocation, destination: EditorLocation) {
        if let Some(index) = self.index {
            self.entries[index] = origin;
            self.entries.truncate(index + 1);
        } else {
            self.entries.push(origin);
            self.index = Some(0);
        }
        if self.entries.last() != Some(&destination) {
            self.entries.push(destination);
        }
        self.index = Some(self.entries.len() - 1);
        self.enforce_capacity();
    }

    /// Move backward, saving the caller's current cursor before leaving it.
    pub fn back(&mut self, current: EditorLocation) -> Option<EditorLocation> {
        let index = self.index?;
        if index == 0 { return None; }
        self.entries[index] = current;
        self.index = Some(index - 1);
        self.entries.get(index - 1).cloned()
    }

    /// Move forward, saving the caller's current cursor before leaving it.
    pub fn forward(&mut self, current: EditorLocation) -> Option<EditorLocation> {
        let index = self.index?;
        if index + 1 >= self.entries.len() { return None; }
        self.entries[index] = current;
        self.index = Some(index + 1);
        self.entries.get(index + 1).cloned()
    }

    fn enforce_capacity(&mut self) {
        if self.entries.len() > self.capacity {
            let remove = self.entries.len() - self.capacity;
            self.entries.drain(..remove);
            self.index = self.index.map(|index| index.saturating_sub(remove));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location(path: &str, offset: usize) -> EditorLocation {
        EditorLocation::new(path, "editor-1", offset)
    }

    #[test]
    fn jump_and_back_forward_preserve_departure_cursor() {
        let mut history = NavigationHistory::new();
        history.record_jump(location("a.rs", 5), location("b.rs", 10));
        assert_eq!(history.back(location("b.rs", 20)), Some(location("a.rs", 5)));
        assert_eq!(history.forward(location("a.rs", 8)), Some(location("b.rs", 20)));
    }

    #[test]
    fn a_new_jump_after_back_truncates_forward_history() {
        let mut history = NavigationHistory::new();
        history.record_jump(location("a", 1), location("b", 2));
        history.record_jump(location("b", 2), location("c", 3));
        assert_eq!(history.back(location("c", 4)), Some(location("b", 2)));
        history.record_jump(location("b", 7), location("d", 9));
        assert_eq!(history.forward(location("d", 9)), None);
        assert_eq!(history.len(), 3);
    }

    #[test]
    fn history_is_bounded_and_keeps_absolute_offsets_and_view_identity() {
        let mut history = NavigationHistory::with_capacity(2);
        history.record_jump(location("a", 1), location("b", 2));
        history.record_jump(location("b", 2), location("c", 99_999));
        assert_eq!(history.len(), 2);
        assert_eq!(history.back(location("c", 100_000)), Some(location("b", 2)));
    }
}

//! Tab-strip geometry and which tabs a close command removes.
//!
//! The dock skin pins `title_suffix` to the far right of the bar. The new-tab
//! button lives there so it stays visible, then shifts left by the empty gap
//! so it sits beside the last tab while the bar still has room.

use std::cell::RefCell;
use std::rc::Rc;

/// Vertical slack for “same tab row”. Side-by-side splits share a top edge
/// and are separated by the button’s anchor instead.
const TAB_ROW_BAND_PX: f32 = 8.0;

/// Space kept between the last tab and the **+**.
const PLUS_GAP_PX: f32 = 4.0;

/// Tab edges collected during one frame, shared by every panel in the dock.
#[derive(Clone, Default)]
pub struct TabStripMetrics {
    edges: Rc<RefCell<Vec<(f32, f32)>>>,
}

impl TabStripMetrics {
    pub fn begin_frame(&self) {
        self.edges.borrow_mut().clear();
    }

    pub fn note_tab(&self, top: f32, right: f32) {
        self.edges.borrow_mut().push((top, right));
    }

    pub fn plus_shift(&self, plus_top: f32, plus_anchor_left: f32) -> f32 {
        plus_shift_px(&self.edges.borrow(), plus_top, plus_anchor_left)
    }
}

/// How far left the **+** should move from its suffix slot.
///
/// `edges` are `(top, right)` of each tab title in window coordinates.
/// `plus_top` / `plus_anchor_left` are the button’s unshifted position (the
/// far-right slot). Tabs that end to the right of that slot are scrolled
/// past it; they do not pull the button into the overflow.
pub fn plus_shift_px(edges: &[(f32, f32)], plus_top: f32, plus_anchor_left: f32) -> f32 {
    let last_right = edges
        .iter()
        .filter(|(top, right)| {
            (top - plus_top).abs() <= TAB_ROW_BAND_PX && *right <= plus_anchor_left + 1.0
        })
        .map(|(_, right)| *right)
        .fold(None, |best: Option<f32>, right| {
            Some(best.map_or(right, |current| current.max(right)))
        });
    let Some(last_right) = last_right else {
        return 0.0;
    };
    (plus_anchor_left - last_right - PLUS_GAP_PX).max(0.0)
}

/// Which other tabs a context-menu close removes. `order` is the center dock’s
/// panel order (tab order inside a group; groups in tree order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabCloseScope {
    Others,
    ToTheRight,
}

pub fn panels_for_close_scope<T: Clone + PartialEq>(
    order: &[T],
    target: &T,
    scope: TabCloseScope,
) -> Vec<T> {
    let Some(ix) = order.iter().position(|id| id == target) else {
        return Vec::new();
    };
    match scope {
        TabCloseScope::Others => order
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != ix)
            .map(|(_, id)| id.clone())
            .collect(),
        TabCloseScope::ToTheRight => order[ix + 1..].to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plus_sits_beside_the_last_tab_when_the_bar_has_room() {
        let edges = [(0.0, 40.0), (0.0, 120.0), (0.0, 200.0)];
        let shift = plus_shift_px(&edges, 0.0, 640.0);
        assert_eq!(shift, 640.0 - 200.0 - PLUS_GAP_PX);
    }

    #[test]
    fn plus_stays_at_the_right_when_tabs_fill_or_overflow() {
        let tight = [(0.0, 80.0), (0.0, 636.0)];
        assert_eq!(plus_shift_px(&tight, 0.0, 640.0), 0.0);

        // The last tab’s layout box is past the suffix; only the visible
        // remainder can pull, and that remainder is already at the edge.
        let overflow = [(0.0, 400.0), (0.0, 636.0), (0.0, 900.0)];
        assert_eq!(plus_shift_px(&overflow, 0.0, 640.0), 0.0);
        assert_eq!(plus_shift_px(&[], 0.0, 640.0), 0.0);
    }

    #[test]
    fn side_by_side_groups_use_their_own_anchor() {
        let edges = [(0.0, 180.0), (0.0, 520.0), (0.0, 700.0)];
        let left = plus_shift_px(&edges, 0.0, 300.0);
        assert_eq!(left, 300.0 - 180.0 - PLUS_GAP_PX);
        let right = plus_shift_px(&edges, 0.0, 760.0);
        assert_eq!(right, 760.0 - 700.0 - PLUS_GAP_PX);
    }

    #[test]
    fn stacked_rows_do_not_share_a_shift() {
        let edges = [(0.0, 200.0), (40.0, 80.0)];
        assert_eq!(
            plus_shift_px(&edges, 40.0, 400.0),
            400.0 - 80.0 - PLUS_GAP_PX
        );
    }

    #[test]
    fn close_scope_matches_tab_order() {
        let order = ["a", "b", "c", "d"];
        assert!(panels_for_close_scope(&order, &"missing", TabCloseScope::Others).is_empty());
        assert_eq!(
            panels_for_close_scope(&order, &"b", TabCloseScope::Others),
            vec!["a", "c", "d"]
        );
        assert_eq!(
            panels_for_close_scope(&order, &"b", TabCloseScope::ToTheRight),
            vec!["c", "d"]
        );
        assert!(panels_for_close_scope(&order, &"d", TabCloseScope::ToTheRight).is_empty());
        assert_eq!(
            panels_for_close_scope(&["only"], &"only", TabCloseScope::Others),
            Vec::<&str>::new()
        );
    }
}

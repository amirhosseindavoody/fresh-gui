//! Client request lifetime tracking, including replies already queued for display.

use std::collections::HashMap;

use fresh_gui_protocol::LspRequestFeature;

/// A received reply can still be superseded before its presentation task runs.
/// Keep the desired ID after receipt, and invalidate it on dismissal or edits.
#[derive(Default)]
pub struct LspRequestTracker {
    desired: HashMap<LspRequestFeature, u64>,
}

impl LspRequestTracker {
    pub fn start(&mut self, feature: LspRequestFeature, request_id: u64) {
        self.desired.insert(feature, request_id);
    }

    pub fn is_current(&self, feature: LspRequestFeature, request_id: u64) -> bool {
        self.desired.get(&feature) == Some(&request_id)
    }

    pub fn cancel(&mut self, feature: LspRequestFeature) {
        self.desired.remove(&feature);
    }

    pub fn clear(&mut self) {
        self.desired.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacing_one_feature_preserves_other_feature_requests() {
        let mut requests = LspRequestTracker::default();
        requests.start(LspRequestFeature::Completion, 1);
        requests.start(LspRequestFeature::Hover, 2);
        requests.start(LspRequestFeature::Completion, 3);
        assert!(!requests.is_current(LspRequestFeature::Completion, 1));
        assert!(requests.is_current(LspRequestFeature::Completion, 3));
        assert!(requests.is_current(LspRequestFeature::Hover, 2));
    }

    #[test]
    fn dismissal_rejects_received_replies_even_when_text_and_caret_are_unchanged() {
        let mut requests = LspRequestTracker::default();
        requests.start(LspRequestFeature::Completion, 1);
        requests.cancel(LspRequestFeature::Completion);
        assert!(!requests.is_current(LspRequestFeature::Completion, 1));
        requests.start(LspRequestFeature::Hover, 2);
        requests.clear();
        assert!(!requests.is_current(LspRequestFeature::Hover, 2));
    }
}

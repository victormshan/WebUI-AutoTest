//! Deciding, from the page's events, how one navigation ended. Pure, so the event orders
//! that matter can be tested without a browser.

use serde_json::Value;

/// What a navigation's events say so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavStep {
    /// Not decided yet.
    Pending,
    /// This navigation's own document fired `load`.
    Loaded,
    /// After this navigation committed, another document took its frame.
    Replaced,
}

/// Follows one navigation, identified by the frame and loaderId `Page.navigate` returned.
///
/// The event subscription starts before `Page.navigate` is sent, so events of an earlier
/// navigation can still arrive first. A `frameNavigated` with another loader therefore only
/// means "replaced" once this navigation has itself committed (its own `frameNavigated`
/// was seen); before that it belongs to an earlier navigation and is ignored. If another
/// navigation wins before this one commits, this one never loads and the caller's timeout
/// reports it (outcome unknown), never a success.
#[derive(Debug)]
pub struct NavWatch {
    frame: String,
    loader: String,
    committed: bool,
}

impl NavWatch {
    pub fn new(frame: &str, loader: &str) -> Self {
        Self {
            frame: frame.into(),
            loader: loader.into(),
            committed: false,
        }
    }

    pub fn on_event(&mut self, method: &str, params: &Value) -> NavStep {
        match method {
            "Page.frameNavigated" if params["frame"]["id"] == self.frame.as_str() => {
                if params["frame"]["loaderId"] == self.loader.as_str() {
                    self.committed = true;
                    NavStep::Pending
                } else if self.committed {
                    NavStep::Replaced
                } else {
                    NavStep::Pending
                }
            }
            "Page.lifecycleEvent"
                if params["frameId"] == self.frame.as_str()
                    && params["loaderId"] == self.loader.as_str()
                    && params["name"] == "load" =>
            {
                NavStep::Loaded
            }
            _ => NavStep::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn navigated(frame: &str, loader: &str) -> (&'static str, Value) {
        (
            "Page.frameNavigated",
            json!({ "frame": { "id": frame, "loaderId": loader } }),
        )
    }

    fn lifecycle(frame: &str, loader: &str, name: &str) -> (&'static str, Value) {
        (
            "Page.lifecycleEvent",
            json!({ "frameId": frame, "loaderId": loader, "name": name }),
        )
    }

    /// The step after feeding all events, stopping at the first decision.
    fn outcome(events: &[(&'static str, Value)]) -> NavStep {
        let mut w = NavWatch::new("F", "L2");
        for (m, p) in events {
            let step = w.on_event(m, p);
            if step != NavStep::Pending {
                return step;
            }
        }
        NavStep::Pending
    }

    #[test]
    fn an_earlier_navigation_queued_before_ours_is_not_a_replacement() {
        // L1 (earlier) commits while our Page.navigate is in flight; ours (L2) then loads.
        assert_eq!(
            outcome(&[
                navigated("F", "L1"),
                lifecycle("F", "L1", "load"),
                navigated("F", "L2"),
                lifecycle("F", "L2", "DOMContentLoaded"),
                lifecycle("F", "L2", "load"),
            ]),
            NavStep::Loaded
        );
    }

    #[test]
    fn a_later_document_after_ours_committed_is_a_replacement() {
        assert_eq!(
            outcome(&[
                navigated("F", "L2"),
                navigated("F", "L3"),
                lifecycle("F", "L3", "load")
            ]),
            NavStep::Replaced
        );
    }

    #[test]
    fn another_navigations_load_never_completes_ours() {
        // Ours never commits (another navigation won first): no success, the caller times out.
        assert_eq!(
            outcome(&[navigated("F", "L3"), lifecycle("F", "L3", "load")]),
            NavStep::Pending
        );
        // A subframe's load with our loader id does not count either.
        assert_eq!(
            outcome(&[lifecycle("child", "L2", "load")]),
            NavStep::Pending
        );
    }

    #[test]
    fn subframe_navigations_are_ignored() {
        assert_eq!(
            outcome(&[
                navigated("F", "L2"),
                navigated("child", "X"),
                lifecycle("F", "L2", "load")
            ]),
            NavStep::Loaded
        );
    }
}

//! Turning a saved workspace tab list into dock work.
//!
//! The daemon keeps the list across a switch, a reconnect, and (with its state
//! file) a restart. A terminal tab whose PTY is still live reattaches. One
//! whose PTY died with the previous daemon is started again with its title.

use std::collections::HashSet;

use fresh_gui_protocol::{WorkspaceTab, WorkspaceTabKind};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestoreStep {
    /// Reattach a live PTY.
    Attach { pty_id: String, title: String },
    /// Start a new shell in place of a PTY that no longer exists.
    Respawn { title: String },
    /// Reopen a file. `activate` marks the saved active tab.
    Editor { path: String, activate: bool },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestorePlan {
    pub steps: Vec<RestoreStep>,
    /// Live PTY to select after editors finish opening, when the saved active
    /// tab is a terminal.
    pub focus_pty: Option<String>,
    /// Live PTYs not named by any tab (opened just before a crash).
    pub orphans: Vec<String>,
}

pub fn restore_plan(tabs: Vec<WorkspaceTab>, active_tab: u32, live: &[String]) -> RestorePlan {
    let live_set: HashSet<&str> = live.iter().map(String::as_str).collect();
    let mut seen = HashSet::new();
    let mut plan = RestorePlan::default();
    for (index, tab) in tabs.into_iter().enumerate() {
        let is_active = index as u32 == active_tab;
        match tab.kind {
            WorkspaceTabKind::Terminal => match tab.pty_id {
                Some(pty_id) if live_set.contains(pty_id.as_str()) => {
                    if !seen.insert(pty_id.clone()) {
                        continue;
                    }
                    if is_active {
                        plan.focus_pty = Some(pty_id.clone());
                    }
                    plan.steps.push(RestoreStep::Attach {
                        pty_id,
                        title: tab.title,
                    });
                }
                _ => plan.steps.push(RestoreStep::Respawn { title: tab.title }),
            },
            WorkspaceTabKind::Editor => {
                if let Some(path) = tab.path {
                    plan.steps.push(RestoreStep::Editor {
                        path,
                        activate: is_active,
                    });
                }
            }
        }
    }
    plan.orphans = live
        .iter()
        .filter(|id| seen.insert((*id).clone()))
        .cloned()
        .collect();
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(title: &str, pty: &str) -> WorkspaceTab {
        WorkspaceTab {
            kind: WorkspaceTabKind::Terminal,
            title: title.into(),
            pty_id: Some(pty.into()),
            path: None,
        }
    }

    fn editor(path: &str) -> WorkspaceTab {
        WorkspaceTab {
            kind: WorkspaceTabKind::Editor,
            title: "f".into(),
            pty_id: None,
            path: Some(path.into()),
        }
    }

    #[test]
    fn live_terminals_reattach_and_dead_ones_respawn_with_their_title() {
        let plan = restore_plan(
            vec![term("1", "live"), term("build", "gone"), editor("/p/a.rs")],
            2,
            &["live".into()],
        );
        assert_eq!(
            plan.steps,
            vec![
                RestoreStep::Attach {
                    pty_id: "live".into(),
                    title: "1".into()
                },
                RestoreStep::Respawn {
                    title: "build".into()
                },
                RestoreStep::Editor {
                    path: "/p/a.rs".into(),
                    activate: true
                },
            ]
        );
        assert_eq!(plan.focus_pty, None);
        assert!(plan.orphans.is_empty());
    }

    #[test]
    fn active_terminal_is_focused_even_after_an_earlier_editor() {
        let plan = restore_plan(
            vec![editor("/p/a.rs"), term("1", "p1"), term("2", "p2")],
            2,
            &["p1".into(), "p2".into()],
        );
        assert_eq!(plan.focus_pty.as_deref(), Some("p2"));
        assert!(plan.steps.contains(&RestoreStep::Editor {
            path: "/p/a.rs".into(),
            activate: false
        }));
    }

    #[test]
    fn duplicate_tabs_attach_once_and_untracked_ptys_are_kept() {
        let plan = restore_plan(
            vec![term("1", "p1"), term("1 again", "p1")],
            0,
            &["p1".into(), "stray".into()],
        );
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.orphans, vec!["stray".to_string()]);
    }
}

//! Pure bookkeeping for the window-button list. No Win32 in here, so the
//! ordering and click-policy logic unit-tests on the Linux dev host.

/// One taskbar-worthy top-level window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskWindow {
    /// The HWND as an integer, so this module stays platform-free.
    pub hwnd: isize,
    pub title: String,
}

/// Merge a fresh enumeration into the current button list, keeping button
/// order stable the way a taskbar should: surviving windows keep their
/// position (with refreshed titles), closed windows drop out, new windows
/// append at the end in enumeration order.
///
/// Returns `(merged, added_titles, removed_titles)`; compare `merged` with
/// the old list to know whether anything (including a title) changed.
pub fn merge(
    old: &[TaskWindow],
    fresh: &[TaskWindow],
) -> (Vec<TaskWindow>, Vec<String>, Vec<String>) {
    let mut merged = Vec::with_capacity(fresh.len());
    let mut removed = Vec::new();
    for w in old {
        match fresh.iter().find(|f| f.hwnd == w.hwnd) {
            Some(f) => merged.push(f.clone()),
            None => removed.push(w.title.clone()),
        }
    }
    let mut added = Vec::new();
    for f in fresh {
        if !old.iter().any(|w| w.hwnd == f.hwnd) {
            added.push(f.title.clone());
            merged.push(f.clone());
        }
    }
    (merged, added, removed)
}

/// What a click on a window's button does. The rules every taskbar follows:
/// clicking the focused window minimizes it, clicking a minimized one brings
/// it back, clicking any other window focuses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickAction {
    Minimize,
    RestoreAndFocus,
    Focus,
}

pub fn click_action(is_foreground: bool, is_minimized: bool) -> ClickAction {
    if is_minimized {
        ClickAction::RestoreAndFocus
    } else if is_foreground {
        ClickAction::Minimize
    } else {
        ClickAction::Focus
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(hwnd: isize, title: &str) -> TaskWindow {
        TaskWindow {
            hwnd,
            title: title.to_string(),
        }
    }

    #[test]
    fn merge_keeps_stable_order_and_appends_new() {
        let old = [w(1, "a"), w(2, "b")];
        // Fresh enumeration in a different (z) order, plus a newcomer.
        let fresh = [w(3, "c"), w(2, "b"), w(1, "a")];
        let (merged, added, removed) = merge(&old, &fresh);
        assert_eq!(merged, vec![w(1, "a"), w(2, "b"), w(3, "c")]);
        assert_eq!(added, vec!["c"]);
        assert!(removed.is_empty());
    }

    #[test]
    fn merge_drops_closed_windows() {
        let old = [w(1, "a"), w(2, "b"), w(3, "c")];
        let fresh = [w(3, "c"), w(1, "a")];
        let (merged, added, removed) = merge(&old, &fresh);
        assert_eq!(merged, vec![w(1, "a"), w(3, "c")]);
        assert!(added.is_empty());
        assert_eq!(removed, vec!["b"]);
    }

    #[test]
    fn merge_refreshes_titles_in_place() {
        let old = [w(1, "Document1 - Editor")];
        let fresh = [w(1, "Document2 - Editor")];
        let (merged, added, removed) = merge(&old, &fresh);
        assert_eq!(merged, vec![w(1, "Document2 - Editor")]);
        assert!(added.is_empty() && removed.is_empty());
        assert_ne!(merged, old.to_vec()); // callers detect the repaint this way
    }

    #[test]
    fn merge_of_identical_lists_is_equal() {
        let old = [w(1, "a"), w(2, "b")];
        let (merged, added, removed) = merge(&old, &old.to_vec());
        assert_eq!(merged, old.to_vec());
        assert!(added.is_empty() && removed.is_empty());
    }

    #[test]
    fn click_rules() {
        assert_eq!(click_action(false, false), ClickAction::Focus);
        assert_eq!(click_action(true, false), ClickAction::Minimize);
        assert_eq!(click_action(false, true), ClickAction::RestoreAndFocus);
        // A minimized-but-foreground oddity restores rather than re-minimizes.
        assert_eq!(click_action(true, true), ClickAction::RestoreAndFocus);
    }
}

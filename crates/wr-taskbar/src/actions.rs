//! Built-in start-menu actions (ADR 0007 follow-up): WinRestyle commands that
//! sit above the scanned `.lnk` apps — a calm, non-emergency Restore (the twin
//! of `Win+Ctrl+F1`), the settings window, and dev-only helpers. Pure list +
//! filter logic, so it unit-tests on the dev host like `apps`; the spawning
//! lives in `winlist::run_menu_action`.

/// Which group an action sits under in the menu — the header it appears
/// beneath. Admin actions are always shown; dev actions are gated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Admin,
    Dev,
}

impl Category {
    /// The header label shown above this group.
    pub fn label(self) -> &'static str {
        match self {
            Category::Admin => "Admin",
            Category::Dev => "Dev",
        }
    }
}

/// What a menu action does when chosen. The bar resolves each to a process to
/// spawn (`winlist::run_menu_action`); this stays free of paths and Win32.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    /// Restore the standard Windows desktop now (`wr-installer deactivate`):
    /// restore the registry, sweep the WinRestyle family, bring explorer back.
    Restore,
    /// Open the manager window (`wr-installer`, no args).
    Settings,
    /// Open a PowerShell in the repo root (dev).
    Terminal,
    /// Launch the VM test suite in a new PowerShell (dev).
    RunTests,
}

/// One action row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuAction {
    pub label: &'static str,
    pub kind: ActionKind,
    pub category: Category,
    /// Shown only in a dev build (exe under a `target\` tree). A shipped
    /// restyler must never surface cargo/test commands to end users.
    pub dev_only: bool,
}

/// The actions to show, filtered by whether this is a dev build. Order is the
/// on-screen order and groups by category: admin actions first, dev after.
pub fn actions(dev: bool) -> Vec<MenuAction> {
    const ALL: [MenuAction; 4] = [
        MenuAction {
            label: "Restore Windows desktop",
            kind: ActionKind::Restore,
            category: Category::Admin,
            dev_only: false,
        },
        MenuAction {
            label: "WinRestyle settings",
            kind: ActionKind::Settings,
            category: Category::Admin,
            dev_only: false,
        },
        MenuAction {
            label: "Open terminal here",
            kind: ActionKind::Terminal,
            category: Category::Dev,
            dev_only: true,
        },
        MenuAction {
            label: "Run VM test suite",
            kind: ActionKind::RunTests,
            category: Category::Dev,
            dev_only: true,
        },
    ];
    ALL.into_iter().filter(|a| dev || !a.dev_only).collect()
}

/// Indices into `actions` whose labels match `filter` (case-insensitive
/// substring; empty matches all) — the same contract as [`crate::apps::
/// filter_indices`], so the menu filters actions and apps identically.
pub fn filter_indices(actions: &[MenuAction], filter: &str) -> Vec<usize> {
    let needle = filter.to_lowercase();
    actions
        .iter()
        .enumerate()
        .filter(|(_, a)| needle.is_empty() || a.label.to_lowercase().contains(&needle))
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_gate_hides_dev_actions_outside_dev_builds() {
        let shipped = actions(false);
        assert!(shipped.iter().all(|a| !a.dev_only));
        assert_eq!(shipped.len(), 2); // Restore + Settings
        assert_eq!(shipped[0].kind, ActionKind::Restore);

        let dev = actions(true);
        assert_eq!(dev.len(), 4);
        assert!(dev.iter().any(|a| a.kind == ActionKind::RunTests));
        // Admin actions still lead the list in dev mode.
        assert_eq!(dev[0].kind, ActionKind::Restore);
        assert_eq!(dev[1].kind, ActionKind::Settings);
    }

    #[test]
    fn actions_carry_their_group() {
        let dev = actions(true);
        // Admin group leads, then dev — grouped and contiguous.
        assert_eq!(dev[0].category, Category::Admin);
        assert_eq!(dev[1].category, Category::Admin);
        assert_eq!(dev[2].category, Category::Dev);
        assert_eq!(dev[3].category, Category::Dev);
        assert_eq!(Category::Admin.label(), "Admin");
        assert_eq!(Category::Dev.label(), "Dev");
    }

    #[test]
    fn filter_matches_labels_case_insensitively() {
        let dev = actions(true);
        assert_eq!(filter_indices(&dev, ""), vec![0, 1, 2, 3]);
        // "test" hits only "Run VM test suite".
        let hits = filter_indices(&dev, "TEST");
        assert_eq!(hits.len(), 1);
        assert_eq!(dev[hits[0]].kind, ActionKind::RunTests);
        // "restore" hits Restore.
        assert_eq!(
            dev[filter_indices(&dev, "restore")[0]].kind,
            ActionKind::Restore
        );
        assert!(filter_indices(&dev, "zzz").is_empty());
    }
}

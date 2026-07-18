//! The mutual-supervision protocol between `wr-watchdog` and `wr-shell`.
//!
//! T5 proved Winlogon's `AutoRestartShell` does not restart a custom per-user
//! shell (see ADR 0002), so the pair supervise *each other*:
//!
//! - the watchdog supervises the shell (relaunch on exit, crash-loop fallback);
//! - the shell watches the watchdog process and relaunches it if it dies. The
//!   relaunched watchdog then kills the stray shell and spawns a fresh one, so
//!   the pair converges to exactly one of each.
//!
//! Both sides communicate through environment variables on the processes they
//! spawn (Phase 1 upgrades this to the `wr-ipc` named pipe):
//!
//! - [`WATCHDOG_PID_ENV`]: set by the watchdog on each shell it spawns, so the
//!   shell knows which process to watch.
//! - [`RELAUNCH_STATE_ENV`]: watchdog-relaunch accounting ([`RelaunchState`]),
//!   set by the shell on a watchdog it relaunches and inherited onward through
//!   each spawn. Without it, a watchdog that crashes on startup would flicker
//!   forever: shell relaunches watchdog → watchdog spawns fresh shell → crash →
//!   fresh shell relaunches again, with every per-process counter reset.

/// Env var carrying the watchdog's PID to the shell it spawns.
pub const WATCHDOG_PID_ENV: &str = "WR_WATCHDOG_PID";

/// Env var carrying [`RelaunchState`] (`"<count>:<first-unix-secs>"`).
pub const RELAUNCH_STATE_ENV: &str = "WR_WD_RELAUNCH_STATE";

/// More than this many watchdog relaunches within [`RELAUNCH_WINDOW_SECS`]
/// means "give up and restore Windows".
pub const RELAUNCH_LIMIT: u32 = 3;
pub const RELAUNCH_WINDOW_SECS: u64 = 60;

/// Watchdog-relaunch accounting, threaded through the spawn chain as
/// `"<count>:<first-unix-secs>"` in [`RELAUNCH_STATE_ENV`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RelaunchState {
    /// Relaunches performed in the current window.
    pub count: u32,
    /// Unix seconds of the first relaunch in the current window.
    pub first_unix_secs: u64,
}

impl RelaunchState {
    /// Parse the env-var form; `None` input (var unset) or garbage both mean
    /// "fresh session, no relaunches yet".
    pub fn parse(raw: Option<&str>) -> Self {
        let parsed = raw.and_then(|s| {
            let (count, first) = s.split_once(':')?;
            Some(RelaunchState {
                count: count.parse().ok()?,
                first_unix_secs: first.parse().ok()?,
            })
        });
        parsed.unwrap_or_default()
    }

    /// Account for one more relaunch happening at `now` (unix seconds). Starts
    /// a new window if the current one has expired.
    #[must_use]
    pub fn bump(self, now_unix_secs: u64) -> Self {
        if self.count == 0
            || now_unix_secs.saturating_sub(self.first_unix_secs) > RELAUNCH_WINDOW_SECS
        {
            RelaunchState {
                count: 1,
                first_unix_secs: now_unix_secs,
            }
        } else {
            RelaunchState {
                count: self.count + 1,
                ..self
            }
        }
    }

    /// True once relaunching should stop in favor of restoring Windows.
    pub fn exhausted(&self) -> bool {
        self.count > RELAUNCH_LIMIT
    }

    /// The env-var form, for the next process in the chain.
    pub fn to_env_value(self) -> String {
        format!("{}:{}", self.count, self.first_unix_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_or_garbage_parses_as_fresh() {
        for raw in [
            None,
            Some(""),
            Some("junk"),
            Some("1:"),
            Some(":5"),
            Some("a:b"),
        ] {
            assert_eq!(
                RelaunchState::parse(raw),
                RelaunchState::default(),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn round_trips_through_env_form() {
        let state = RelaunchState::parse(Some("2:1000"));
        assert_eq!(state.count, 2);
        assert_eq!(RelaunchState::parse(Some(&state.to_env_value())), state);
    }

    #[test]
    fn bump_counts_within_window_and_resets_after() {
        let first = RelaunchState::default().bump(1000);
        assert_eq!((first.count, first.first_unix_secs), (1, 1000));

        let second = first.bump(1030);
        assert_eq!((second.count, second.first_unix_secs), (2, 1000));

        // Window expired → new window, count restarts.
        let later = second.bump(1000 + RELAUNCH_WINDOW_SECS + 1);
        assert_eq!((later.count, later.first_unix_secs), (1, 1061));
    }

    #[test]
    fn exhausts_only_past_the_limit() {
        let mut state = RelaunchState::default();
        for _ in 0..RELAUNCH_LIMIT {
            state = state.bump(1000);
            assert!(!state.exhausted());
        }
        assert!(state.bump(1000).exhausted());
    }
}

//! Safe-apply: the sequence the Phase 3 manager runs when the user commits a
//! restyle, and the matching teardown. The order exists to keep the prime
//! invariant ("we must always be able to put explorer back") intact even if the
//! machine loses power between any two steps:
//!
//! 1. **Preflight** — the sibling binaries (`wr-watchdog.exe`, `wr-shell.exe`,
//!    `wr-taskbar.exe`) are all present. A swap that points the shell at a
//!    missing watchdog would brick the next logon.
//! 2. **Write config** — persist the chosen components and startup opt-outs
//!    *before* the swap, so the first swapped logon already reflects them.
//! 3. **Trial run** — launch `wr-shell --selftest`, proving the shell binary
//!    actually runs on this machine (right architecture, DLLs resolve, config
//!    parses) while explorer is still the shell and a failure costs nothing.
//! 4. **Back up + swap** — only now touch the registry, and only after backing
//!    up the original `HKCU\…\Shell` value byte-for-byte (`wr_core::shell`).
//! 5. **Recovery instructions** — hand the user the emergency hotkey and the
//!    one-command restore, every time.
//!
//! Teardown ([`uninstall`]) is the CLI `restore` path, shared so the GUI and
//! CLI can never diverge: restore the registry, sweep our surfaces, and relaunch
//! explorer only if no desktop shell is already on screen (idempotent — the same
//! rule the watchdog's `recover()` follows).
//!
//! The pure parts (recovery text, preflight logic, step descriptions) are
//! cross-platform and unit-tested; only the registry/process steps are gated to
//! Windows.

/// The WinRestyle binaries that must sit next to the installer for a swap to be
/// safe. The registry `Shell` value points at the watchdog, which spawns the
/// shell, which spawns the taskbar — all three must exist.
pub const REQUIRED_BINARIES: [&str; 3] = ["wr-watchdog.exe", crate::SHELL_EXE, crate::TASKBAR_EXE];

/// Which of [`REQUIRED_BINARIES`] are absent, given a presence predicate.
/// Split out from the filesystem so the check is testable on any host.
pub fn missing_binaries<F: Fn(&str) -> bool>(exists: F) -> Vec<&'static str> {
    REQUIRED_BINARIES
        .into_iter()
        .filter(|name| !exists(name))
        .collect()
}

/// The recovery instructions shown after every apply. One source of truth, so
/// the UI, the CLI, and the docs quote the same hotkey and command.
pub fn recovery_instructions() -> String {
    format!(
        "If the new desktop misbehaves, you can always get Windows back:\n\
         \n\
         • Press {hotkey} at any time — it restores explorer immediately.\n\
         • Or run `wr-installer restore` from another machine/account, then log in again.\n\
         • The change only takes full effect after you log out and back in.\n\
         \n\
         Your original shell setting was backed up and is restored byte-for-byte.",
        hotkey = crate::EMERGENCY_HOTKEY_LABEL,
    )
}

/// The human-readable result of a successful apply, for the manager to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// A one-line headline ("Restyle applied — log out and back in").
    pub headline: String,
    /// The full recovery instructions ([`recovery_instructions`]).
    pub instructions: String,
}

impl ApplyOutcome {
    #[cfg_attr(not(windows), allow(dead_code))]
    fn applied() -> Self {
        ApplyOutcome {
            headline: "Restyle applied — log out and back in to start WinRestyle.".to_string(),
            instructions: recovery_instructions(),
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    use anyhow::{bail, Context, Result};

    use crate::config::Config;
    use crate::shell::RestoreOutcome;

    use super::{missing_binaries, ApplyOutcome};

    /// How long the trial `--selftest` run is given to come up and exit. A
    /// healthy shell selftest returns in well under a second; this is only a
    /// backstop against a wedged binary.
    const TRIAL_TIMEOUT: Duration = Duration::from_secs(10);

    /// The directory the installer (and its sibling binaries) live in.
    fn install_dir() -> Result<PathBuf> {
        let exe = std::env::current_exe().context("locating the installer executable")?;
        exe.parent()
            .map(Path::to_path_buf)
            .context("installer has no parent directory")
    }

    /// Confirm every required binary sits next to the installer.
    pub fn preflight() -> Result<()> {
        let dir = install_dir()?;
        let missing = missing_binaries(|name| dir.join(name).is_file());
        if !missing.is_empty() {
            bail!(
                "missing WinRestyle binaries next to the installer: {}",
                missing.join(", ")
            );
        }
        Ok(())
    }

    /// Launch `wr-shell --selftest` and require a clean (exit 0) return within
    /// [`TRIAL_TIMEOUT`]. Proves the shell binary runs on this machine before
    /// we make it the registered shell. Never touches the registry.
    pub fn trial_run() -> Result<()> {
        let shell = install_dir()?.join(crate::SHELL_EXE);
        let mut child = Command::new(&shell)
            .arg("--selftest")
            .spawn()
            .with_context(|| format!("launching {} for a trial run", shell.display()))?;

        let deadline = Instant::now() + TRIAL_TIMEOUT;
        loop {
            match child.try_wait().context("waiting on the trial shell")? {
                Some(status) if status.success() => return Ok(()),
                Some(status) => bail!("trial shell run failed: {status}"),
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("trial shell run did not finish within {TRIAL_TIMEOUT:?}");
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    /// The full safe-apply. `config_path` is where to persist `config`
    /// (typically [`crate::config::default_path`]).
    pub fn apply_restyle(config_path: &Path, config: &Config) -> Result<ApplyOutcome> {
        preflight()?;
        crate::config::write(config_path, config).context("writing config before swap")?;
        trial_run().context("trial run failed; registry NOT changed")?;

        // The registry Shell value must point at the *watchdog* (it owns the
        // emergency hotkey and supervises the shell), never wr-shell directly.
        let watchdog = install_dir()?.join("wr-watchdog.exe");
        crate::shell::backup_and_set_shell(&watchdog.to_string_lossy())
            .context("backing up and setting the shell")?;

        log::info!("restyle applied: shell -> {}", watchdog.display());
        Ok(ApplyOutcome::applied())
    }

    /// Teardown: restore the registry and bring explorer back if needed. Shared
    /// by the CLI `restore` and the manager's uninstall button. Idempotent — a
    /// second explorer is launched only when no desktop shell is on screen
    /// (the same rule as the watchdog's `recover()`), and our surfaces are
    /// swept so none linger over the restored desktop.
    pub fn uninstall() -> Result<RestoreOutcome> {
        let outcome = crate::shell::restore_shell().context("restoring the shell registry")?;

        // Sweep our surfaces so an uninstall from within a swapped session
        // never leaves a WinRestyle window floating over explorer.
        for exe in [crate::TASKBAR_EXE, crate::SHELL_EXE] {
            crate::process::kill_all_named(exe);
        }

        if crate::shell::desktop_shell_running() {
            log::info!("desktop shell already running; not launching explorer");
        } else {
            match Command::new("explorer.exe").spawn() {
                Ok(_) => log::info!("launched explorer.exe"),
                Err(e) => log::error!("failed to launch explorer.exe: {e}"),
            }
        }
        Ok(outcome)
    }
}

#[cfg(windows)]
pub use imp::{apply_restyle, preflight, trial_run, uninstall};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_instructions_name_the_emergency_hotkey() {
        let text = recovery_instructions();
        assert!(text.contains(crate::EMERGENCY_HOTKEY_LABEL));
        assert!(text.contains("restore"));
        assert!(text.contains("log out"));
    }

    #[test]
    fn missing_binaries_reports_exactly_whats_absent() {
        // Everything present → nothing missing.
        assert!(missing_binaries(|_| true).is_empty());
        // Nothing present → all three.
        assert_eq!(missing_binaries(|_| false).len(), REQUIRED_BINARIES.len());
        // Only the shell present → the other two are missing.
        let missing = missing_binaries(|name| name == crate::SHELL_EXE);
        assert!(!missing.contains(&crate::SHELL_EXE));
        assert!(missing.contains(&"wr-watchdog.exe"));
        assert!(missing.contains(&crate::TASKBAR_EXE));
    }

    #[test]
    fn required_binaries_point_the_shell_at_the_watchdog() {
        // The watchdog must be in the required set — the swap targets it, not
        // wr-shell directly (see apply_restyle).
        assert!(REQUIRED_BINARIES.contains(&"wr-watchdog.exe"));
        assert!(REQUIRED_BINARIES.contains(&crate::SHELL_EXE));
        assert!(REQUIRED_BINARIES.contains(&crate::TASKBAR_EXE));
    }

    #[test]
    fn applied_outcome_carries_recovery_text() {
        let outcome = ApplyOutcome::applied();
        assert!(!outcome.headline.is_empty());
        assert_eq!(outcome.instructions, recovery_instructions());
    }
}

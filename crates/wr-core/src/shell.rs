//! Read, back up, replace, and restore the per-user Windows shell.
//!
//! Windows decides which process is "the shell" at logon from:
//!
//! ```text
//! HKEY_CURRENT_USER \Software\Microsoft\Windows NT\CurrentVersion\Winlogon\Shell
//! HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon\Shell  (default "explorer.exe")
//! ```
//!
//! If the **HKCU** value is absent, Windows falls back to the HKLM value (which
//! is `explorer.exe` on a stock install). WinRestyle only ever touches the
//! per-user value, so "restore" means putting HKCU back exactly as we found it:
//! either the original string, or *absent* if there was none.
//!
//! NOTE: This module is Windows-only. The whole crate compiles on other
//! platforms (for editor/CI convenience) but these functions are gated to
//! `cfg(windows)`; non-Windows builds get stubs that return an error.

/// Outcome of restoring the shell, for logging / UX.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// We wrote the original HKCU `Shell` value back.
    RestoredOriginal,
    /// There was no original HKCU value, so we removed our override (Windows
    /// now falls back to the HKLM `explorer.exe` default).
    RemovedOverride,
    /// Nothing to do — no WinRestyle override was present.
    NothingToRestore,
}

#[cfg(windows)]
mod imp {
    use super::RestoreOutcome;
    use anyhow::{Context, Result};
    use winreg::enums::*;
    use winreg::RegKey;

    const WINLOGON: &str = r"Software\Microsoft\Windows NT\CurrentVersion\Winlogon";
    const SHELL_VALUE: &str = "Shell";

    // Our own backup location.
    const BACKUP_KEY: &str = r"Software\WinRestyle";
    const BACKUP_VALUE: &str = "OriginalShell";
    /// 1 if an original HKCU `Shell` value existed, 0 if it was absent.
    const BACKUP_PRESENT: &str = "OriginalShellPresent";

    /// The current per-user `Shell` value, or `None` if not set (Windows then
    /// uses the HKLM `explorer.exe` default).
    pub fn read_user_shell() -> Result<Option<String>> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let winlogon = hkcu
            .open_subkey(WINLOGON)
            .context("opening HKCU Winlogon key")?;
        match winlogon.get_value::<String, _>(SHELL_VALUE) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("reading HKCU Shell value"),
        }
    }

    /// True if WinRestyle has a saved backup (i.e. an override is/was applied).
    pub fn has_backup() -> Result<bool> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        match hkcu.open_subkey(BACKUP_KEY) {
            Ok(k) => Ok(k.get_raw_value(BACKUP_PRESENT).is_ok()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).context("opening WinRestyle backup key"),
        }
    }

    /// Back up the current per-user `Shell` value (idempotent — refuses to
    /// clobber an existing backup) and set ours.
    ///
    /// `new_shell` is typically the absolute path to `wr-shell.exe`.
    pub fn backup_and_set_shell(new_shell: &str) -> Result<()> {
        if has_backup()? {
            anyhow::bail!(
                "a WinRestyle shell backup already exists; restore before applying again"
            );
        }

        let original = read_user_shell()?;

        // Save the backup first, so a failure mid-way never loses the original.
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (backup, _) = hkcu
            .create_subkey(BACKUP_KEY)
            .context("creating WinRestyle backup key")?;
        match &original {
            Some(value) => {
                backup.set_value(BACKUP_VALUE, value)?;
                backup.set_value(BACKUP_PRESENT, &1u32)?;
            }
            None => {
                backup.set_value(BACKUP_VALUE, &"")?;
                backup.set_value(BACKUP_PRESENT, &0u32)?;
            }
        }

        let (winlogon, _) = hkcu
            .create_subkey(WINLOGON)
            .context("opening HKCU Winlogon key for write")?;
        winlogon
            .set_value(SHELL_VALUE, &new_shell)
            .context("writing new HKCU Shell value")?;

        log::info!("shell override applied: HKCU Shell = {new_shell:?} (was {original:?})");
        Ok(())
    }

    /// Restore the per-user shell to exactly what it was before WinRestyle, and
    /// clear our backup. Safe to call when nothing is applied.
    pub fn restore_shell() -> Result<RestoreOutcome> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);

        let backup = match hkcu.open_subkey(BACKUP_KEY) {
            Ok(k) if k.get_raw_value(BACKUP_PRESENT).is_ok() => k,
            _ => return Ok(RestoreOutcome::NothingToRestore),
        };

        let present: u32 = backup.get_value(BACKUP_PRESENT).unwrap_or(0);
        let (winlogon, _) = hkcu
            .create_subkey(WINLOGON)
            .context("opening HKCU Winlogon key for restore")?;

        let outcome = if present == 1 {
            let original: String = backup
                .get_value(BACKUP_VALUE)
                .context("reading backed-up Shell value")?;
            winlogon
                .set_value(SHELL_VALUE, &original)
                .context("restoring HKCU Shell value")?;
            RestoreOutcome::RestoredOriginal
        } else {
            // There was no HKCU value originally — remove our override so the
            // HKLM default (explorer.exe) applies again. Tolerate "not found".
            match winlogon.delete_value(SHELL_VALUE) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).context("removing HKCU Shell override"),
            }
            RestoreOutcome::RemovedOverride
        };

        // Clear the backup so a future apply starts clean.
        hkcu.delete_subkey_all(BACKUP_KEY)
            .context("clearing WinRestyle backup key")?;

        log::info!("shell restored: {outcome:?}");
        Ok(outcome)
    }
}

#[cfg(not(windows))]
mod imp {
    use super::RestoreOutcome;
    use anyhow::{bail, Result};

    const MSG: &str = "shell registry operations are only available on Windows";

    pub fn read_user_shell() -> Result<Option<String>> {
        bail!(MSG)
    }
    pub fn has_backup() -> Result<bool> {
        bail!(MSG)
    }
    pub fn backup_and_set_shell(_new_shell: &str) -> Result<()> {
        bail!(MSG)
    }
    pub fn restore_shell() -> Result<RestoreOutcome> {
        bail!(MSG)
    }
}

pub use imp::{backup_and_set_shell, has_backup, read_user_shell, restore_shell};

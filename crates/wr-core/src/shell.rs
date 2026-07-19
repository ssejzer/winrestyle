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

    /// True if a desktop shell is on screen right now, detected via the
    /// taskbar window class (`Shell_TrayWnd`) that a shell creates only when
    /// running *as the shell* (explorer as a file manager does not).
    /// Recovery paths check this before launching `explorer.exe`: when a
    /// live shell is already present, a second explorer would not re-adopt
    /// the shell role — it would just open a stray file-manager window.
    /// (A *hung* shell still counts as present; recovering from a wedged
    /// explorer is not our failure mode.)
    ///
    /// Since tray hosting (ADR 0005 amendment), `wr-taskbar` creates a
    /// `Shell_TrayWnd` of its own in swapped sessions, so *whose* window it
    /// is matters. The policy, per candidate window:
    ///
    /// - owned by `wr-taskbar.exe` → **not** a desktop shell (it is us);
    /// - owned by any other identifiable process → a desktop shell
    ///   (explorer, or a third-party replacement that claimed the class —
    ///   both mean a second explorer would only open a stray window);
    /// - owner unresolvable (window or process mid-teardown, access denied)
    ///   → **not** a desktop shell. The failure directions are asymmetric:
    ///   the recovery sweep terminates `wr-taskbar.exe` *without waiting*,
    ///   so a dying tray host of ours can be enumerable-but-unopenable at
    ///   check time — counting it would make `recover()` (which runs at
    ///   most once) skip launching explorer and strand the user with no
    ///   shell at all, while not counting a real-but-unidentifiable shell
    ///   merely opens one stray explorer window. We must always be able to
    ///   put explorer back; when in doubt, launch it.
    pub fn desktop_shell_running() -> bool {
        use windows::core::w;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::FindWindowExW;

        unsafe {
            let mut hwnd = HWND::default();
            loop {
                hwnd = match FindWindowExW(None, hwnd, w!("Shell_TrayWnd"), None) {
                    Ok(h) if !h.is_invalid() => h,
                    _ => return false, // no (further) tray windows at all
                };
                match window_owner_image(hwnd) {
                    Some(image) if super::image_is_our_taskbar(&image) => {}
                    Some(_) => return true,
                    None => {}
                }
            }
        }
    }

    /// Full image path of the process owning `hwnd`, or `None` when it
    /// cannot be resolved (window or process already gone, access denied).
    unsafe fn window_owner_image(hwnd: windows::Win32::Foundation::HWND) -> Option<String> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION,
        };
        use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

        let mut pid = 0u32;
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let image = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .ok()
        .map(|()| String::from_utf16_lossy(&buf[..len as usize]));
        let _ = CloseHandle(process);
        image
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
    pub fn desktop_shell_running() -> bool {
        false
    }
}

pub use imp::{
    backup_and_set_shell, desktop_shell_running, has_backup, read_user_shell, restore_shell,
};

/// Whether a full process image path names our own taskbar executable
/// (`wr-taskbar.exe`, the only WinRestyle binary that ever creates a
/// `Shell_TrayWnd`). Case-insensitive on the basename only.
#[cfg_attr(not(windows), allow(dead_code))]
fn image_is_our_taskbar(path: &str) -> bool {
    path.rsplit(['\\', '/'])
        .next()
        .is_some_and(|name| name.eq_ignore_ascii_case(crate::TASKBAR_EXE))
}

#[cfg(test)]
mod tests {
    use super::image_is_our_taskbar;

    #[test]
    fn our_taskbar_image_matching() {
        assert!(image_is_our_taskbar(
            r"C:\Program Files\WinRestyle\wr-taskbar.exe"
        ));
        assert!(image_is_our_taskbar(r"D:\builds\WR-TASKBAR.EXE"));
        assert!(image_is_our_taskbar("wr-taskbar.exe"));
        // Forward slashes appear in some query paths.
        assert!(image_is_our_taskbar("C:/x/wr-taskbar.exe"));

        // Explorer and third-party shells are NOT ours — they count as
        // desktop shells in desktop_shell_running.
        assert!(!image_is_our_taskbar(r"C:\Windows\explorer.exe"));
        assert!(!image_is_our_taskbar(r"C:\shells\litestep.exe"));
        assert!(!image_is_our_taskbar(r"C:\evil\notwr-taskbar.exe.bak"));
        assert!(!image_is_our_taskbar(r"C:\wr-taskbar.exe\payload.exe"));
        assert!(!image_is_our_taskbar(""));
    }
}

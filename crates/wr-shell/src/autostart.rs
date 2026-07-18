//! Logon autostart (ADR 0004): launch what explorer would at shell start, so
//! a WinRestyle session isn't degraded — Run/RunOnce registry entries, the
//! Startup folders, and session helpers like `rdpclip.exe`.
//!
//! Safety posture, in the same spirit as the rest of the shell:
//!
//! - **Never write HKLM.** HKLM `Run` entries are launched (read-only); HKLM
//!   `RunOnce` entries are *skipped with a warning* — honoring them requires
//!   deleting HKLM values, and running them without deleting would re-run
//!   installers every logon.
//! - **HKCU `RunOnce` follows the Windows contract**: the value is deleted
//!   before the command runs (after, for `!`-prefixed names). The deletion is
//!   the OS contract, not a restyling change, so it is intentionally not
//!   backed up.
//! - **Once per logon session**: a marker in `%LOCALAPPDATA%` keyed by the
//!   logon-session LUID stops a crash-relaunched shell from re-running
//!   everything.
//! - **Never in an unswapped session**: if another desktop shell is on
//!   screen (dev/test runs), autostart is skipped entirely.
//! - Every entry is logged with an id (`hkcu-run:<name>`, `startup-user:
//!   <file>`, `session:rdpclip`, …) that the config's `[autostart].disabled`
//!   list matches against.
//!
//! `--autostart-test-filter=<substr>` (VM tests only) bypasses the two guards
//! and launches *only* entries whose id contains the substring, so T12 can
//! exercise the real enumerate → filter → launch path without spawning the
//! session's real startup apps.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};

use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE};
use winreg::types::FromRegValue;
use winreg::{RegKey, HKEY};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::System::Threading::{
    CreateProcessW, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION, SW_SHOWNORMAL};

use wr_core::config::ConfigStore;

const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUNONCE_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\RunOnce";

/// Spawn the autostart thread. Fire-and-forget: failures are logged and never
/// touch the safety harness.
pub fn start(store: Arc<ConfigStore>, test_filter: Option<String>) {
    std::thread::spawn(move || {
        // ShellExecuteW (Startup-folder .lnk files) wants COM.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        run(&store, test_filter.as_deref());
    });
}

fn run(store: &ConfigStore, test_filter: Option<&str>) {
    let cfg = store.get().autostart;
    if !cfg.enabled {
        log::info!("autostart disabled in config; launching nothing");
        return;
    }
    if test_filter.is_none() {
        if wr_core::shell::desktop_shell_running() {
            log::info!("autostart skipped: another desktop shell is on screen (unswapped run)");
            return;
        }
        match session::already_ran() {
            Ok(true) => {
                log::info!("autostart already ran this logon session; skipping (relaunch)");
                return;
            }
            Ok(false) => {}
            // Fail open: a missed marker means a rare double-run after a
            // crash; a false "already ran" would silently degrade every
            // session.
            Err(e) => log::error!("autostart session marker unavailable; running anyway: {e:#}"),
        }
    }

    let (mut launched, mut disabled, mut failed) = (0u32, 0u32, 0u32);
    for entry in enumerate() {
        if let Some(filter) = test_filter {
            if !entry
                .id
                .to_ascii_lowercase()
                .contains(&filter.to_ascii_lowercase())
            {
                continue;
            }
        }
        if !cfg.allows(&entry.id) {
            log::info!("autostart: skipped {} (disabled in config)", entry.id);
            disabled += 1;
            continue;
        }
        match entry.launch() {
            Ok(()) => {
                log::info!("autostart: launched {}", entry.id);
                launched += 1;
            }
            Err(e) => {
                log::warn!("autostart: {} failed: {e:#}", entry.id);
                failed += 1;
            }
        }
    }
    if test_filter.is_none() {
        if let Err(e) = session::record_ran() {
            log::error!("autostart: could not record session marker: {e:#}");
        }
    }
    log::info!("autostart done: {launched} launched, {disabled} disabled, {failed} failed");
}

struct Entry {
    id: String,
    action: Action,
}

enum Action {
    /// A raw command line (registry `Run` entries).
    CommandLine(String),
    /// An HKCU `RunOnce` value: delete, then run (reversed for `!` names).
    RunOnce {
        name: String,
        command: String,
        delete_after: bool,
    },
    /// A Startup-folder item (usually a `.lnk`) — ShellExecute it.
    OpenPath(PathBuf),
    /// `rdpclip.exe`, so clipboard/redirection works in remote sessions.
    Rdpclip,
}

impl Entry {
    fn launch(&self) -> Result<()> {
        match &self.action {
            Action::CommandLine(cmd) => spawn_command_line(cmd),
            Action::RunOnce {
                name,
                command,
                delete_after,
            } => {
                let key = RegKey::predef(HKEY_CURRENT_USER)
                    .open_subkey_with_flags(RUNONCE_SUBKEY, KEY_SET_VALUE)
                    .context("opening HKCU RunOnce for delete")?;
                // Delete-before-run is the Windows contract: a command that
                // crashes the session must not run again next logon.
                if !delete_after {
                    key.delete_value(name).context("deleting RunOnce value")?;
                }
                spawn_command_line(command)?;
                if *delete_after {
                    let _ = key.delete_value(name);
                }
                Ok(())
            }
            Action::OpenPath(path) => shell_open(path),
            Action::Rdpclip => std::process::Command::new("rdpclip.exe")
                .spawn()
                .map(drop)
                .context("spawning rdpclip.exe"),
        }
    }
}

/// Everything explorer would launch, in roughly explorer's order. Enumeration
/// only — nothing here mutates or launches.
fn enumerate() -> Vec<Entry> {
    let mut entries = Vec::new();

    // HKLM RunOnce: see the module docs — skipped, loudly.
    for (name, _) in reg_values(HKEY_LOCAL_MACHINE, RUNONCE_SUBKEY) {
        log::warn!(
            "autostart: ignoring HKLM RunOnce entry {name:?} \
             (HKLM is never written; see ADR 0004)"
        );
    }

    for (name, command) in reg_values(HKEY_CURRENT_USER, RUNONCE_SUBKEY) {
        let delete_after = name.starts_with('!');
        entries.push(Entry {
            id: format!("hkcu-runonce:{name}"),
            action: Action::RunOnce {
                name: name.clone(),
                command,
                delete_after,
            },
        });
    }
    for (name, command) in reg_values(HKEY_LOCAL_MACHINE, RUN_SUBKEY) {
        entries.push(Entry {
            id: format!("hklm-run:{name}"),
            action: Action::CommandLine(command),
        });
    }
    for (name, command) in reg_values(HKEY_CURRENT_USER, RUN_SUBKEY) {
        entries.push(Entry {
            id: format!("hkcu-run:{name}"),
            action: Action::CommandLine(command),
        });
    }

    for (scope, base) in [
        ("startup-common", std::env::var_os("ProgramData")),
        ("startup-user", std::env::var_os("APPDATA")),
    ] {
        let Some(base) = base else { continue };
        let dir = PathBuf::from(base).join(r"Microsoft\Windows\Start Menu\Programs\Startup");
        let Ok(items) = std::fs::read_dir(&dir) else {
            continue;
        };
        for item in items.flatten() {
            let name = item.file_name().to_string_lossy().into_owned();
            if name.eq_ignore_ascii_case("desktop.ini") {
                continue;
            }
            if item.file_type().is_ok_and(|t| t.is_file()) {
                entries.push(Entry {
                    id: format!("{scope}:{name}"),
                    action: Action::OpenPath(item.path()),
                });
            }
        }
    }

    if unsafe { GetSystemMetrics(SM_REMOTESESSION) } != 0 {
        entries.push(Entry {
            id: "session:rdpclip".to_string(),
            action: Action::Rdpclip,
        });
    }

    entries
}

/// Non-empty string values of a registry key, env-expanded. A missing key is
/// just an empty list.
fn reg_values(root: HKEY, subkey: &str) -> Vec<(String, String)> {
    let Ok(key) = RegKey::predef(root).open_subkey_with_flags(subkey, KEY_QUERY_VALUE) else {
        return Vec::new();
    };
    key.enum_values()
        .filter_map(|r| r.ok())
        .filter_map(|(name, value)| {
            let raw = String::from_reg_value(&value).ok()?;
            let command = expand_env(raw.trim());
            (!name.is_empty() && !command.is_empty()).then_some((name, command))
        })
        .collect()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `%VAR%` expansion for REG_EXPAND_SZ-style values. Returns the input
/// unchanged on any failure.
fn expand_env(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let input = wide(s);
    unsafe {
        let needed = ExpandEnvironmentStringsW(PCWSTR(input.as_ptr()), None);
        if needed == 0 {
            return s.to_string();
        }
        let mut buf = vec![0u16; needed as usize];
        let written = ExpandEnvironmentStringsW(PCWSTR(input.as_ptr()), Some(&mut buf));
        if written == 0 || written as usize > buf.len() {
            return s.to_string();
        }
        String::from_utf16_lossy(&buf[..written as usize - 1])
    }
}

/// Launch a raw command line, exactly as explorer does for Run entries.
fn spawn_command_line(cmd: &str) -> Result<()> {
    let mut cmd_wide = wide(cmd);
    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            PCWSTR::null(),
            PWSTR(cmd_wide.as_mut_ptr()),
            None,
            None,
            false,
            PROCESS_CREATION_FLAGS(0),
            None,
            PCWSTR::null(),
            &si,
            &mut pi,
        )
        .with_context(|| format!("CreateProcessW {cmd:?}"))?;
        let _ = CloseHandle(pi.hProcess);
        let _ = CloseHandle(pi.hThread);
    }
    Ok(())
}

/// ShellExecute a Startup-folder item (resolves `.lnk` shortcuts).
fn shell_open(path: &std::path::Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let path_wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let result = unsafe {
        ShellExecuteW(
            None,
            windows::core::w!("open"),
            PCWSTR(path_wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // Per the ShellExecute contract, values > 32 mean success.
    anyhow::ensure!(
        result.0 as isize > 32,
        "ShellExecuteW failed (code {})",
        result.0 as isize
    );
    Ok(())
}

/// The once-per-logon-session guard: a marker file holding the logon-session
/// LUID. Same LUID → this session already ran autostart. Survives arbitrary
/// shell/watchdog churn, needs no registry writes, and self-invalidates at
/// the next logon (new LUID).
mod session {
    use std::path::PathBuf;

    use anyhow::{Context, Result};

    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenStatistics, TOKEN_QUERY, TOKEN_STATISTICS,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub fn already_ran() -> Result<bool> {
        let marker = marker_path()?;
        let current = format!("{:x}", logon_session_luid()?);
        match std::fs::read_to_string(&marker) {
            Ok(recorded) => Ok(recorded.trim() == current),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(anyhow::Error::new(e).context("reading session marker")),
        }
    }

    pub fn record_ran() -> Result<()> {
        let marker = marker_path()?;
        if let Some(dir) = marker.parent() {
            std::fs::create_dir_all(dir).context("creating marker dir")?;
        }
        let current = format!("{:x}", logon_session_luid()?);
        std::fs::write(&marker, current).context("writing session marker")
    }

    fn marker_path() -> Result<PathBuf> {
        let base = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA not set")?;
        Ok(PathBuf::from(base)
            .join("WinRestyle")
            .join("autostart-session"))
    }

    /// The LUID Windows assigns this logon session — unique per logon, even
    /// across reboots.
    fn logon_session_luid() -> Result<u64> {
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
                .context("OpenProcessToken")?;
            let mut stats = TOKEN_STATISTICS::default();
            let mut len = 0u32;
            let result = GetTokenInformation(
                token,
                TokenStatistics,
                Some(std::ptr::from_mut(&mut stats).cast()),
                std::mem::size_of::<TOKEN_STATISTICS>() as u32,
                &mut len,
            );
            let _ = CloseHandle(token);
            result.context("GetTokenInformation")?;
            Ok(((stats.AuthenticationId.HighPart as u64) << 32)
                | stats.AuthenticationId.LowPart as u64)
        }
    }
}

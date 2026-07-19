//! Shared types and the shell-registry backup/restore logic that underpins
//! WinRestyle's safety model.
//!
//! The single most important invariant in this whole project: **we must always
//! be able to put `explorer.exe` back.** That logic lives in [`shell`].

pub mod autostart;
pub mod components;
pub mod config;
pub mod guardian;
pub mod manager;
pub mod process;
pub mod shell;

/// The named pipe used to coordinate watchdog ⇄ shell ⇄ installer.
pub const PIPE_NAME: &str = r"\\.\pipe\winrestyle";

/// Emergency-restore hotkey, documented in one place so UI and watchdog agree.
pub const EMERGENCY_HOTKEY_LABEL: &str = "Win + Ctrl + F1";

/// Executable names, shared so supervision and stray sweeps never drift.
pub const WATCHDOG_EXE: &str = "wr-watchdog.exe";
pub const SHELL_EXE: &str = "wr-shell.exe";
pub const TASKBAR_EXE: &str = "wr-taskbar.exe";
/// The installer/manager, spawned by the taskbar's start-menu actions
/// (ADR 0007 follow-up) to run `deactivate`/open the manager.
pub const INSTALLER_EXE: &str = "wr-installer.exe";

/// Window class of the taskbar's top-level bar windows. The shell finds the
/// bar by this class to forward config-change notifications.
///
/// Deliberately NOT `Shell_TrayWnd`: the tray host is a *separate* hidden
/// window the taskbar creates only in swapped sessions, and
/// `shell::desktop_shell_running` counts only explorer-owned `Shell_TrayWnd`
/// windows as a live desktop (ADR 0005 amendment).
pub const TASKBAR_WINDOW_CLASS: &str = "WinRestyleTaskbar";

/// Name of the registered window message (`RegisterWindowMessageW`) the shell
/// posts to surface windows after a config reload.
pub const CONFIG_CHANGED_MESSAGE: &str = "WinRestyleConfigChanged";

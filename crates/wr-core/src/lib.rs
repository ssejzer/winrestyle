//! Shared types and the shell-registry backup/restore logic that underpins
//! WinRestyle's safety model.
//!
//! The single most important invariant in this whole project: **we must always
//! be able to put `explorer.exe` back.** That logic lives in [`shell`].

pub mod guardian;
pub mod shell;

/// The named pipe used to coordinate watchdog ⇄ shell ⇄ installer.
pub const PIPE_NAME: &str = r"\\.\pipe\winrestyle";

/// Emergency-restore hotkey, documented in one place so UI and watchdog agree.
pub const EMERGENCY_HOTKEY_LABEL: &str = "Win + Ctrl + F1";

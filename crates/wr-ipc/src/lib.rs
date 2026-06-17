//! Message protocol shared by the watchdog, shell, and installer.
//!
//! Phase 0 defines the message shapes; Phase 1 implements the named-pipe
//! transport (`wr_core::PIPE_NAME`). Keeping the protocol in its own crate lets
//! every process agree on one definition.

use serde::{Deserialize, Serialize};

/// Messages sent *to* the watchdog (the guardian).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToWatchdog {
    /// The shell is alive; resets the crash-loop counter window.
    ShellHeartbeat,
    /// Restore `explorer.exe` now (same path as the emergency hotkey).
    RequestRestore,
}

/// Messages sent *to* the shell.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToShell {
    /// Re-read config from disk and re-apply (hot reload).
    ReloadConfig,
    /// Begin a clean shutdown.
    Shutdown,
}

//! WinRestyle installer / manager — the one-screen UX (Phase 3).
//!
//! Run with no arguments it opens the **manager window**: a Direct2D-rendered
//! (consistent with the taskbar) checklist of components (taskbar, wallpaper,
//! startup-programs management), a per-entry list of the logon-autostart
//! programs the shell would run, and two actions — **Restyle Now** (safe apply:
//! preflight → write config → trial run → back up + swap; `wr-core::manager`)
//! and **Undo / Restore**. The safety-critical logic all lives in cross-platform
//! `wr-core` modules (`components`, `autostart`, `manager`, `config`) and is
//! unit-tested; this crate is the thin, Windows-only presentation layer, so the
//! window itself is verified in the VM (manual T3 / T16).
//!
//! The headless subcommands remain for the automated harness and hand-driving:
//!
//!   wr-installer              open the manager window (default)
//!   wr-installer status       show the current/backed-up shell state
//!   wr-installer apply        back up + set HKCU Shell (DANGER: VM only)
//!   wr-installer activate     swap THIS session onto WinRestyle now (ADR 0008)
//!   wr-installer deactivate   restore + sweep + bring explorer back now
//!   wr-installer restore      restore the original shell registry value only

use anyhow::Result;

mod cli;
// Pure geometry/hit-testing; compiled (and unit-tested) on every host, used by
// the Windows-only render/app modules.
#[cfg(windows)]
mod app;
#[cfg(windows)]
mod render;
#[cfg_attr(not(windows), allow(dead_code))]
mod view;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cmd = std::env::args().nth(1);
    match cmd.as_deref() {
        None => gui(),
        Some("status") => cli::status(),
        Some("apply") => cli::apply(),
        Some("activate") => cli::activate(),
        Some("deactivate") => cli::deactivate(),
        Some("restore") => cli::restore(),
        Some("gui") => gui(),
        Some("--help" | "-h" | "help") => cli::usage(),
        Some(other) => {
            eprintln!("unknown command {other:?}");
            cli::usage();
        }
    }
}

#[cfg(windows)]
fn gui() -> Result<()> {
    app::run()
}

#[cfg(not(windows))]
fn gui() -> Result<()> {
    anyhow::bail!("WinRestyle only runs on Windows 11");
}

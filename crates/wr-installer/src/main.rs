//! WinRestyle installer / manager — the one-screen UX.
//!
//! Planned for **Phase 3**: a component checklist + **Restyle Now** / uninstall
//! button. Apply does: trial-run the shell → back up the registry → swap →
//! show recovery instructions.
//!
//! ## Phase 0 status: a tiny CLI for testing the safety harness by hand.
//!
//! This lets us exercise `wr-core::shell` without a UI yet:
//!
//!   wr-installer status     show the current/backed-up shell state
//!   wr-installer apply       back up + set HKCU Shell to wr-shell.exe (DANGER: VM only)
//!   wr-installer restore     restore the original shell

use anyhow::{bail, Result};

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cmd = std::env::args().nth(1).unwrap_or_else(|| "status".into());
    match cmd.as_str() {
        "status" => status(),
        "apply" => apply(),
        "restore" => restore(),
        other => {
            bail!("unknown command {other:?}; use: status | apply | restore");
        }
    }
}

#[cfg(windows)]
fn status() -> Result<()> {
    let current = wr_core::shell::read_user_shell()?;
    let backed_up = wr_core::shell::has_backup()?;
    println!("HKCU Shell (current): {current:?}");
    println!("WinRestyle backup present: {backed_up}");
    Ok(())
}

#[cfg(windows)]
fn apply() -> Result<()> {
    use anyhow::Context;
    let shell = std::env::current_exe()?
        .parent()
        .context("installer has no parent dir")?
        .join("wr-shell.exe");
    println!("WARNING: this replaces your per-user shell. Run in a VM only.");
    wr_core::shell::backup_and_set_shell(&shell.to_string_lossy())?;
    println!("applied. Log out/in (or reboot) to start the WinRestyle shell.");
    println!("emergency restore hotkey: {}", wr_core::EMERGENCY_HOTKEY_LABEL);
    Ok(())
}

#[cfg(windows)]
fn restore() -> Result<()> {
    let outcome = wr_core::shell::restore_shell()?;
    println!("restore outcome: {outcome:?}");
    Ok(())
}

#[cfg(not(windows))]
fn status() -> Result<()> {
    not_windows()
}
#[cfg(not(windows))]
fn apply() -> Result<()> {
    not_windows()
}
#[cfg(not(windows))]
fn restore() -> Result<()> {
    not_windows()
}
#[cfg(not(windows))]
fn not_windows() -> Result<()> {
    bail!("WinRestyle only runs on Windows 11");
}

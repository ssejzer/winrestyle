//! The headless CLI: `status` / `apply` / `restore`. Kept for the automated VM
//! harness (T0, T4) and for driving the safety harness by hand — the exact
//! behavior Phases 0–2 validated, unchanged. The GUI ([`crate::app`]) is the
//! Phase 3 front door; these stay the scriptable back door.

use anyhow::Result;

#[cfg(windows)]
pub fn status() -> Result<()> {
    let current = wr_core::shell::read_user_shell()?;
    let backed_up = wr_core::shell::has_backup()?;
    println!("HKCU Shell (current): {current:?}");
    println!("WinRestyle backup present: {backed_up}");
    Ok(())
}

#[cfg(windows)]
pub fn apply() -> Result<()> {
    use anyhow::Context;
    // The registry `Shell` value must point at the *watchdog*, not `wr-shell`
    // directly: the watchdog owns the `Win + Ctrl + F1` emergency hotkey and
    // supervises `wr-shell` as its child. Pointing `Shell` at `wr-shell.exe`
    // would log the user into a blank desktop with no hotkey and no supervisor
    // running — exactly the brick the safety harness exists to prevent.
    let shell = std::env::current_exe()?
        .parent()
        .context("installer has no parent dir")?
        .join("wr-watchdog.exe");
    println!("WARNING: this replaces your per-user shell. Run in a VM only.");
    wr_core::shell::backup_and_set_shell(&shell.to_string_lossy())?;
    println!("applied. Log out/in (or reboot) to start the WinRestyle shell.");
    println!(
        "emergency restore hotkey: {}",
        wr_core::EMERGENCY_HOTKEY_LABEL
    );
    Ok(())
}

#[cfg(windows)]
pub fn restore() -> Result<()> {
    let outcome = wr_core::shell::restore_shell()?;
    println!("restore outcome: {outcome:?}");
    Ok(())
}

#[cfg(not(windows))]
pub fn status() -> Result<()> {
    not_windows()
}
#[cfg(not(windows))]
pub fn apply() -> Result<()> {
    not_windows()
}
#[cfg(not(windows))]
pub fn restore() -> Result<()> {
    not_windows()
}

#[cfg(not(windows))]
fn not_windows() -> Result<()> {
    anyhow::bail!("WinRestyle only runs on Windows 11");
}

/// Print CLI usage (shared by the unknown-command error and `--help`).
pub fn usage() -> ! {
    eprintln!(
        "WinRestyle installer / manager\n\
         \n\
         Usage:\n\
         \x20 wr-installer              open the manager window (default)\n\
         \x20 wr-installer status       show the current/backed-up shell state\n\
         \x20 wr-installer apply        back up + set the shell (DANGER: VM only)\n\
         \x20 wr-installer restore      restore the original shell"
    );
    std::process::exit(2);
}

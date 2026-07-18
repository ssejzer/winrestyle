//! WinRestyle shell host.
//!
//! ## Phase 0 status: deliberately a dummy.
//!
//! Right now this is a throwaway "I am the shell" process used only to exercise
//! the watchdog (relaunch, crash-loop fallback, emergency hotkey). It does NOT
//! paint a desktop or host a taskbar yet — that arrives in Phase 1/2.
//!
//! Test helpers (args):
//!   --crash-after=<secs>   panic after N seconds (to test relaunch/crash-loop)
//!   --exit-after=<secs>    exit cleanly after N seconds
//!
//! With a small `--crash-after`, the watchdog should relaunch us a few times
//! and then fall back to explorer.exe — that is the behavior we want to verify.

use std::time::{Duration, Instant};

// `ticks % 10 == 0` reads fine; `.is_multiple_of()` would raise our MSRV (1.77).
#[allow(clippy::manual_is_multiple_of)]
fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let opts = Options::from_args(std::env::args().skip(1));
    log::info!(
        "wr-shell (Phase 0 dummy) starting; pid {}",
        std::process::id()
    );

    // ADR 0002 mutual supervision: watch the watchdog and relaunch it if it
    // dies (Winlogon won't — AutoRestartShell ignores custom per-user shells).
    #[cfg(windows)]
    guardian::watch_watchdog();
    log::info!(
        "==> If you see a blank desktop, this is expected. Press {} to restore Windows.",
        wr_core::EMERGENCY_HOTKEY_LABEL
    );

    let start = Instant::now();
    let mut ticks: u64 = 0;
    loop {
        if let Some(after) = opts.crash_after {
            if start.elapsed() >= after {
                panic!("wr-shell: simulated crash after {after:?}");
            }
        }
        if let Some(after) = opts.exit_after {
            if start.elapsed() >= after {
                log::info!("wr-shell: clean exit after {after:?}");
                return;
            }
        }
        std::thread::sleep(Duration::from_secs(1));
        ticks += 1;
        if ticks % 10 == 0 {
            log::info!("wr-shell alive ({ticks}s)");
        }
    }
}

/// The shell's half of ADR 0002 mutual supervision: the watchdog supervises
/// us, and we relaunch the watchdog if *it* dies. The relaunched watchdog then
/// kills us (its stray sweep) and spawns a fresh shell, so the pair converges
/// to exactly one of each.
#[cfg(windows)]
mod guardian {
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE,
    };

    use wr_core::guardian::{RelaunchState, RELAUNCH_STATE_ENV, WATCHDOG_PID_ENV};

    /// Start the monitor thread. No-op (with a log line) when we were not
    /// spawned by a watchdog, e.g. when run directly during development.
    pub fn watch_watchdog() {
        let pid = std::env::var(WATCHDOG_PID_ENV)
            .ok()
            .and_then(|v| v.parse::<u32>().ok());
        match pid {
            Some(pid) => {
                std::thread::spawn(move || monitor(pid));
            }
            None => log::info!("{WATCHDOG_PID_ENV} not set; watchdog monitor disabled"),
        }
    }

    fn monitor(watchdog_pid: u32) {
        // NOTE: a PID-reuse race is possible if the watchdog dies before we
        // open the handle. The window is milliseconds at spawn time; Phase 1's
        // pipe heartbeat removes it. Accepted for Phase 0 (ADR 0002).
        let handle = match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, watchdog_pid) } {
            Ok(h) => h,
            Err(e) => {
                log::error!("cannot watch watchdog (pid {watchdog_pid}): {e}");
                return;
            }
        };
        log::info!("watching watchdog (pid {watchdog_pid})");
        unsafe {
            WaitForSingleObject(handle, INFINITE);
            let _ = CloseHandle(handle);
        }

        // The watchdog is gone — with it the emergency hotkey and our own
        // supervision. Relaunch it, unless it is crash-looping.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let state = RelaunchState::parse(std::env::var(RELAUNCH_STATE_ENV).ok().as_deref());
        let state = state.bump(now);
        if state.exhausted() {
            log::error!(
                "watchdog crash-loop ({} relaunches); restoring Windows",
                state.count - 1
            );
            restore_windows_and_exit();
        }

        log::warn!(
            "watchdog (pid {watchdog_pid}) died; relaunching it (attempt {})",
            state.count
        );
        let watchdog_exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("wr-watchdog.exe")));
        let Some(watchdog_exe) = watchdog_exe else {
            log::error!("cannot locate wr-watchdog.exe next to wr-shell.exe");
            restore_windows_and_exit();
        };
        match Command::new(&watchdog_exe)
            .env(RELAUNCH_STATE_ENV, state.to_env_value())
            .spawn()
        {
            // Nothing more to do: the new watchdog kills us (stray sweep) and
            // spawns a fresh shell. Just keep running until then.
            Ok(child) => log::info!("watchdog relaunched (pid {})", child.id()),
            Err(e) => {
                log::error!("failed to relaunch watchdog: {e}");
                restore_windows_and_exit();
            }
        }
    }

    /// Last resort with no watchdog to lean on: put explorer back ourselves.
    fn restore_windows_and_exit() -> ! {
        match wr_core::shell::restore_shell() {
            Ok(outcome) => log::info!("registry restore: {outcome:?}"),
            Err(e) => log::error!("registry restore FAILED: {e:#}"),
        }
        match Command::new("explorer.exe").spawn() {
            Ok(_) => log::info!("launched explorer.exe"),
            Err(e) => log::error!("failed to launch explorer.exe: {e}"),
        }
        std::process::exit(1);
    }
}

#[derive(Default)]
struct Options {
    crash_after: Option<Duration>,
    exit_after: Option<Duration>,
}

impl Options {
    fn from_args(args: impl Iterator<Item = String>) -> Self {
        let mut opts = Options::default();
        for arg in args {
            if let Some(v) = arg.strip_prefix("--crash-after=") {
                opts.crash_after = v.parse().ok().map(Duration::from_secs);
            } else if let Some(v) = arg.strip_prefix("--exit-after=") {
                opts.exit_after = v.parse().ok().map(Duration::from_secs);
            }
        }
        opts
    }
}

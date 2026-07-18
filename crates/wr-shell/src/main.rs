//! WinRestyle shell host.
//!
//! ## Phase 0 status: deliberately a dummy.
//!
//! Right now this is a throwaway "I am the shell" process used only to exercise
//! the watchdog (relaunch, crash-loop fallback, emergency hotkey). It does NOT
//! paint a desktop or host a taskbar yet — that arrives in Phase 1/2.
//!
//! Test helpers (args):
//!   --crash-after=<secs>           panic after N seconds (to test relaunch/crash-loop)
//!   --exit-after=<secs>            exit cleanly after N seconds
//!   --hang-heartbeat-after=<secs>  stop heartbeating after N seconds while
//!                                  staying alive (simulates a hung shell; the
//!                                  watchdog should kill and relaunch us)
//!
//! The same flags are also read from the `WR_SHELL_TEST_ARGS` env var
//! (whitespace-separated), so tests can reach a shell the *watchdog* spawns:
//! `set WR_SHELL_TEST_ARGS=--hang-heartbeat-after=10` before running the
//! watchdog. Real CLI args win over env args.
//!
//! With a small `--crash-after`, the watchdog should relaunch us a few times
//! and then fall back to explorer.exe — that is the behavior we want to verify.

use std::time::{Duration, Instant};

// `ticks % 10 == 0` reads fine; `.is_multiple_of()` would raise our MSRV (1.77).
#[allow(clippy::manual_is_multiple_of)]
fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Env-var test args first, real CLI args second, so the CLI wins.
    let env_args = std::env::var("WR_SHELL_TEST_ARGS").unwrap_or_default();
    let opts = Options::from_args(
        env_args
            .split_whitespace()
            .map(String::from)
            .chain(std::env::args().skip(1)),
    );
    log::info!(
        "wr-shell (Phase 0 dummy) starting; pid {}",
        std::process::id()
    );

    // ADR 0002 mutual supervision: watch the watchdog and relaunch it if it
    // dies (Winlogon won't — AutoRestartShell ignores custom per-user shells).
    #[cfg(windows)]
    guardian::watch_watchdog();

    // ADR 0003: heartbeat over the pipe so a *hung* watchdog is detected too.
    #[cfg(windows)]
    guardian::start_heartbeat(opts.hang_heartbeat_after);
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
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, TerminateProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE,
        PROCESS_TERMINATE,
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

    /// Start the ADR 0003 heartbeat thread: send `ShellHeartbeat` every
    /// [`wr_ipc::HEARTBEAT_INTERVAL`] and expect acks. An ack-silent-but-alive
    /// watchdog is hung — kill it; the [`monitor`] thread then relaunches it
    /// through the ordinary death path. `hang_after` is a test hook that stops
    /// heartbeating (while staying alive) to simulate a hung *shell*.
    pub fn start_heartbeat(hang_after: Option<Duration>) {
        std::thread::spawn(move || heartbeat_loop(hang_after));
    }

    fn heartbeat_loop(hang_after: Option<Duration>) {
        let started = Instant::now();
        let mut logged_unavailable = false;
        loop {
            let mut conn = match wr_ipc::pipe::Connection::connect(wr_core::PIPE_NAME) {
                Ok(c) => c,
                Err(e) => {
                    if !logged_unavailable {
                        log::info!("watchdog pipe not available (running solo?): {e:#}");
                        logged_unavailable = true;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
            };
            logged_unavailable = false;
            log::info!("connected to watchdog pipe");

            let mut seq: u64 = 0;
            let mut last_ack = Instant::now();
            let mut watchdog_pid: Option<u32> = None;
            'connection: loop {
                if let Some(after) = hang_after {
                    if started.elapsed() >= after {
                        log::warn!("SIMULATING SHELL HANG: heartbeats stopped (test flag)");
                        loop {
                            std::thread::sleep(Duration::from_secs(60));
                        }
                    }
                }

                seq += 1;
                let beat = wr_ipc::ToWatchdog::ShellHeartbeat {
                    seq,
                    pid: std::process::id(),
                };
                if conn.send(&beat).is_err() {
                    // A broken pipe is a *dead* watchdog: the monitor thread
                    // handles that. Just try to reach its successor.
                    log::warn!("pipe write failed (watchdog gone); reconnecting");
                    break 'connection;
                }

                // Drain acks/commands until the next beat is due.
                let next_beat = Instant::now() + wr_ipc::HEARTBEAT_INTERVAL;
                while Instant::now() < next_beat {
                    match conn.try_recv::<wr_ipc::ToShell>() {
                        Ok(Some(wr_ipc::ToShell::HeartbeatAck { pid, .. })) => {
                            last_ack = Instant::now();
                            watchdog_pid = Some(pid);
                        }
                        Ok(Some(wr_ipc::ToShell::Shutdown)) => {
                            log::info!("shutdown requested over IPC");
                            std::process::exit(0);
                        }
                        Ok(Some(wr_ipc::ToShell::ReloadConfig)) => {
                            log::info!("ReloadConfig received (no config yet — Phase 1)");
                        }
                        Ok(None) => std::thread::sleep(Duration::from_millis(200)),
                        Err(_) => {
                            log::warn!("pipe read failed (watchdog gone); reconnecting");
                            break 'connection;
                        }
                    }
                }

                // The pipe is open but nothing answers: the watchdog is alive
                // and wedged — with a dead hotkey and dead supervision. Kill it
                // (the ack pid is authoritative, unlike the launch-time env
                // var); the monitor thread relaunches it.
                if last_ack.elapsed() > wr_ipc::HEARTBEAT_TIMEOUT {
                    match watchdog_pid {
                        Some(pid) => {
                            log::error!(
                                "watchdog silent for >{:?} on a live pipe; killing hung \
                                 watchdog (pid {pid}) so the monitor relaunches it",
                                wr_ipc::HEARTBEAT_TIMEOUT
                            );
                            kill_process(pid);
                        }
                        // Never acked: can't tell a hang from a peer that is
                        // not our watchdog. Leave death detection to the
                        // monitor thread.
                        None => log::warn!("no heartbeat ack ever received; reconnecting"),
                    }
                    break 'connection;
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    fn kill_process(pid: u32) {
        match unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
            Ok(handle) => unsafe {
                if let Err(e) = TerminateProcess(handle, 1) {
                    log::error!("failed to terminate pid {pid}: {e}");
                }
                let _ = CloseHandle(handle);
            },
            Err(e) => log::error!("failed to open pid {pid} for terminate: {e}"),
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
    #[cfg_attr(not(windows), allow(dead_code))]
    hang_heartbeat_after: Option<Duration>,
}

impl Options {
    fn from_args(args: impl Iterator<Item = String>) -> Self {
        let mut opts = Options::default();
        for arg in args {
            if let Some(v) = arg.strip_prefix("--crash-after=") {
                opts.crash_after = v.parse().ok().map(Duration::from_secs);
            } else if let Some(v) = arg.strip_prefix("--exit-after=") {
                opts.exit_after = v.parse().ok().map(Duration::from_secs);
            } else if let Some(v) = arg.strip_prefix("--hang-heartbeat-after=") {
                opts.hang_heartbeat_after = v.parse().ok().map(Duration::from_secs);
            }
        }
        opts
    }
}

//! WinRestyle watchdog — the guardian process.
//!
//! This is the safety backbone of the whole project. It runs as a *separate*
//! process from `wr-shell` so it survives a shell crash or hang. It:
//!
//! 1. Registers the global emergency hotkey (`Win + Ctrl + F1`). On press it
//!    restores `explorer.exe` and exits the custom shell.
//! 2. Launches and supervises `wr-shell`, relaunching it if it exits.
//! 3. Detects a crash-loop (too many exits too fast) and falls back to
//!    `explorer.exe` instead of relaunching forever.
//!
//! The watchdog's *own* crash recovery is **mutual supervision**: `wr-shell`
//! watches our process and relaunches us if we die (Winlogon's
//! `AutoRestartShell` does NOT restart a custom per-user shell — T5 disproved
//! that; see ADR 0002 in `docs/decisions/`). On startup we kill any stray
//! `wr-shell` a previous watchdog instance left behind, so a relaunch always
//! converges back to exactly one watchdog + one shell.
//!
//! ## Phase 0 status
//!
//! This is the make-or-break spike. The exact mechanism for restoring the
//! *full* explorer shell mid-session is the key unknown (see
//! `docs/ARCHITECTURE.md`); the [`recover`] function is where we iterate on it.
//!
//! Build and test this on Windows 11 only (in a VM with snapshots).

// The watchdog has no UI; on Windows we'd normally mark it
// `#![windows_subsystem = "windows"]`, but keep the console during Phase 0 so
// logs are visible while iterating.

#[cfg(not(windows))]
fn main() {
    eprintln!("wr-watchdog only runs on Windows 11.");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    win::run()
}

#[cfg(windows)]
mod win {
    use anyhow::{Context, Result};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, UnregisterHotKey, MOD_CONTROL, MOD_NOREPEAT, MOD_WIN, VK_F1,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, PostThreadMessageW, MSG, WM_HOTKEY, WM_QUIT,
    };

    const HOTKEY_ID: i32 = 1;

    /// Crash-loop policy: more than `CRASH_LIMIT` shell exits within
    /// `CRASH_WINDOW` means "give up and restore explorer".
    const CRASH_LIMIT: usize = 3;
    const CRASH_WINDOW: Duration = Duration::from_secs(20);

    /// A healthy pipe thread stamps `pipe_alive` every ~200ms; if it hasn't
    /// within this bound, its heartbeat bookkeeping cannot be trusted. Kept
    /// well under `wr_ipc::HEARTBEAT_TIMEOUT` (see the T9 race note below) and
    /// generous enough for scheduling stalls.
    const PIPE_OBSERVING_BOUND: Duration = Duration::from_secs(2);

    /// Shared guardian state, cloneable across the supervisor thread and the
    /// hotkey handler on the main thread.
    #[derive(Clone)]
    struct Guardian {
        /// Set when we are intentionally tearing down (hotkey or crash-loop).
        shutting_down: Arc<AtomicBool>,
        /// Ensures recovery (restore + launch explorer) runs exactly once.
        recovered: Arc<AtomicBool>,
        /// The currently running shell child, so the hotkey path can kill it.
        child: Arc<Mutex<Option<Child>>>,
        /// When the current shell last heartbeated over the pipe. `None` until
        /// it does — a shell that never connects is degraded but alive, and is
        /// never killed for silence (ADR 0003).
        shell_heartbeat: Arc<Mutex<Option<Instant>>>,
        /// Liveness stamp of the pipe-server thread while a client is
        /// connected. Stale heartbeats are only evidence against the *shell*
        /// if the thread recording them is itself alive — otherwise the
        /// watchdog is the hung one and would kill an innocent shell (found
        /// the hard way in T9).
        pipe_alive: Arc<Mutex<Option<Instant>>>,
        /// Liveness stamp of the supervisor thread. The pipe thread refuses to
        /// ack heartbeats while this is stale: a wedged supervisor (the Phase 0
        /// deadlock!) must look *hung* to the shell, not healthy.
        supervisor_alive: Arc<Mutex<Instant>>,
        /// Main thread id, so background threads can end the message loop.
        main_thread: u32,
    }

    pub fn run() -> Result<()> {
        let shell_exe = shell_exe_path().context("locating wr-shell.exe")?;
        log::info!("watchdog starting; shell = {}", shell_exe.display());

        // ADR 0002: if wr-shell just relaunched us after a crash, that shell —
        // and possibly the crashed watchdog's other strays — are still running
        // (Windows does not kill orphans). Spawning another shell would give
        // the user two desktops — sweep first.
        kill_stray_shells(&shell_exe);

        let guardian = Guardian {
            shutting_down: Arc::new(AtomicBool::new(false)),
            recovered: Arc::new(AtomicBool::new(false)),
            child: Arc::new(Mutex::new(None)),
            shell_heartbeat: Arc::new(Mutex::new(None)),
            pipe_alive: Arc::new(Mutex::new(None)),
            supervisor_alive: Arc::new(Mutex::new(Instant::now())),
            main_thread: unsafe { GetCurrentThreadId() },
        };

        // Supervisor thread owns the shell lifecycle.
        let supervisor = {
            let g = guardian.clone();
            std::thread::spawn(move || supervise_shell(&g, &shell_exe))
        };

        // Pipe server: heartbeats from the shell, commands from the installer.
        // VM-test-only flags: `--ack-hang-after=<secs>` freezes it to fake a
        // hang; `--send-reload-every=<secs>` sends the shell `ReloadConfig`
        // periodically (nothing sends it for real until the Phase 3 installer),
        // so T10 can exercise the hot-reload path end to end.
        {
            let g = guardian.clone();
            let ack_hang_after = std::env::args()
                .find_map(|a| a.strip_prefix("--ack-hang-after=")?.parse().ok())
                .map(Duration::from_secs);
            let send_reload_every = std::env::args()
                .find_map(|a| a.strip_prefix("--send-reload-every=")?.parse().ok())
                .map(Duration::from_secs);
            std::thread::spawn(move || serve_pipe(&g, ack_hang_after, send_reload_every));
        }

        // Main thread: register the emergency hotkey and pump messages.
        register_hotkey().context("registering emergency hotkey")?;
        log::info!(
            "emergency hotkey registered: {}",
            wr_core::EMERGENCY_HOTKEY_LABEL
        );

        message_loop(&guardian);

        // Tear down.
        let _ = unsafe { UnregisterHotKey(None, HOTKEY_ID) };
        guardian.shutting_down.store(true, Ordering::SeqCst);
        kill_child(&guardian);
        let _ = supervisor.join();
        log::info!("watchdog exited");
        Ok(())
    }

    /// Spawn + monitor the shell, relaunching on exit and bailing on crash-loop.
    fn supervise_shell(g: &Guardian, shell_exe: &PathBuf) {
        let mut crashes: Vec<Instant> = Vec::new();

        while !g.shutting_down.load(Ordering::SeqCst) {
            *g.supervisor_alive.lock().unwrap() = Instant::now();
            // ADR 0002 mutual supervision: tell the shell which process to
            // watch. WR_WD_RELAUNCH_STATE (if a shell relaunched us) is passed
            // along implicitly via normal env inheritance.
            let child = match Command::new(shell_exe)
                .env(
                    wr_core::guardian::WATCHDOG_PID_ENV,
                    std::process::id().to_string(),
                )
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    log::error!("failed to launch shell: {e}");
                    recover(g, "shell would not launch");
                    end_main_loop(g);
                    return;
                }
            };
            log::info!("shell launched (pid {})", child.id());
            // A fresh shell starts with a clean heartbeat slate.
            *g.shell_heartbeat.lock().unwrap() = None;
            *g.child.lock().unwrap() = Some(child);

            // Wait for the shell to exit WITHOUT holding the lock across the
            // blocking wait. The emergency-hotkey path (recover -> kill_child)
            // needs this same lock to terminate a *still-running* child; holding
            // it across a blocking `wait()` would deadlock recovery — the exact
            // failure that leaves the desktop stuck with no way back. So poll
            // `try_wait` and drop the lock between polls.
            let status = loop {
                if g.shutting_down.load(Ordering::SeqCst) {
                    // Recovery is tearing us down and owns killing the child.
                    return;
                }
                *g.supervisor_alive.lock().unwrap() = Instant::now();
                let mut guard = g.child.lock().unwrap();
                match guard.as_mut() {
                    Some(c) => match c.try_wait() {
                        Ok(Some(status)) => {
                            *guard = None;
                            break status;
                        }
                        Ok(None) => {
                            // Alive — but is it *responsive*? A shell that
                            // heartbeated before and has now gone silent is
                            // hung; kill it and let the normal exit path
                            // (relaunch + crash-loop accounting) take over.
                            let hung = g
                                .shell_heartbeat
                                .lock()
                                .unwrap()
                                .is_some_and(|t| t.elapsed() > wr_ipc::HEARTBEAT_TIMEOUT);
                            if hung {
                                // Stale heartbeats only incriminate the shell
                                // if the pipe thread recording them is alive
                                // *right now* — a healthy one stamps every
                                // ~200ms even with no traffic. Demanding it be
                                // stale by the full heartbeat timeout instead
                                // loses the race by design: the last beat is
                                // always up to a beat interval older than the
                                // last stamp, so "beats stale" fires first and
                                // the innocent shell gets shot (seen in T9).
                                // If the pipe thread is not observing, *we*
                                // are the hung process: exit, so the shell's
                                // monitor relaunches a fresh watchdog
                                // (ADR 0003 — convert hangs to deaths).
                                let pipe_observing = g
                                    .pipe_alive
                                    .lock()
                                    .unwrap()
                                    .is_some_and(|t| t.elapsed() < PIPE_OBSERVING_BOUND);
                                if !pipe_observing {
                                    log::error!(
                                        "own pipe thread stopped observing; exiting so the \
                                         shell relaunches a fresh watchdog"
                                    );
                                    std::process::exit(2);
                                }
                                log::error!(
                                    "shell heartbeat silent for >{:?}; killing hung shell",
                                    wr_ipc::HEARTBEAT_TIMEOUT
                                );
                                *g.shell_heartbeat.lock().unwrap() = None;
                                let _ = c.kill();
                            }
                            drop(guard); // release before sleeping
                            std::thread::sleep(Duration::from_millis(200));
                        }
                        Err(e) => {
                            *guard = None;
                            drop(guard);
                            log::error!("waiting on shell failed: {e}");
                            recover(g, "waiting on shell failed");
                            end_main_loop(g);
                            return;
                        }
                    },
                    None => return, // taken & killed by the recovery path
                }
            };

            if g.shutting_down.load(Ordering::SeqCst) {
                break;
            }
            log::warn!("shell exited unexpectedly: {status:?}");

            // Crash-loop accounting.
            let now = Instant::now();
            crashes.retain(|t| now.duration_since(*t) < CRASH_WINDOW);
            crashes.push(now);
            if crashes.len() > CRASH_LIMIT {
                log::error!(
                    "shell crash-loop: {} exits within {:?}; falling back to explorer",
                    crashes.len(),
                    CRASH_WINDOW
                );
                recover(g, "shell crash-loop");
                end_main_loop(g);
                return;
            }
            log::info!("relaunching shell ({} recent exits)", crashes.len());
        }
    }

    /// The emergency-restore action: put the original shell back and bring the
    /// desktop back up. Idempotent — runs at most once.
    ///
    /// PHASE 0 SPIKE: validate that launching `explorer.exe` here reliably
    /// re-adopts the shell role mid-session. If it only opens a file window, we
    /// will switch strategies (e.g. restore registry, then trigger a controlled
    /// re-logon). This function is the experiment.
    fn recover(g: &Guardian, reason: &str) {
        if g.recovered.swap(true, Ordering::SeqCst) {
            return; // already recovered
        }
        g.shutting_down.store(true, Ordering::SeqCst);
        log::warn!("EMERGENCY RECOVER ({reason})");

        kill_child(g);

        match wr_core::shell::restore_shell() {
            Ok(outcome) => log::info!("registry restore: {outcome:?}"),
            Err(e) => log::error!("registry restore FAILED: {e:#}"),
        }

        if wr_core::shell::desktop_shell_running() {
            // A live shell (explorer's taskbar) is already on screen — a
            // second explorer would only open a file-manager window.
            log::info!("desktop shell already running; not launching explorer");
        } else {
            match Command::new("explorer.exe").spawn() {
                Ok(_) => log::info!("launched explorer.exe"),
                Err(e) => log::error!("failed to launch explorer.exe: {e}"),
            }
        }
    }

    /// Host the `wr-ipc` pipe: track shell heartbeats (ADR 0003) and serve
    /// commands. Serves one client at a time and survives client churn — every
    /// relaunched shell reconnects to the same server.
    fn serve_pipe(
        g: &Guardian,
        ack_hang_after: Option<Duration>,
        send_reload_every: Option<Duration>,
    ) {
        let mut server = match wr_ipc::pipe::Server::create(wr_core::PIPE_NAME) {
            Ok(s) => s,
            Err(e) => {
                // Heartbeats are an upgrade, not a requirement: without the
                // pipe we degrade to Phase 0 behavior (death-only detection).
                log::error!("pipe server failed; hang detection disabled: {e:#}");
                return;
            }
        };
        log::info!("pipe server up at {}", wr_core::PIPE_NAME);
        let started = Instant::now();

        loop {
            if g.shutting_down.load(Ordering::SeqCst) {
                return;
            }
            if let Err(e) = server.wait_for_client() {
                log::warn!("pipe accept failed: {e:#}");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            log::info!("pipe client connected");

            // Timer for `--send-reload-every`; per-connection so a freshly
            // connected shell gets a full interval before its first reload.
            let mut last_reload = Instant::now();

            loop {
                if g.shutting_down.load(Ordering::SeqCst) {
                    return;
                }
                *g.pipe_alive.lock().unwrap() = Some(Instant::now());
                if let Some(every) = send_reload_every {
                    if last_reload.elapsed() >= every {
                        last_reload = Instant::now();
                        log::info!("sending ReloadConfig to the shell (test flag)");
                        if let Err(e) = server.send(&wr_ipc::ToShell::ReloadConfig) {
                            log::warn!("test ReloadConfig send failed: {e:#}");
                            break;
                        }
                    }
                }
                if let Some(after) = ack_hang_after {
                    if started.elapsed() >= after {
                        log::warn!("SIMULATING WATCHDOG HANG: pipe server frozen (test flag)");
                        loop {
                            std::thread::sleep(Duration::from_secs(60));
                        }
                    }
                }
                match server.try_recv::<wr_ipc::ToWatchdog>() {
                    Ok(Some(wr_ipc::ToWatchdog::ShellHeartbeat { seq, pid })) => {
                        *g.shell_heartbeat.lock().unwrap() = Some(Instant::now());
                        // An ack vouches for the whole watchdog, not just this
                        // thread. If the supervisor is wedged (the Phase 0
                        // deadlock shape: hotkey recovery dead, this thread
                        // fine), withhold acks — to the shell we must look
                        // hung, so it kills and relaunches us (ADR 0003).
                        let supervisor_wedged = g.supervisor_alive.lock().unwrap().elapsed()
                            > wr_ipc::HEARTBEAT_TIMEOUT;
                        if supervisor_wedged {
                            log::error!(
                                "supervisor thread is wedged; withholding acks so the \
                                 shell treats this watchdog as hung and replaces it"
                            );
                            continue;
                        }
                        let ack = wr_ipc::ToShell::HeartbeatAck {
                            seq,
                            pid: std::process::id(),
                        };
                        if let Err(e) = server.send(&ack) {
                            log::warn!("heartbeat ack to pid {pid} failed: {e:#}");
                            break;
                        }
                    }
                    Ok(Some(wr_ipc::ToWatchdog::RequestRestore)) => {
                        recover(g, "restore requested over IPC");
                        end_main_loop(g);
                        return;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(200)),
                    Err(_) => break, // client gone
                }
            }

            // The dead client's last heartbeat must not get its successor
            // killed for "silence" it never produced; likewise our own stamp
            // only means something while a client is connected.
            *g.shell_heartbeat.lock().unwrap() = None;
            *g.pipe_alive.lock().unwrap() = None;
            server.disconnect();
            log::info!("pipe client disconnected");
        }
    }

    /// Kill any `wr-shell.exe` left over from a previous watchdog instance.
    /// Runs once at startup, before we spawn our own child.
    fn kill_stray_shells(shell_exe: &Path) {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };
        use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

        let target = match shell_exe.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_ascii_lowercase(),
            None => return,
        };

        let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
            Ok(s) => s,
            Err(e) => {
                log::warn!("stray-shell sweep skipped: process snapshot failed: {e}");
                return;
            }
        };

        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok();
        while more {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_ascii_lowercase();
            if name == target {
                let pid = entry.th32ProcessID;
                log::warn!("killing stray {target} (pid {pid}) from a previous watchdog");
                match unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
                    Ok(process) => unsafe {
                        if let Err(e) = TerminateProcess(process, 1) {
                            log::error!("failed to kill stray pid {pid}: {e}");
                        }
                        let _ = CloseHandle(process);
                    },
                    Err(e) => log::error!("failed to open stray pid {pid}: {e}"),
                }
            }
            more = unsafe { Process32NextW(snapshot, &mut entry) }.is_ok();
        }
        unsafe {
            let _ = CloseHandle(snapshot);
        }
    }

    fn kill_child(g: &Guardian) {
        if let Some(mut c) = g.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// End the main thread's message loop from a background thread.
    fn end_main_loop(g: &Guardian) {
        unsafe {
            let _ = PostThreadMessageW(g.main_thread, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }

    fn register_hotkey() -> Result<()> {
        // `None` HWND posts WM_HOTKEY to this thread's message queue.
        unsafe {
            RegisterHotKey(
                None,
                HOTKEY_ID,
                MOD_CONTROL | MOD_WIN | MOD_NOREPEAT,
                VK_F1.0 as u32,
            )?;
        }
        Ok(())
    }

    fn message_loop(g: &Guardian) {
        let mut msg = MSG::default();
        // GetMessageW returns 0 on WM_QUIT, -1 on error, >0 otherwise.
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
            if msg.message == WM_HOTKEY && msg.wParam.0 as i32 == HOTKEY_ID {
                log::warn!("emergency hotkey pressed");
                recover(g, "emergency hotkey");
                break;
            }
            unsafe {
                let _ = DispatchMessageW(&msg);
            }
        }
    }

    /// `wr-shell.exe` is expected to sit next to the watchdog binary.
    fn shell_exe_path() -> Result<PathBuf> {
        let exe = std::env::current_exe()?;
        let dir = exe.parent().context("watchdog has no parent directory")?;
        Ok(dir.join("wr-shell.exe"))
    }
}

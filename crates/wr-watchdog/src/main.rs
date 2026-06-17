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
    use std::path::PathBuf;
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
        /// Main thread id, so background threads can end the message loop.
        main_thread: u32,
    }

    pub fn run() -> Result<()> {
        let shell_exe = shell_exe_path().context("locating wr-shell.exe")?;
        log::info!("watchdog starting; shell = {}", shell_exe.display());

        let guardian = Guardian {
            shutting_down: Arc::new(AtomicBool::new(false)),
            recovered: Arc::new(AtomicBool::new(false)),
            child: Arc::new(Mutex::new(None)),
            main_thread: unsafe { GetCurrentThreadId() },
        };

        // Supervisor thread owns the shell lifecycle.
        let supervisor = {
            let g = guardian.clone();
            std::thread::spawn(move || supervise_shell(&g, &shell_exe))
        };

        // Main thread: register the emergency hotkey and pump messages.
        register_hotkey().context("registering emergency hotkey")?;
        log::info!("emergency hotkey registered: {}", wr_core::EMERGENCY_HOTKEY_LABEL);

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
            let child = match Command::new(shell_exe).spawn() {
                Ok(c) => c,
                Err(e) => {
                    log::error!("failed to launch shell: {e}");
                    recover(g, "shell would not launch");
                    end_main_loop(g);
                    return;
                }
            };
            log::info!("shell launched (pid {})", child.id());
            *g.child.lock().unwrap() = Some(child);

            // Block until it exits (the hotkey path may kill it from elsewhere).
            let status = {
                let mut guard = g.child.lock().unwrap();
                match guard.as_mut() {
                    Some(c) => c.wait(),
                    None => break, // killed and taken by the hotkey path
                }
            };
            *g.child.lock().unwrap() = None;

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

        match Command::new("explorer.exe").spawn() {
            Ok(_) => log::info!("launched explorer.exe"),
            Err(e) => log::error!("failed to launch explorer.exe: {e}"),
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
        let dir = exe
            .parent()
            .context("watchdog has no parent directory")?;
        Ok(dir.join("wr-shell.exe"))
    }
}

//! Child surfaces (ADR 0005): the shell spawns and supervises `wr-taskbar.exe`.
//!
//! Surfaces are **cosmetic**. The policy is deliberately weaker than the
//! watchdog⇄shell supervision: a dead taskbar is relaunched, a crash-looping
//! taskbar makes the shell *give up on the taskbar* — with a logged error —
//! and nothing else. A missing taskbar never justifies touching the registry,
//! recovery, or the safety harness (never add a second recovery mechanism).
//!
//! Same concurrency invariant as the watchdog's supervisor: never hold the
//! child-handle lock across a blocking wait. Shutdown paths take that same
//! lock to kill a *running* taskbar; we poll `try_wait` and release between
//! polls.

use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, RegisterWindowMessageW};

use wr_core::config::ConfigStore;

/// Crash-loop policy: more than this many exits within the window means
/// "give up on the taskbar until the next shell start".
const CRASH_LIMIT: usize = 3;
const CRASH_WINDOW: Duration = Duration::from_secs(20);

/// The running taskbar child, shared with [`shutdown`].
static TASKBAR_CHILD: Mutex<Option<Child>> = Mutex::new(None);

/// Set when the shell is going away on purpose; stops the supervisor from
/// relaunching the taskbar it is about to kill.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// Sweep strays and start the taskbar supervisor thread. Never fails; a
/// surface that cannot run is a logged error, not a shell problem.
pub fn start(store: Arc<ConfigStore>) {
    std::thread::spawn(move || supervise(store));
}

/// Kill the taskbar because the shell itself is exiting on purpose (IPC
/// `Shutdown`, last-resort restore). Safe from any thread; idempotent.
pub fn shutdown() {
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
    if let Some(mut child) = TASKBAR_CHILD.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    // Belt and braces: a stray from a previous shell generation would sit on
    // top of the restored desktop otherwise.
    wr_core::process::kill_all_named(wr_core::TASKBAR_EXE);
}

/// Tell the taskbar the config changed (safe from any thread). A no-op while
/// no taskbar window exists — a freshly spawned one reads a fresh config
/// anyway. Enable/disable transitions are handled by the supervisor's own
/// config polling, not this message.
pub fn notify_config_changed() {
    static MSG_ID: OnceLock<u32> = OnceLock::new();
    let msg = *MSG_ID.get_or_init(|| {
        let name = wide(wr_core::CONFIG_CHANGED_MESSAGE);
        unsafe { RegisterWindowMessageW(PCWSTR(name.as_ptr())) }
    });
    if msg == 0 {
        return;
    }
    let class = wide(wr_core::TASKBAR_WINDOW_CLASS);
    if let Ok(hwnd) = unsafe { FindWindowW(PCWSTR(class.as_ptr()), None) } {
        if !hwnd.is_invalid() {
            unsafe {
                let _ = PostMessageW(hwnd, msg, WPARAM(0), LPARAM(0));
            }
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// What ended one round of watching the child.
enum Watch {
    /// The child exited (or waiting on it failed) — counts toward the cap.
    Exited,
    /// We stopped it on purpose (disabled in config) or another thread took
    /// the handle — not a crash.
    Stopped,
}

fn supervise(store: Arc<ConfigStore>) {
    // A stray taskbar from a crashed previous shell would double up with the
    // one we are about to spawn (Windows does not kill orphans).
    wr_core::process::kill_all_named(wr_core::TASKBAR_EXE);

    let taskbar_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(wr_core::TASKBAR_EXE)));
    let Some(taskbar_exe) = taskbar_exe else {
        log::error!(
            "cannot locate {} next to wr-shell.exe",
            wr_core::TASKBAR_EXE
        );
        return;
    };

    let mut crashes: Vec<Instant> = Vec::new();
    let mut logged_disabled = false;
    loop {
        if SHUTTING_DOWN.load(Ordering::SeqCst) {
            return;
        }
        if !store.get().taskbar.enabled {
            if !logged_disabled {
                log::info!("taskbar disabled in config; not spawning it");
                logged_disabled = true;
            }
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        logged_disabled = false;

        let child = match Command::new(&taskbar_exe).spawn() {
            Ok(c) => c,
            Err(e) => {
                log::error!(
                    "taskbar failed to launch ({}): {e}; giving up on the taskbar",
                    taskbar_exe.display()
                );
                return;
            }
        };
        log::info!("taskbar launched (pid {})", child.id());
        *TASKBAR_CHILD.lock().unwrap() = Some(child);

        match watch_child(&store) {
            Watch::Stopped => continue,
            Watch::Exited => {
                if SHUTTING_DOWN.load(Ordering::SeqCst) {
                    return;
                }
                let now = Instant::now();
                crashes.retain(|t| now.duration_since(*t) < CRASH_WINDOW);
                crashes.push(now);
                if crashes.len() > CRASH_LIMIT {
                    log::error!(
                        "taskbar crash-loop: {} exits within {:?}; giving up on the taskbar \
                         (the shell keeps running)",
                        crashes.len(),
                        CRASH_WINDOW
                    );
                    return;
                }
                log::info!("relaunching taskbar ({} recent exits)", crashes.len());
            }
        }
    }
}

/// Poll the child until it exits or should be stopped. Never holds the lock
/// across a sleep or a blocking wait.
fn watch_child(store: &ConfigStore) -> Watch {
    loop {
        {
            let mut guard = TASKBAR_CHILD.lock().unwrap();
            match guard.as_mut() {
                Some(child) => match child.try_wait() {
                    Ok(Some(status)) => {
                        *guard = None;
                        log::warn!("taskbar exited unexpectedly: {status:?}");
                        return Watch::Exited;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        *guard = None;
                        log::error!("waiting on taskbar failed: {e}");
                        return Watch::Exited;
                    }
                },
                // Taken and killed by a shutdown path.
                None => return Watch::Stopped,
            }
        }
        if SHUTTING_DOWN.load(Ordering::SeqCst) {
            return Watch::Stopped;
        }
        if !store.get().taskbar.enabled {
            log::info!("taskbar disabled in config; stopping it");
            if let Some(mut child) = TASKBAR_CHILD.lock().unwrap().take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            return Watch::Stopped;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

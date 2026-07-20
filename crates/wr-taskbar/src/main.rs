//! WinRestyle taskbar — the Phase 2 flagship surface.
//!
//! ## Current state
//!
//! A rounded, translucent, config-themed bar (Direct2D on a DirectComposition
//! swapchain, WARP fallback for GPU-less VMs) at the bottom of **every
//! monitor** (per-monitor DPI, display-change rebuild). Left to right:
//! Start button — opens the start menu (`startmenu` + `apps`, ADR 0007): a
//! popup listing the Start Menu folders' shortcuts with type-to-filter,
//! arrows/Enter, click-to-launch — pinned-app chips (config `pinned`, click
//! to launch), window buttons — icon + ellipsized title, hover + foreground
//! highlights, click to activate/minimize/restore, kept fresh event-driven
//! via WinEvent hooks (`winlist`) — an overflow `»` menu when they don't
//! fit, tray icons (hosted `Shell_NotifyIcon`, swapped sessions only; ADR
//! 0005 amendment), and the clock with a date line. Optional DWM
//! acrylic/mica backdrop and `text_color` theming. The start menu's icon
//! column shows real shortcut icons, decoded off the UI thread (`iconload`,
//! ADR 0007 amendment 3). Per-app grouping, the appbar channel, and balloons
//! are later polish — see `docs/ROADMAP.md`.
//!
//! Spawned and supervised by `wr-shell` (ADR 0005). The taskbar is cosmetic:
//! a crash here is relaunched by the shell, and a crash-loop makes the shell
//! give up on the taskbar — never anything more. Nothing in this process may
//! touch the registry or the safety harness.
//!
//! Test helpers (args, also via the `WR_TASKBAR_TEST_ARGS` env var,
//! whitespace-separated; real CLI args win):
//!   --crash-after=<secs>  abort after N seconds (exercises the shell's
//!                         relaunch and crash-loop-give-up paths)
//!   --exit-after=<secs>   exit cleanly after N seconds

#[cfg_attr(not(windows), allow(dead_code))]
mod actions;
#[cfg_attr(not(windows), allow(dead_code))]
mod apps;
#[cfg(windows)]
mod bar;
#[cfg_attr(not(windows), allow(dead_code))]
mod iconload;
#[cfg_attr(not(windows), allow(dead_code))]
mod layout;
#[cfg(windows)]
mod render;
#[cfg_attr(not(windows), allow(dead_code))]
mod startmenu;
#[cfg_attr(not(windows), allow(dead_code))]
mod tasks;
#[cfg_attr(not(windows), allow(dead_code))]
mod tray;
#[cfg(windows)]
mod winlist;

#[cfg(not(windows))]
fn main() {
    eprintln!("wr-taskbar only runs on Windows 11.");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Env-var test args first, real CLI args second, so the CLI wins.
    let env_args = std::env::var("WR_TASKBAR_TEST_ARGS").unwrap_or_default();
    let opts = Options::from_args(
        env_args
            .split_whitespace()
            .map(String::from)
            .chain(std::env::args().skip(1)),
    );
    log::info!("wr-taskbar starting; pid {}", std::process::id());
    opts.spawn_test_threads();

    // Same rules as the shell: config can never block startup.
    let store = std::sync::Arc::new(wr_core::config::ConfigStore::load_default());

    if let Err(e) = bar::run(store) {
        log::error!("taskbar failed: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
#[derive(Default)]
struct Options {
    crash_after: Option<std::time::Duration>,
    exit_after: Option<std::time::Duration>,
}

#[cfg(windows)]
impl Options {
    fn from_args(args: impl Iterator<Item = String>) -> Self {
        let mut opts = Options::default();
        for arg in args {
            if let Some(v) = arg.strip_prefix("--crash-after=") {
                opts.crash_after = v.parse().ok().map(std::time::Duration::from_secs);
            } else if let Some(v) = arg.strip_prefix("--exit-after=") {
                opts.exit_after = v.parse().ok().map(std::time::Duration::from_secs);
            }
        }
        opts
    }

    /// The test flags run off-thread so they fire regardless of what the
    /// message pump is doing. `abort` (not panic) so the crash is a crash in
    /// every build profile.
    fn spawn_test_threads(&self) {
        if let Some(after) = self.crash_after {
            std::thread::spawn(move || {
                std::thread::sleep(after);
                log::error!("wr-taskbar: simulated crash after {after:?}");
                std::process::abort();
            });
        }
        if let Some(after) = self.exit_after {
            std::thread::spawn(move || {
                std::thread::sleep(after);
                log::info!("wr-taskbar: clean exit after {after:?}");
                std::process::exit(0);
            });
        }
    }
}

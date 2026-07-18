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

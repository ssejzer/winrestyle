//! Async app-icon loader (Phase 4, ADR 0007 amendment): the start menu's icon
//! column is filled by a background thread, so `SHGetFileInfoW`'s disk- and
//! shell-bound work never blocks the UI thread that owns the bars and the
//! message pump. The UI thread hands the loader the shortcut paths it wants and
//! the menu window to poke; the loader decodes each icon off-thread and posts
//! `WM_APP_ICON_READY` back, where the UI thread drains the finished pixels and
//! repaints. Only the dedup bookkeeping (`needed`) is pure and unit-tested (it
//! runs on the Linux dev host); the thread + Win32 plumbing is thin and
//! Windows-only.

use std::collections::HashSet;
use std::path::PathBuf;

/// The paths in `apps` not yet handed to the loader. `requested` holds every
/// path ever requested — still decoding or already done — so this returns only
/// fresh work, deduped and in first-seen order. The caller inserts the result
/// into `requested` before dispatching it, so a later open with the same apps
/// (the list is re-scanned each time) asks for nothing.
pub fn needed(apps: &[PathBuf], requested: &HashSet<PathBuf>) -> Vec<PathBuf> {
    let mut fresh: HashSet<&PathBuf> = HashSet::new();
    apps.iter()
        .filter(|p| !requested.contains(*p) && fresh.insert(*p))
        .cloned()
        .collect()
}

#[cfg(windows)]
pub use imp::{IconLoader, WM_APP_ICON_READY};

#[cfg(windows)]
mod imp {
    use std::path::PathBuf;
    use std::sync::mpsc::{Receiver, Sender};

    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

    use crate::tasks::Icon;

    /// Posted (no payload) to the menu window when one or more icons have
    /// finished decoding; the handler drains [`IconLoader::drain`]. `WM_APP`
    /// based, so it never collides with a system message.
    pub const WM_APP_ICON_READY: u32 = WM_APP + 1;

    /// A decode job: the shortcut to read, and the menu window to poke after.
    struct Request {
        path: PathBuf,
        notify: isize,
    }

    /// Handle to the background icon-decoding thread. One per process, spawned
    /// at startup and living until the process ends — no join, no shutdown
    /// handshake. Dropping the request sender (only at process teardown) just
    /// ends the worker's loop cleanly.
    pub struct IconLoader {
        tx: Sender<Request>,
        rx: Receiver<(PathBuf, Option<Icon>)>,
    }

    impl IconLoader {
        /// Spawn the worker. `None` if the thread can't start — the menu then
        /// keeps its first-letter chips, never an error.
        pub fn spawn() -> Option<Self> {
            let (req_tx, req_rx) = std::sync::mpsc::channel::<Request>();
            let (res_tx, res_rx) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("wr-iconload".into())
                .spawn(move || worker(req_rx, res_tx))
                .map_err(|e| log::warn!("icon loader thread failed to start: {e}"))
                .ok()?;
            Some(IconLoader {
                tx: req_tx,
                rx: res_rx,
            })
        }

        /// Queue `paths` for decoding; each finished icon posts
        /// `WM_APP_ICON_READY` to `notify`. Non-blocking (an unbounded channel
        /// send), so — unlike a window send — it pumps nothing and is safe to
        /// call under a `STATE` borrow.
        pub fn request(&self, paths: Vec<PathBuf>, notify: isize) {
            for path in paths {
                if self.tx.send(Request { path, notify }).is_err() {
                    break; // worker gone; nothing more will decode
                }
            }
        }

        /// Take every icon decoded since the last drain.
        pub fn drain(&self) -> Vec<(PathBuf, Option<Icon>)> {
            self.rx.try_iter().collect()
        }
    }

    /// The worker loop: one COM apartment for the thread's life, then decode
    /// each requested icon and poke its owner. STA to match the main thread's
    /// apartment and the many shell icon handlers that assume one; the decode
    /// (`SHGetFileInfoW`) is synchronous and needs no message pump here.
    fn worker(req_rx: Receiver<Request>, res_tx: Sender<(PathBuf, Option<Icon>)>) {
        unsafe {
            if let Err(e) = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() {
                log::warn!("icon loader: CoInitializeEx failed: {e} (icons may not load)");
            }
        }
        while let Ok(Request { path, notify }) = req_rx.recv() {
            let icon = crate::winlist::pinned_icon(&path);
            if res_tx.send((path, icon)).is_err() {
                break; // UI side gone
            }
            // Poke the menu to drain. A stale handle (the menu is recreated on
            // a bar rebuild) just fails the post; those pixels wait in the
            // channel for the next successful drain.
            unsafe {
                let _ = PostMessageW(HWND(notify as _), WM_APP_ICON_READY, WPARAM(0), LPARAM(0));
            }
        }
        unsafe {
            CoUninitialize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn needed_returns_all_when_nothing_requested_yet() {
        let apps = [p("/a.lnk"), p("/b.lnk")];
        assert_eq!(
            needed(&apps, &HashSet::new()),
            vec![p("/a.lnk"), p("/b.lnk")]
        );
    }

    #[test]
    fn needed_skips_already_requested_paths() {
        let apps = [p("/a.lnk"), p("/b.lnk"), p("/c.lnk")];
        let requested: HashSet<_> = [p("/b.lnk")].into_iter().collect();
        assert_eq!(needed(&apps, &requested), vec![p("/a.lnk"), p("/c.lnk")]);
    }

    #[test]
    fn needed_dedups_within_one_call_preserving_first_seen_order() {
        let apps = [p("/a.lnk"), p("/b.lnk"), p("/a.lnk")];
        assert_eq!(
            needed(&apps, &HashSet::new()),
            vec![p("/a.lnk"), p("/b.lnk")]
        );
    }

    #[test]
    fn needed_is_empty_once_everything_is_requested() {
        let apps = [p("/a.lnk"), p("/b.lnk")];
        let requested: HashSet<_> = apps.iter().cloned().collect();
        assert!(needed(&apps, &requested).is_empty());
    }
}

//! The Win32 side of window buttons: which top-level windows deserve one,
//! event-driven change notification, and the click actions.
//!
//! Change tracking is out-of-context WinEvent hooks (delivered on this
//! process's message pump), coalesced into a single [`WM_WINDOWS_CHANGED`]
//! posted to the bar — the bar then re-enumerates and diffs. No polling:
//! an idle desktop costs nothing (the rendering doc's idle-cost goal).

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::Win32::Foundation::{BOOL, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetForegroundWindow, GetWindow, GetWindowLongPtrW, GetWindowTextW,
    IsIconic, IsWindowVisible, PostMessageW, SetForegroundWindow, ShowWindow, SwitchToThisWindow,
    EVENT_OBJECT_CLOAKED, EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE, EVENT_OBJECT_NAMECHANGE,
    EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND,
    EVENT_SYSTEM_MINIMIZESTART, GWL_EXSTYLE, GW_OWNER, OBJID_WINDOW, SW_MINIMIZE, SW_RESTORE,
    WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_APP, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};

use crate::tasks::{self, ClickAction, TaskWindow};

/// Posted (coalesced) to the bar whenever the window population may have
/// changed. The handler must call [`ack_refresh`] before re-enumerating.
pub const WM_WINDOWS_CHANGED: u32 = WM_APP + 2;

/// The bar window the hooks notify. Zero until [`install_hooks`].
static BAR_HWND: AtomicIsize = AtomicIsize::new(0);
/// True while a `WM_WINDOWS_CHANGED` is posted but not yet handled, so event
/// storms (a window opening fires dozens of object events) post only once.
static REFRESH_PENDING: AtomicBool = AtomicBool::new(false);

/// Snapshot the taskbar-worthy windows, in z-order.
pub fn enumerate() -> Vec<TaskWindow> {
    let mut out: Vec<TaskWindow> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(enum_proc),
            LPARAM(&mut out as *mut Vec<TaskWindow> as isize),
        );
    }
    out
}

extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let out = unsafe { &mut *(lparam.0 as *mut Vec<TaskWindow>) };
    if let Some(w) = task_window(hwnd) {
        out.push(w);
    }
    true.into()
}

/// The standard "does it get a taskbar button" rules (what alt-tab shows):
/// visible, not a tool window, unowned (unless it forces itself in with
/// `WS_EX_APPWINDOW`), not DWM-cloaked (suspended UWP), titled — and never
/// one of our own surfaces.
fn task_window(hwnd: HWND) -> Option<TaskWindow> {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return None;
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let appwindow = ex & WS_EX_APPWINDOW.0 != 0;
        if ex & WS_EX_TOOLWINDOW.0 != 0 && !appwindow {
            return None;
        }
        if !appwindow {
            let owned = GetWindow(hwnd, GW_OWNER)
                .map(|o| !o.is_invalid())
                .unwrap_or(false);
            if owned {
                return None;
            }
        }
        if cloaked(hwnd) {
            return None;
        }

        let mut buf = [0u16; 256];
        let len = GetClassNameW(hwnd, &mut buf);
        let class = String::from_utf16_lossy(&buf[..len.max(0) as usize]);
        // Our own surfaces, and explorer's desktop windows when unswapped.
        if class.starts_with("WinRestyle") || class == "Progman" || class == "WorkerW" {
            return None;
        }

        let len = GetWindowTextW(hwnd, &mut buf);
        if len <= 0 {
            return None;
        }
        Some(TaskWindow {
            hwnd: hwnd.0 as isize,
            title: String::from_utf16_lossy(&buf[..len as usize]),
        })
    }
}

/// DWM cloaking hides a window while keeping it "visible" (suspended UWP
/// apps, windows on other virtual desktops). Treat query failure as visible.
fn cloaked(hwnd: HWND) -> bool {
    let mut cloak: u32 = 0;
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloak as *mut u32 as *mut core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        )
        .map(|()| cloak != 0)
        .unwrap_or(false)
    }
}

/// The current foreground window as a raw handle (0 if none).
pub fn foreground() -> isize {
    unsafe { GetForegroundWindow().0 as isize }
}

/// Install the WinEvent hooks that keep the button list fresh. Must run on
/// the bar's thread: out-of-context hook callbacks ride its message pump.
/// Failure of any range degrades to a stale list, never an error.
pub fn install_hooks(bar: HWND) {
    BAR_HWND.store(bar.0 as isize, Ordering::SeqCst);
    let ranges = [
        // New/gone/shown/hidden windows (CREATE..HIDE is one contiguous run).
        (EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE),
        (EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_NAMECHANGE),
        (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
        (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
        (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
    ];
    for (lo, hi) in ranges {
        let hook: HWINEVENTHOOK = unsafe {
            SetWinEventHook(
                lo,
                hi,
                None,
                Some(win_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        if hook.is_invalid() {
            log::warn!("WinEvent hook {lo:#06x}-{hi:#06x} failed; window list may go stale");
        }
    }
}

/// Re-arm the coalescing flag; the bar calls this at the top of its
/// `WM_WINDOWS_CHANGED` handler so events during the refresh re-post.
pub fn ack_refresh() {
    REFRESH_PENDING.store(false, Ordering::SeqCst);
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    idobject: i32,
    idchild: i32,
    _thread: u32,
    _time: u32,
) {
    // Whole windows only — every control and list item fires these too.
    if idobject != OBJID_WINDOW.0 || idchild != 0 {
        return;
    }
    let bar = BAR_HWND.load(Ordering::SeqCst);
    if bar == 0 {
        return;
    }
    if !REFRESH_PENDING.swap(true, Ordering::SeqCst) {
        let _ = PostMessageW(HWND(bar as _), WM_WINDOWS_CHANGED, WPARAM(0), LPARAM(0));
    }
}

/// Perform the taskbar click on a window (see [`tasks::click_action`]).
pub fn activate(hwnd_raw: isize) {
    let hwnd = HWND(hwnd_raw as _);
    unsafe {
        let action = tasks::click_action(GetForegroundWindow() == hwnd, IsIconic(hwnd).as_bool());
        match action {
            ClickAction::Minimize => {
                let _ = ShowWindow(hwnd, SW_MINIMIZE);
            }
            ClickAction::RestoreAndFocus => {
                let _ = ShowWindow(hwnd, SW_RESTORE);
                focus(hwnd);
            }
            ClickAction::Focus => focus(hwnd),
        }
    }
}

/// `SetForegroundWindow` is subject to the foreground lock; a click on our
/// (never-activated) bar usually grants it, but fall back to the alt-tab
/// path when it refuses.
unsafe fn focus(hwnd: HWND) {
    if !SetForegroundWindow(hwnd).as_bool() {
        SwitchToThisWindow(hwnd, true);
    }
}

//! The Win32 side of window buttons: which top-level windows deserve one,
//! event-driven change notification, and the click actions.
//!
//! Change tracking is out-of-context WinEvent hooks (delivered on this
//! process's message pump), coalesced into a single [`WM_WINDOWS_CHANGED`]
//! posted to the bar — the bar then re-enumerates and diffs. No polling:
//! an idle desktop costs nothing (the rendering doc's idle-cost goal).

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VK_LWIN,
};
use windows::Win32::UI::Shell::{SHGetFileInfoW, ShellExecuteW, SHFILEINFOW, SHGFI_ICON};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyIcon, DestroyMenu, EnumWindows, GetClassLongPtrW,
    GetClassNameW, GetForegroundWindow, GetIconInfo, GetWindow, GetWindowLongPtrW, GetWindowTextW,
    IsIconic, IsWindowVisible, PostMessageW, SendMessageTimeoutW, SetForegroundWindow, ShowWindow,
    SwitchToThisWindow, TrackPopupMenu, EVENT_OBJECT_CLOAKED, EVENT_OBJECT_CREATE,
    EVENT_OBJECT_HIDE, EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND,
    EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, GCLP_HICON, GCLP_HICONSM, GWL_EXSTYLE,
    GW_OWNER, HICON, ICONINFO, ICON_BIG, ICON_SMALL, ICON_SMALL2, MF_STRING, OBJID_WINDOW,
    SMTO_ABORTIFHUNG, SW_MINIMIZE, SW_RESTORE, SW_SHOWNORMAL, TPM_BOTTOMALIGN, TPM_NONOTIFY,
    TPM_RETURNCMD, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_APP, WM_GETICON, WM_NULL,
    WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};

use crate::tasks::{self, ClickAction, Icon, TaskWindow};

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

/// Point the hooks' change notifications at a different bar window (after a
/// display-change rebuild replaces the windows the hooks were aimed at).
pub fn retarget(bar: HWND) {
    BAR_HWND.store(bar.0 as isize, Ordering::SeqCst);
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

/// Fetch a window's icon as renderable pixels. Asks the window first
/// (`WM_GETICON`, with a short abort-if-hung timeout — a wedged app must
/// never wedge the bar), then falls back to the window-class icon. `None`
/// (no icon anywhere, or an undecodable one) renders as a text-only chip.
pub fn window_icon(hwnd_raw: isize) -> Option<Icon> {
    let hwnd = HWND(hwnd_raw as _);
    unsafe {
        let mut handle: usize = 0;
        for kind in [ICON_SMALL2, ICON_SMALL, ICON_BIG] {
            let mut result: usize = 0;
            SendMessageTimeoutW(
                hwnd,
                WM_GETICON,
                WPARAM(kind as usize),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                100,
                Some(&mut result),
            );
            if result != 0 {
                handle = result;
                break;
            }
        }
        if handle == 0 {
            handle = GetClassLongPtrW(hwnd, GCLP_HICONSM);
        }
        if handle == 0 {
            handle = GetClassLongPtrW(hwnd, GCLP_HICON);
        }
        if handle == 0 {
            return None;
        }
        // Shared handle — ours to read, not to destroy.
        icon_pixels(HICON(handle as _))
    }
}

/// Decode an `HICON` into premultiplied BGRA via GDI.
unsafe fn icon_pixels(hicon: HICON) -> Option<Icon> {
    let mut info = ICONINFO::default();
    GetIconInfo(hicon, &mut info).ok()?;
    // GetIconInfo hands us *copies* of both planes; free them on every path.
    let result = decode_icon_info(&info);
    let _ = DeleteObject(info.hbmColor);
    let _ = DeleteObject(info.hbmMask);
    result
}

unsafe fn decode_icon_info(info: &ICONINFO) -> Option<Icon> {
    if info.hbmColor.is_invalid() {
        // Monochrome (mask-only) icon; not worth rendering.
        return None;
    }
    let mut bm = BITMAP::default();
    if GetObjectW(
        info.hbmColor,
        std::mem::size_of::<BITMAP>() as i32,
        Some(&mut bm as *mut BITMAP as *mut core::ffi::c_void),
    ) == 0
    {
        return None;
    }
    let (w, h) = (bm.bmWidth, bm.bmHeight);
    if !(1..=256).contains(&w) || !(1..=256).contains(&h) {
        return None;
    }
    let color = dib_bits(info.hbmColor, w, h)?;
    // The AND-mask is only needed for alpha-less legacy icons; converting the
    // 1bpp mask through GetDIBits to 32bpp gives white/black pixels.
    let mask = dib_bits(info.hbmMask, w, h);
    tasks::build_icon(w as u32, h as u32, color, mask.as_deref())
}

/// Read a bitmap's pixels as 32bpp top-down BGRA.
unsafe fn dib_bits(bitmap: HBITMAP, w: i32, h: i32) -> Option<Vec<u8>> {
    let hdc = CreateCompatibleDC(None);
    if hdc.is_invalid() {
        return None;
    }
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // negative = top-down rows
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pixels = vec![0u8; (w as usize) * (h as usize) * 4];
    let lines = GetDIBits(
        hdc,
        bitmap,
        0,
        h as u32,
        Some(pixels.as_mut_ptr().cast()),
        &mut bmi,
        DIB_RGB_COLORS,
    );
    let _ = DeleteDC(hdc);
    (lines == h).then_some(pixels)
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

/// Decode a *foreign* `HICON` (a tray registration's, owned by the sending
/// process — shared, never destroyed here) into renderable pixels.
pub fn foreign_icon_pixels(hicon_raw: isize) -> Option<Icon> {
    unsafe { icon_pixels(HICON(hicon_raw as _)) }
}

/// NUL-terminated UTF-16 for a path, losslessly (`encode_wide` preserves
/// unpaired surrogates that a `to_string_lossy` round-trip would mangle).
fn wide_path(path: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Extract a pinned launcher's icon (exe, shortcut, or document) as
/// renderable pixels. `None` renders as a letter chip. Unlike window icons,
/// the returned `HICON` is ours and must be destroyed.
pub fn pinned_icon(path: &std::path::Path) -> Option<Icon> {
    let wide = wide_path(path);
    let mut info = SHFILEINFOW::default();
    unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON,
        );
        if info.hIcon.is_invalid() {
            return None;
        }
        let pixels = icon_pixels(info.hIcon);
        let _ = DestroyIcon(info.hIcon);
        pixels
    }
}

/// Launch a pinned entry the way a double-click in explorer would.
/// Fire-and-forget: failures land in the log, never in the bar.
pub fn launch_pinned(path: &std::path::Path) {
    let wide = wide_path(path);
    let result = unsafe {
        ShellExecuteW(
            None,
            windows::core::w!("open"),
            PCWSTR(wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW's contract: values <= 32 are error codes.
    if result.0 as usize <= 32 {
        log::warn!(
            "pinned launch failed ({}): code {}",
            path.display(),
            result.0 as usize
        );
    } else {
        log::info!("pinned launch: {}", path.display());
    }
}

/// Show the overflow menu listing the window buttons that didn't fit, at
/// screen coordinates `(x, y)` (bottom-aligned). Returns the chosen window.
/// Blocks pumping messages until dismissed — the caller must hold no STATE
/// borrow.
pub fn show_overflow_menu(bar: HWND, x: i32, y: i32, items: &[(isize, String)]) -> Option<isize> {
    if items.is_empty() {
        return None;
    }
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        for (i, (_, title)) in items.iter().enumerate() {
            let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
            // Command ids start at 1; 0 is TrackPopupMenu's "dismissed".
            let _ = AppendMenuW(menu, MF_STRING, i + 1, PCWSTR(wide.as_ptr()));
        }
        // The standard dance for menus on a non-activated window: take
        // foreground so the menu dismisses on an outside click, and post a
        // no-op message afterwards so the menu loop exits cleanly.
        let _ = SetForegroundWindow(bar);
        let picked = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_BOTTOMALIGN,
            x,
            y,
            0,
            bar,
            None,
        );
        let _ = PostMessageW(bar, WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        let id = picked.0 as usize;
        (id >= 1 && id <= items.len()).then(|| items[id - 1].0)
    }
}

/// Stub Start action: tap the Win key. Unswapped this opens the system Start
/// menu (explorer is running); in a swapped session no Start experience
/// exists, so the tap lands on nothing — the real menu is `wr-startmenu`, a
/// later phase.
pub fn open_start_menu() {
    let key = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VK_LWIN,
                dwFlags: flags,
                ..Default::default()
            },
        },
    };
    let inputs = [key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)];
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        // Blocked by UIPI or an open secure desktop; nothing to recover.
        log::warn!("start: SendInput injected {sent}/{} events", inputs.len());
    }
}

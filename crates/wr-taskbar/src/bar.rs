//! The taskbar windows: creation (one per monitor), positioning, the message
//! pump, and the tray host.
//!
//! Everything runs on the main thread (the test-flag threads in `main` are
//! the only others). Repaints happen on demand — clock ticks, config changes,
//! display changes, window/tray events — not per frame, keeping idle cost
//! near zero.
//!
//! ## The one concurrency rule in this file
//!
//! Never call anything that can pump incoming sent messages — cross-process
//! `SendMessage*`, `SetWindowPos`, `TrackPopupMenu`, `ShellExecuteW`,
//! `DestroyWindow` — while a `STATE` borrow is held. A pumped message
//! re-enters `wndproc` (or `tray_wndproc`) on this same thread and the
//! re-entrant borrow panics (aborts, in release). The pattern everywhere:
//! snapshot under a short borrow, drop it, do the Win32 call, re-borrow to
//! store results.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    D2DERR_RECREATE_TARGET, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMSBT_MAINWINDOW, DWMSBT_NONE,
    DWMSBT_TRANSIENTWINDOW, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_DEFAULT, DWMWCP_ROUND, DWM_SYSTEMBACKDROP_TYPE, DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::{GetLocalTime, GetTickCount64};
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, GetDpiForSystem, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_DOWN, VK_ESCAPE, VK_RETURN, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    GetSystemMetrics, GetWindowRect, IsWindow, LoadCursorW, PostQuitMessage, RegisterClassW,
    RegisterWindowMessageW, SendNotifyMessageW, SetForegroundWindow, SetTimer, SetWindowPos,
    ShowWindow, TranslateMessage, HWND_BROADCAST, HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST,
    IDC_ARROW, MSG, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, WA_INACTIVE,
    WM_ACTIVATE, WM_CHAR, WM_COPYDATA, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_TIMER, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_POPUP,
    WS_VISIBLE,
};

use wr_core::config::{Backdrop, Config, ConfigStore};

use crate::apps;
use crate::layout::{self, BarRect, Hit};
use crate::render::{Frame, MenuFrame, MenuRow, Renderer, TrayItem};
use crate::startmenu;
use crate::tasks::{self, TaskWindow};
use crate::tray;
use crate::winlist;

const CLOCK_TIMER: usize = 1;

/// `windows-rs` files this under `UI_Controls`; not worth a whole feature
/// for one well-known message id.
const WM_MOUSELEAVE: u32 = 0x02A3;

/// Window class of the start-menu popup (ADR 0007). The `WinRestyle` prefix
/// keeps it out of `winlist`'s button rules; it is not `Shell_TrayWnd`, so
/// recovery logic never sees it.
const MENU_WINDOW_CLASS: &str = "WinRestyleStartMenu";

/// A Start click arriving this soon after the menu was dismissed is the
/// click that dismissed it — treat it as "toggle closed", not "reopen".
const MENU_REOPEN_DEBOUNCE_MS: u64 = 300;

/// One bar window on one monitor.
struct Bar {
    /// Raw window handle (the key `wndproc` finds the bar by).
    hwnd: isize,
    renderer: Renderer,
    dpi: u32,
    /// The monitor this bar lives on: x, y, w, h in virtual-screen pixels.
    mon: (i32, i32, i32, i32),
    layout: layout::BarLayout,
    /// The element under the mouse on this bar.
    hovered: Option<Hit>,
    /// Whether a `WM_MOUSELEAVE` request is currently armed for this bar.
    mouse_tracking: bool,
    /// The DWM backdrop this window currently carries. Fresh windows carry
    /// none, so the default config never touches DWM at all, and windows
    /// recreated by a display-change rebuild get theirs re-applied.
    applied_backdrop: Backdrop,
}

/// The start-menu popup (ADR 0007): one lazily created window, reused across
/// opens, torn down on bar rebuilds. Unlike the bars it takes activation —
/// keyboard drives it, and losing activation dismisses it.
struct StartMenu {
    /// Raw window handle.
    hwnd: isize,
    renderer: Renderer,
    /// The bar's DPI at the last open.
    dpi: u32,
    /// Window size in physical pixels (the layout's coordinate space).
    size: (i32, i32),
    /// Geometry for the current filtered list + scroll.
    layout: startmenu::MenuLayout,
    /// Filter text, selection, scroll.
    state: startmenu::MenuState,
    /// The scanned app list (re-scanned on every open).
    apps: Vec<apps::AppEntry>,
    /// Indices into `apps` matching the filter.
    filtered: Vec<usize>,
    /// Filtered-list index under the mouse.
    hovered: Option<usize>,
    /// Whether a `WM_MOUSELEAVE` request is currently armed.
    mouse_tracking: bool,
    visible: bool,
    /// Tick (ms) of the last dismissal, for the reopen debounce.
    dismissed_at: u64,
}

struct State {
    store: Arc<ConfigStore>,
    config: Config,
    /// Topmost only in a real swapped session; in an unswapped dev/test run
    /// explorer's taskbar is live and we must not sit on top of it.
    topmost: bool,
    clock: String,
    /// Second clock line; empty when `show_date` is off.
    date: String,
    /// Taskbar-worthy windows in stable button order (shared by all bars).
    tasks: Vec<TaskWindow>,
    /// Foreground window handle (0 = none), for the highlighted chip.
    active: isize,
    /// Decoded icons per window; `Some(None)` remembers "asked, has none".
    icons: HashMap<isize, Option<tasks::Icon>>,
    /// Pinned launchers in config order, with their decoded icons.
    pinned: Vec<(PathBuf, Option<tasks::Icon>)>,
    /// Tray icon registry (only populated when hosting; see `tray_hwnd`).
    tray: Vec<tray::TrayIcon>,
    /// Decoded pixels for tray icons, keyed by (owner, uid).
    tray_pixels: HashMap<(isize, u32), Option<tasks::Icon>>,
    /// The `Shell_TrayWnd` host window; 0 when not hosting (unswapped).
    tray_hwnd: isize,
    /// The start-menu popup; `None` until first opened.
    menu: Option<StartMenu>,
    bars: Vec<Bar>,
    /// Id of the registered `CONFIG_CHANGED_MESSAGE` the shell posts to us.
    config_changed_msg: u32,
    /// Log the next successful draw (startup and config changes) so the VM
    /// harness can assert paints happen.
    log_next_paint: bool,
}

thread_local! {
    // One taskbar process per session; all windows live on this thread.
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    // Rebuild coalescing: display changes arrive in bursts, and the rebuild
    // itself pumps sent messages (DestroyWindow, CreateWindowExW), so a
    // nested WM_DISPLAYCHANGE queues one follow-up run instead of
    // re-entering and orphaning half-built bar sets.
    static REBUILDING: Cell<bool> = const { Cell::new(false) };
    static REBUILD_QUEUED: Cell<bool> = const { Cell::new(false) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn run(store: Arc<ConfigStore>) -> anyhow::Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // SHGetFileInfoW (pinned icons) wants COM on this thread; failure
        // degrades to letter chips, not an error.
        if let Err(e) = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() {
            log::warn!("CoInitializeEx failed: {e} (pinned icons may not load)");
        }
    }
    let config = store.get();
    let topmost = !wr_core::shell::desktop_shell_running();

    let instance = unsafe { GetModuleHandleW(None)? };
    let class_name = wide(wr_core::TASKBAR_WINDOW_CLASS);
    let class = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
        ..Default::default()
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(windows::core::Error::from_win32().into());
    }
    let menu_class_name = wide(MENU_WINDOW_CLASS);
    let menu_class = WNDCLASSW {
        lpfnWndProc: Some(menu_wndproc),
        hInstance: instance.into(),
        lpszClassName: PCWSTR(menu_class_name.as_ptr()),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
        ..Default::default()
    };
    if unsafe { RegisterClassW(&menu_class) } == 0 {
        return Err(windows::core::Error::from_win32().into());
    }

    let pinned = fetch_pinned(&config.taskbar.pinned);
    let bars = create_bars(&config, topmost, pinned.len())?;
    anyhow::ensure!(!bars.is_empty(), "no monitors to put a taskbar on");
    let primary = HWND(bars[0].hwnd as _);
    let config_changed_msg =
        unsafe { RegisterWindowMessageW(PCWSTR(wide(wr_core::CONFIG_CHANGED_MESSAGE).as_ptr())) };

    // Tray hosting only in a swapped session: unswapped, explorer's real
    // Shell_TrayWnd is live and creating a second one would fight it for
    // every app's icon registration (ADR 0005).
    let tray_hwnd = if topmost { create_tray_host() } else { 0 };

    log::info!(
        "taskbar up: {} bar(s), {}, tray host {}",
        bars.len(),
        if topmost {
            "topmost"
        } else {
            "not topmost (another desktop shell is on screen)"
        },
        if tray_hwnd != 0 { "active" } else { "off" },
    );
    for bar in &bars {
        log::info!(
            "taskbar window up (monitor at {},{} {}x{}; dpi {})",
            bar.mon.0,
            bar.mon.1,
            bar.mon.2,
            bar.mon.3,
            bar.dpi
        );
    }

    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            store,
            config,
            topmost,
            clock: String::new(),
            date: String::new(),
            tasks: Vec::new(),
            active: 0,
            icons: HashMap::new(),
            pinned,
            tray: Vec::new(),
            tray_pixels: HashMap::new(),
            tray_hwnd,
            menu: None,
            bars,
            config_changed_msg,
            log_next_paint: true,
        })
    });

    apply_backdrop_all();
    winlist::install_hooks(primary);
    refresh_windows();
    redraw_bars(None);
    unsafe { SetTimer(primary, CLOCK_TIMER, 1000, None) };

    // The VM harness drives chip clicks by posting WM_LBUTTONDOWN; publish
    // the geometry so the tests never re-derive layout constants.
    STATE.with(|s| {
        if let Some(st) = s.borrow().as_ref() {
            if let Some(bar) = st.bars.first() {
                let r = bar.layout.start;
                log::info!("start chip at {},{} {}x{} (bar-local)", r.x, r.y, r.w, r.h);
            }
            if let Some(r) = st.bars.first().and_then(|b| b.layout.pinned.first()) {
                log::info!(
                    "pinned[0] chip at {},{} {}x{} (bar-local)",
                    r.x,
                    r.y,
                    r.w,
                    r.h
                );
            }
        }
    });

    if tray_hwnd != 0 {
        // Tell every running app the taskbar (re)started so they re-register
        // their tray icons with us.
        let msg = unsafe { RegisterWindowMessageW(windows::core::w!("TaskbarCreated")) };
        if msg != 0 {
            let _ = unsafe { SendNotifyMessageW(HWND_BROADCAST, msg, WPARAM(0), LPARAM(0)) };
            log::info!("tray host: broadcast TaskbarCreated");
        }
    }

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        unsafe {
            // Translate: the start menu's type-to-filter needs WM_CHAR.
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// One monitor's placement: virtual-screen rect + effective DPI.
struct Monitor {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    dpi: u32,
}

fn monitors() -> Vec<Monitor> {
    extern "system" fn enum_proc(
        hmon: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> windows::Win32::Foundation::BOOL {
        let out = unsafe { &mut *(lparam.0 as *mut Vec<Monitor>) };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetMonitorInfoW(hmon, &mut info) }.as_bool() {
            let (mut dx, mut dy) = (0u32, 0u32);
            let dpi = match unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) } {
                Ok(()) if dx > 0 => dx,
                _ => unsafe { GetDpiForSystem() },
            };
            let _ = dy;
            let r = info.rcMonitor;
            out.push(Monitor {
                x: r.left,
                y: r.top,
                w: r.right - r.left,
                h: r.bottom - r.top,
                dpi: dpi.max(1),
            });
        }
        true.into()
    }
    let mut out: Vec<Monitor> = Vec::new();
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(enum_proc),
            LPARAM(&mut out as *mut Vec<Monitor> as isize),
        )
    };
    if !ok.as_bool() || out.is_empty() {
        // Headless or API failure: fall back to the primary-screen metrics.
        out = vec![Monitor {
            x: 0,
            y: 0,
            w: unsafe { GetSystemMetrics(SM_CXSCREEN) },
            h: unsafe { GetSystemMetrics(SM_CYSCREEN) },
            dpi: unsafe { GetDpiForSystem() }.max(1),
        }];
    }
    out
}

/// Create one bar window (positioned, renderer attached) per monitor.
/// Runs with no `STATE` borrow held — window creation dispatches messages.
fn create_bars(config: &Config, topmost: bool, pinned_count: usize) -> anyhow::Result<Vec<Bar>> {
    let instance = unsafe { GetModuleHandleW(None)? };
    let class_name = wide(wr_core::TASKBAR_WINDOW_CLASS);
    let mut bars = Vec::new();
    for mon in monitors() {
        let rect = layout::bar_rect(
            mon.x,
            mon.y,
            mon.w,
            mon.h,
            config.taskbar.height,
            config.taskbar.margin,
            mon.dpi,
        );
        let hwnd = unsafe {
            CreateWindowExW(
                // NOREDIRECTIONBITMAP: all pixels come from the composition
                // swapchain; no GDI surface is allocated for this window.
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_NOREDIRECTIONBITMAP,
                PCWSTR(class_name.as_ptr()),
                windows::core::w!("WinRestyle Taskbar"),
                WS_POPUP | WS_VISIBLE,
                rect.x,
                rect.y,
                rect.w,
                rect.h,
                None,
                None,
                instance,
                None,
            )
        };
        let hwnd = match hwnd {
            Ok(h) => h,
            Err(e) => {
                log::error!("bar window creation failed on one monitor: {e}");
                continue;
            }
        };
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                if topmost {
                    HWND_TOPMOST
                } else {
                    HWND_NOTOPMOST
                },
                rect.x,
                rect.y,
                rect.w,
                rect.h,
                SWP_NOACTIVATE,
            );
        }
        match Renderer::new(hwnd, rect.w, rect.h) {
            Ok(renderer) => bars.push(Bar {
                hwnd: hwnd.0 as isize,
                renderer,
                dpi: mon.dpi,
                mon: (mon.x, mon.y, mon.w, mon.h),
                layout: layout::bar_layout(rect.w, rect.h, mon.dpi, pinned_count, 0, 0),
                hovered: None,
                mouse_tracking: false,
                applied_backdrop: Backdrop::None,
            }),
            Err(e) => {
                log::error!("renderer failed on one monitor: {e:#}");
                let _ = unsafe { DestroyWindow(hwnd) };
            }
        }
    }
    Ok(bars)
}

/// Extract icons for the pinned launchers. Sends nothing to other windows,
/// but is still called with no `STATE` borrow held (shell extensions can do
/// anything).
fn fetch_pinned(paths: &[PathBuf]) -> Vec<(PathBuf, Option<tasks::Icon>)> {
    let pinned: Vec<_> = paths
        .iter()
        .map(|p| (p.clone(), winlist::pinned_icon(p)))
        .collect();
    if !pinned.is_empty() {
        log::info!(
            "pinned apps: {} ({} with icons)",
            pinned.len(),
            pinned.iter().filter(|(_, i)| i.is_some()).count()
        );
    }
    pinned
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // The registered config-changed message id is not a compile-time constant,
    // so it can't be a `match` arm.
    let config_changed = STATE.with(|s| {
        s.borrow()
            .as_ref()
            .is_some_and(|st| msg == st.config_changed_msg && msg != 0)
    });
    if config_changed {
        on_config_changed();
        return LRESULT(0);
    }
    match msg {
        winlist::WM_WINDOWS_CHANGED => {
            // Re-arm first: events landing during the refresh must re-post.
            winlist::ack_refresh();
            refresh_windows();
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            on_click(hwnd, mouse_x(lparam), mouse_y(lparam));
            // Tray owners get the raw button stream too; forwarding lives in
            // one place so left-down can't diverge from up/right-click.
            forward_tray_mouse(hwnd, mouse_x(lparam), mouse_y(lparam), msg);
            LRESULT(0)
        }
        WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP => {
            // Only tray icons care about these; their owners expect the full
            // down/up stream to drive their menus.
            forward_tray_mouse(hwnd, mouse_x(lparam), mouse_y(lparam), msg);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            on_mouse_move(hwnd, mouse_x(lparam), mouse_y(lparam));
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            let had_hover = STATE.with(|s| {
                let mut s = s.borrow_mut();
                let Some(bar) = s.as_mut().and_then(|st| bar_mut(&mut st.bars, hwnd)) else {
                    return false;
                };
                bar.mouse_tracking = false;
                bar.hovered.take().is_some()
            });
            if had_hover {
                redraw_bars(Some(hwnd.0 as isize));
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == CLOCK_TIMER => {
            on_timer();
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            rebuild_bars("display change");
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let dpi = ((wparam.0 & 0xffff) as u32).max(1);
            let changed = STATE.with(|s| {
                let mut s = s.borrow_mut();
                match s.as_mut().and_then(|st| bar_mut(&mut st.bars, hwnd)) {
                    Some(bar) if bar.dpi != dpi => {
                        bar.dpi = dpi;
                        true
                    }
                    _ => false,
                }
            });
            if changed {
                // Only this bar's monitor rescaled; the others are untouched
                // (a swapchain resize is a full GPU buffer reallocation).
                apply_layout(Some(hwnd.0 as isize));
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // Deliberate destroys (rebuild_bars, create_bars' partial-failure
            // cleanup) only ever hit windows that were removed from — or
            // never added to — the bar set. Losing a *live* bar is the only
            // shutdown signal; anything else must not poison the pump (a
            // startup-time PostQuitMessage would turn a one-monitor renderer
            // failure into a clean-exit crash loop).
            let live_bar = STATE.with(|s| {
                s.borrow()
                    .as_ref()
                    .is_some_and(|st| st.bars.iter().any(|b| b.hwnd == hwnd.0 as isize))
            });
            if live_bar {
                unsafe { PostQuitMessage(0) };
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn mouse_x(lparam: LPARAM) -> i32 {
    (lparam.0 & 0xffff) as i16 as i32
}

fn mouse_y(lparam: LPARAM) -> i32 {
    ((lparam.0 >> 16) & 0xffff) as i16 as i32
}

fn bar_mut(bars: &mut [Bar], hwnd: HWND) -> Option<&mut Bar> {
    bars.iter_mut().find(|b| b.hwnd == hwnd.0 as isize)
}

fn bar_ref(bars: &[Bar], hwnd: HWND) -> Option<&Bar> {
    bars.iter().find(|b| b.hwnd == hwnd.0 as isize)
}

/// The tray icons a viewer can see, in cell order — the single projection
/// behind layout counts, rendering, and `Hit::Tray(i)` click routing.
fn visible_tray(tray: &[tray::TrayIcon]) -> impl Iterator<Item = &tray::TrayIcon> {
    tray.iter().filter(|t| !t.hidden)
}

/// A left click landed at bar-local (x, y). Decide under a short borrow,
/// act with the borrow dropped (every action below can pump messages).
fn on_click(hwnd: HWND, x: i32, y: i32) {
    enum Action {
        Start,
        Launch(PathBuf),
        Activate(isize),
        Overflow(Vec<(isize, String)>, POINT),
        None,
    }
    let action = STATE.with(|s| {
        let s = s.borrow();
        let Some(st) = s.as_ref() else {
            return Action::None;
        };
        let Some(bar) = bar_ref(&st.bars, hwnd) else {
            return Action::None;
        };
        match bar.layout.hit_test(x, y) {
            Some(Hit::Start) => Action::Start,
            Some(Hit::Pinned(i)) => match st.pinned.get(i) {
                Some((path, _)) => Action::Launch(path.clone()),
                None => Action::None,
            },
            Some(Hit::Task(i)) => match st.tasks.get(i) {
                Some(t) => Action::Activate(t.hwnd),
                None => Action::None,
            },
            Some(Hit::Overflow) => {
                let shown = bar.layout.tasks.len();
                let items = st.tasks[shown.min(st.tasks.len())..]
                    .iter()
                    .map(|t| (t.hwnd, t.title.clone()))
                    .collect();
                let anchor = bar
                    .layout
                    .overflow
                    .map_or(POINT { x, y }, |r| POINT { x: r.x, y: r.y });
                Action::Overflow(items, anchor)
            }
            // Tray clicks ride the wndproc's forwarding path, not this one.
            Some(Hit::Tray(_)) | None => Action::None,
        }
    });
    match action {
        Action::Start => toggle_menu(hwnd),
        Action::Launch(path) => winlist::launch_pinned(&path),
        Action::Activate(target) => winlist::activate(target),
        Action::Overflow(items, mut anchor) => {
            let _ = unsafe { ClientToScreen(hwnd, &mut anchor) };
            log::info!("overflow menu: {} windows", items.len());
            if let Some(target) = winlist::show_overflow_menu(hwnd, anchor.x, anchor.y, &items) {
                winlist::activate(target);
            }
        }
        Action::None => {}
    }
}

/// Forward a mouse event on a tray cell to the icon's owner, in whichever
/// callback encoding it negotiated. Version-4 owners additionally get the
/// contract's synthesized events — `NIN_SELECT` after left-up and
/// `WM_CONTEXTMENU` after right-up — which is what v4 apps actually key
/// their activation and menus off.
fn forward_tray_mouse(hwnd: HWND, x: i32, y: i32, msg: u32) {
    // Screen coords first (v4 owners anchor their menus with them); no
    // borrow held around the Win32 call.
    let mut screen = POINT { x, y };
    let _ = unsafe { ClientToScreen(hwnd, &mut screen) };
    let post = STATE.with(|s| {
        let s = s.borrow();
        let st = s.as_ref()?;
        let bar = bar_ref(&st.bars, hwnd)?;
        let Some(Hit::Tray(i)) = bar.layout.hit_test(x, y) else {
            return None;
        };
        let icon = visible_tray(&st.tray).nth(i)?;
        if icon.callback == 0 {
            return None;
        }
        let mut events = vec![msg];
        if icon.version >= tray::VERSION_4 {
            match msg {
                WM_LBUTTONUP => events.push(tray::NIN_SELECT),
                WM_RBUTTONUP => events.push(tray::WM_CONTEXTMENU),
                _ => {}
            }
        }
        let params: Vec<(usize, isize)> = events
            .into_iter()
            .map(|m| tray::callback_params(icon, m, screen.x, screen.y))
            .collect();
        Some((icon.owner, icon.callback, params))
    });
    if let Some((owner, callback, params)) = post {
        for (wparam, lparam) in params {
            let _ = unsafe {
                windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    HWND(owner as _),
                    callback,
                    WPARAM(wparam),
                    LPARAM(lparam),
                )
            };
        }
    }
}

fn on_mouse_move(hwnd: HWND, x: i32, y: i32) {
    let (hover_changed, arm) = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(bar) = s.as_mut().and_then(|st| bar_mut(&mut st.bars, hwnd)) else {
            return (false, false);
        };
        let hovered = bar.layout.hit_test(x, y);
        let changed = hovered != bar.hovered;
        bar.hovered = hovered;
        let arm = !bar.mouse_tracking;
        bar.mouse_tracking = true;
        (changed, arm)
    });
    if arm {
        // Ask for one WM_MOUSELEAVE so the hover highlight clears when the
        // mouse leaves this bar.
        let mut track = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        if unsafe { TrackMouseEvent(&mut track) }.is_err() {
            STATE.with(|s| {
                if let Some(bar) = s
                    .borrow_mut()
                    .as_mut()
                    .and_then(|st| bar_mut(&mut st.bars, hwnd))
                {
                    bar.mouse_tracking = false; // retry on the next move
                }
            });
        }
    }
    if hover_changed {
        redraw_bars(Some(hwnd.0 as isize));
    }
}

/// Clock tick: repaint when the strings change, and — when hosting the tray
/// — prune icons whose owner died without a `NIM_DELETE` (window gone).
fn on_timer() {
    let (stale, dead_owners) = STATE.with(|s| {
        let s = s.borrow();
        let Some(st) = s.as_ref() else {
            return (false, Vec::new());
        };
        let (clock, date) = clock_strings(st.config.taskbar.show_date);
        let stale = st.clock != clock || st.date != date;
        let dead: Vec<isize> = if st.tray_hwnd == 0 {
            Vec::new()
        } else {
            st.tray
                .iter()
                .map(|t| t.owner)
                .filter(|&o| !unsafe { IsWindow(HWND(o as _)) }.as_bool())
                .collect()
        };
        (stale, dead)
    });
    if !dead_owners.is_empty() {
        STATE.with(|s| {
            if let Some(st) = s.borrow_mut().as_mut() {
                st.tray.retain(|t| !dead_owners.contains(&t.owner));
                st.tray_pixels.retain(|(o, _), _| !dead_owners.contains(o));
                log::info!("tray: pruned {} dead owner(s)", dead_owners.len());
            }
        });
        relayout_all();
    } else if stale {
        redraw_bars(None);
    }
}

/// Re-snapshot the config from disk (the shell posts the config-changed
/// message *after* the file was rewritten) and re-apply everything.
fn on_config_changed() {
    let diff = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let st = s.as_mut()?;
        let new = st.store.reload();
        if new == st.config {
            return None;
        }
        let pinned_changed = new.taskbar.pinned != st.config.taskbar.pinned;
        let pinned_paths = new.taskbar.pinned.clone();
        st.config = new;
        st.log_next_paint = true;
        Some((pinned_changed, pinned_paths))
    });
    let Some((pinned_changed, pinned_paths)) = diff else {
        return;
    };
    if pinned_changed {
        // Icon extraction runs unborrowed; store the result after.
        let fetched = fetch_pinned(&pinned_paths);
        STATE.with(|s| {
            if let Some(st) = s.borrow_mut().as_mut() {
                st.pinned = fetched;
            }
        });
    }
    apply_backdrop_all();
    apply_layout(None);
    // An open menu re-themes too (no-op while hidden).
    redraw_menu();
}

/// Push the configured DWM backdrop (or its absence) onto every bar window
/// that doesn't already carry it. Fresh windows carry none, so the default
/// `backdrop = "none"` path never touches DWM at all — and windows created
/// by a display-change rebuild get a non-default material re-applied.
fn apply_backdrop_all() {
    let Some((targets, backdrop)) = STATE.with(|s| {
        s.borrow().as_ref().map(|st| {
            let desired = st.config.taskbar.backdrop;
            (
                st.bars
                    .iter()
                    .filter(|b| b.applied_backdrop != desired)
                    .map(|b| b.hwnd)
                    .collect::<Vec<_>>(),
                desired,
            )
        })
    }) else {
        return;
    };
    if targets.is_empty() {
        return;
    }
    let enable = backdrop != Backdrop::None;
    let sbt: DWM_SYSTEMBACKDROP_TYPE = match backdrop {
        Backdrop::None => DWMSBT_NONE,
        Backdrop::Acrylic => DWMSBT_TRANSIENTWINDOW,
        Backdrop::Mica => DWMSBT_MAINWINDOW,
    };
    let corner: DWM_WINDOW_CORNER_PREFERENCE = if enable { DWMWCP_ROUND } else { DWMWCP_DEFAULT };
    // Extend the frame into the whole client area: the system material only
    // renders where the DWM frame is.
    let m = if enable { -1 } else { 0 };
    let margins = MARGINS {
        cxLeftWidth: m,
        cxRightWidth: m,
        cyTopHeight: m,
        cyBottomHeight: m,
    };
    let mut failed = false;
    for hwnd in &targets {
        let hwnd = HWND(*hwnd as _);
        unsafe {
            if let Err(e) = DwmExtendFrameIntoClientArea(hwnd, &margins) {
                // Composition off (remote sessions, odd VMs): same fallback
                // posture as a missing backdrop API — the bar's own
                // translucent fill still renders.
                log::warn!("backdrop: system backdrop unavailable (frame extension failed: {e})");
                failed = true;
                continue;
            }
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &corner as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
            );
            if let Err(e) = DwmSetWindowAttribute(
                hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE,
                &sbt as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<DWM_SYSTEMBACKDROP_TYPE>() as u32,
            ) {
                // Pre-22H2 builds don't know the attribute.
                log::warn!("backdrop: system backdrop unavailable: {e}");
                failed = true;
            }
        }
    }
    // Record the attempt either way — a failing DWM won't start succeeding
    // because we hammer it on every config nudge.
    STATE.with(|s| {
        if let Some(st) = s.borrow_mut().as_mut() {
            for bar in st.bars.iter_mut().filter(|b| targets.contains(&b.hwnd)) {
                bar.applied_backdrop = backdrop;
            }
        }
    });
    if !failed {
        log::info!("backdrop applied: {backdrop:?}");
    }
}

/// Recompute bar rectangles (all bars, or just `only`) and apply them:
/// move/size the windows, resize the swapchains, relayout, repaint.
/// `SetWindowPos` runs outside the state borrow — it dispatches messages
/// synchronously into `wndproc`.
fn apply_layout(only: Option<isize>) {
    let Some(targets) = STATE.with(|s| {
        s.borrow().as_ref().map(|st| {
            st.bars
                .iter()
                .filter(|b| only.is_none_or(|h| h == b.hwnd))
                .map(|b| {
                    (
                        b.hwnd,
                        layout::bar_rect(
                            b.mon.0,
                            b.mon.1,
                            b.mon.2,
                            b.mon.3,
                            st.config.taskbar.height,
                            st.config.taskbar.margin,
                            b.dpi,
                        ),
                        st.topmost,
                    )
                })
                .collect::<Vec<_>>()
        })
    }) else {
        return;
    };
    for (hwnd, rect, topmost) in &targets {
        unsafe {
            let _ = SetWindowPos(
                HWND(*hwnd as _),
                if *topmost {
                    HWND_TOPMOST
                } else {
                    HWND_NOTOPMOST
                },
                rect.x,
                rect.y,
                rect.w,
                rect.h,
                SWP_NOACTIVATE,
            );
        }
    }
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(st) = s.as_mut() else { return };
        for (hwnd, rect, _) in &targets {
            if let Some(bar) = bar_mut(&mut st.bars, HWND(*hwnd as _)) {
                if let Err(e) = bar.renderer.resize(rect.w, rect.h) {
                    log::error!("swapchain resize failed: {e}");
                }
            }
        }
    });
    relayout_all();
}

/// Recompute on-bar geometry (element counts changed) and repaint. No window
/// moves; safe whenever.
fn relayout_all() {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(st) = s.as_mut() else { return };
        let State {
            bars,
            pinned,
            tasks,
            tray,
            ..
        } = st;
        let tray_visible = visible_tray(tray).count();
        for bar in bars.iter_mut() {
            let mut client = RECT::default();
            let _ = unsafe { GetClientRect(HWND(bar.hwnd as _), &mut client) };
            bar.layout = layout::bar_layout(
                client.right - client.left,
                client.bottom - client.top,
                bar.dpi,
                pinned.len(),
                tasks.len(),
                tray_visible,
            );
            // The element under the cursor may have shifted; the highlight
            // is re-derived on the next mouse move.
            bar.hovered = None;
        }
    });
    redraw_bars(None);
}

/// Re-enumerate the window population, merge it into the button list, and
/// repaint if anything the user can see changed. Triggered by the WinEvent
/// hooks (coalesced), and once at startup.
fn refresh_windows() {
    let fresh = winlist::enumerate();
    let active = winlist::foreground();
    let (count_changed, changed, added, removed) = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(st) = s.as_mut() else {
            return (false, false, Vec::new(), Vec::new());
        };
        let (merged, added, removed) = tasks::merge(&st.tasks, &fresh);
        for w in &added {
            log::info!("taskbar: window added: {:?}", w.title);
        }
        for w in &removed {
            log::info!("taskbar: window removed: {:?}", w.title);
        }
        let count_changed = merged.len() != st.tasks.len();
        let list_changed = merged != st.tasks;
        if list_changed {
            if count_changed {
                log::info!("taskbar windows: {}", merged.len());
            }
            st.tasks = merged;
        }
        let active_changed = st.active != active;
        st.active = active;
        (
            count_changed,
            list_changed || active_changed,
            added,
            removed,
        )
    });

    // Icon fetching sends messages to other windows (WM_GETICON), and a
    // blocked send pumps *incoming* sent messages through our wndproc — so
    // it must happen with no STATE borrow held, or a re-entrant handler
    // panics the RefCell.
    if !added.is_empty() || !removed.is_empty() {
        let fetched: Vec<(isize, Option<tasks::Icon>)> = added
            .iter()
            .map(|w| (w.hwnd, winlist::window_icon(w.hwnd)))
            .collect();
        STATE.with(|s| {
            if let Some(st) = s.borrow_mut().as_mut() {
                for w in &removed {
                    st.icons.remove(&w.hwnd);
                }
                for (hwnd, icon) in fetched {
                    st.icons.insert(hwnd, icon);
                }
            }
        });
    }
    // Geometry depends only on the button COUNT; title and focus changes
    // just repaint (and keep the hover highlight under a resting cursor).
    if count_changed {
        relayout_all();
    } else if changed {
        redraw_bars(None);
    }
}

/// Destroy every bar window and rebuild the set from the current monitor
/// topology (WM_DISPLAYCHANGE: monitors added/removed/resized). Re-entrant
/// calls (display changes arrive in bursts, and the rebuild pumps sent
/// messages) coalesce into one queued follow-up run.
fn rebuild_bars(reason: &str) {
    if REBUILDING.get() {
        REBUILD_QUEUED.set(true);
        return;
    }
    REBUILDING.set(true);
    loop {
        rebuild_bars_once(reason);
        if !REBUILD_QUEUED.replace(false) {
            break;
        }
    }
    REBUILDING.set(false);
}

fn rebuild_bars_once(reason: &str) {
    log::info!("rebuilding bars ({reason})");
    // The menu (if any) anchors to a bar and DPI that are about to change;
    // drop it — the next Start click recreates it against the new topology.
    let menu = STATE.with(|s| {
        s.borrow_mut()
            .as_mut()
            .and_then(|st| st.menu.take())
            .map(|m| m.hwnd)
    });
    if let Some(hwnd) = menu {
        let _ = unsafe { DestroyWindow(HWND(hwnd as _)) };
    }
    // Take the bars out (dropping their renderers), then destroy the windows
    // with no borrow held — DestroyWindow dispatches WM_DESTROY, which is
    // benign for windows no longer in the bar set (see the WM_DESTROY arm).
    let old: Vec<isize> = STATE.with(|s| {
        s.borrow_mut()
            .as_mut()
            .map(|st| std::mem::take(&mut st.bars))
            .unwrap_or_default()
            .iter()
            .map(|b| b.hwnd)
            .collect()
    });
    for hwnd in old {
        let _ = unsafe { DestroyWindow(HWND(hwnd as _)) };
    }
    let Some((config, topmost, pinned_count)) = STATE.with(|s| {
        s.borrow()
            .as_ref()
            .map(|st| (st.config.clone(), st.topmost, st.pinned.len()))
    }) else {
        return;
    };
    let bars = match create_bars(&config, topmost, pinned_count) {
        Ok(bars) if !bars.is_empty() => bars,
        other => {
            log::error!(
                "bar rebuild produced no windows{}; exiting for a supervised relaunch",
                match other {
                    Err(e) => format!(": {e:#}"),
                    _ => String::new(),
                }
            );
            unsafe { PostQuitMessage(1) };
            return;
        }
    };
    let primary = HWND(bars[0].hwnd as _);
    STATE.with(|s| {
        if let Some(st) = s.borrow_mut().as_mut() {
            st.bars = bars;
            st.log_next_paint = true;
        }
    });
    winlist::retarget(primary);
    // A WM_WINDOWS_CHANGED posted to a destroyed bar died undelivered; if
    // the coalescing latch stayed armed, the hooks would never post again
    // and the buttons would freeze. Re-arm it unconditionally.
    winlist::ack_refresh();
    unsafe { SetTimer(primary, CLOCK_TIMER, 1000, None) };
    apply_backdrop_all();
    refresh_windows();
    relayout_all();
    // A config nudge posted to a dying bar died with it too; a fresh reload
    // closes that race (no-op when the file didn't change).
    on_config_changed();
}

/// Paint one bar (`only`) or all of them.
fn redraw_bars(only: Option<isize>) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(st) = s.as_mut() else { return };
        let (clock, date) = clock_strings(st.config.taskbar.show_date);
        st.clock = clock;
        st.date = date;
        let State {
            bars,
            config,
            clock,
            date,
            tasks,
            active,
            icons,
            pinned,
            tray,
            tray_pixels,
            log_next_paint,
            ..
        } = st;
        let tray_items: Vec<TrayItem> = visible_tray(tray)
            .map(|t| TrayItem {
                key: (t.owner, t.uid),
                rev: t.rev,
                icon: tray_pixels.get(&(t.owner, t.uid)).and_then(|i| i.as_ref()),
            })
            .collect();
        for bar in bars.iter_mut() {
            if only.is_some_and(|h| h != bar.hwnd) {
                continue;
            }
            // One frame serves both the draw and the device-loss retry: it
            // borrows bar.layout while the retry replaces bar.renderer —
            // disjoint fields, so the borrow checker permits sharing it.
            let frame = Frame {
                bar: &config.taskbar,
                clock,
                date,
                dpi: bar.dpi,
                tasks,
                layout: &bar.layout,
                active: *active,
                icons,
                pinned,
                tray: &tray_items,
                hovered: bar.hovered,
            };
            match bar.renderer.draw(&frame) {
                Ok(()) => {
                    if *log_next_paint {
                        *log_next_paint = false;
                        log::info!(
                            "taskbar painted: color {} alpha {}",
                            config.taskbar.color,
                            config.taskbar.alpha
                        );
                    }
                }
                // The device was lost (driver reset, session change): rebuild
                // this bar's rendering stack and try once more.
                Err(e) if e.code() == D2DERR_RECREATE_TARGET => {
                    log::warn!("render target lost; recreating renderer");
                    let hwnd = HWND(bar.hwnd as _);
                    let mut rect = RECT::default();
                    let _ = unsafe { GetClientRect(hwnd, &mut rect) };
                    match Renderer::new(hwnd, rect.right - rect.left, rect.bottom - rect.top) {
                        Ok(r) => {
                            bar.renderer = r;
                            if let Err(e) = bar.renderer.draw(&frame) {
                                log::error!("draw after renderer rebuild failed: {e}");
                            }
                        }
                        Err(e) => log::error!("renderer rebuild failed: {e:#}"),
                    }
                }
                Err(e) => log::error!("taskbar draw failed: {e}"),
            }
        }
    });
}

fn clock_strings(show_date: bool) -> (String, String) {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let t = unsafe { GetLocalTime() };
    let clock = format!("{:02}:{:02}", t.wHour, t.wMinute);
    let date = if show_date {
        format!(
            "{} {} {}",
            DAYS[(t.wDayOfWeek as usize) % 7],
            t.wDay,
            MONTHS[(t.wMonth as usize).clamp(1, 12) - 1],
        )
    } else {
        String::new()
    };
    (clock, date)
}

// ---------------------------------------------------------------------------
// Start menu (ADR 0007)
// ---------------------------------------------------------------------------

fn tick_ms() -> u64 {
    unsafe { GetTickCount64() }
}

/// A Start-chip click: close an open menu, open a closed one — unless this
/// is the click whose activation change just dismissed it.
fn toggle_menu(bar_hwnd: HWND) {
    enum Todo {
        Hide,
        Open,
        Nothing,
    }
    let todo = STATE.with(|s| {
        let s = s.borrow();
        let Some(st) = s.as_ref() else {
            return Todo::Nothing;
        };
        match &st.menu {
            Some(m) if m.visible => Todo::Hide,
            Some(m) if tick_ms().saturating_sub(m.dismissed_at) < MENU_REOPEN_DEBOUNCE_MS => {
                Todo::Nothing
            }
            _ => Todo::Open,
        }
    });
    match todo {
        Todo::Hide => hide_menu(),
        Todo::Open => open_menu(bar_hwnd),
        Todo::Nothing => {}
    }
}

fn create_menu_window(rect: &BarRect) -> Option<HWND> {
    let instance = unsafe { GetModuleHandleW(None) }.ok()?;
    let class_name = wide(MENU_WINDOW_CLASS);
    unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOREDIRECTIONBITMAP,
            PCWSTR(class_name.as_ptr()),
            windows::core::w!("WinRestyle Start Menu"),
            WS_POPUP, // hidden until shown; activatable — it takes the keyboard
            rect.x,
            rect.y,
            rect.w,
            rect.h,
            None,
            None,
            instance,
            None,
        )
    }
    .map_err(|e| log::error!("start menu window creation failed: {e}"))
    .ok()
}

/// Open the menu anchored to `bar_hwnd`'s bar: fresh app scan, lazily created
/// window, then show + activate. Every pumping call (window creation,
/// `SetWindowPos`, `SetForegroundWindow`) runs with no `STATE` borrow held.
fn open_menu(bar_hwnd: HWND) {
    let Some((mon, dpi, topmost)) = STATE.with(|s| {
        let s = s.borrow();
        let st = s.as_ref()?;
        let bar = bar_ref(&st.bars, bar_hwnd)?;
        Some((bar.mon, bar.dpi, st.topmost))
    }) else {
        return;
    };

    // File I/O and window geometry, unborrowed.
    let apps = apps::scan(&apps::roots());
    let count = apps.len();
    let mut wr = RECT::default();
    if unsafe { GetWindowRect(bar_hwnd, &mut wr) }.is_err() {
        return;
    }
    let bar_rect = BarRect {
        x: wr.left,
        y: wr.top,
        w: wr.right - wr.left,
        h: wr.bottom - wr.top,
    };
    let rect = startmenu::menu_rect(mon, bar_rect, dpi);

    let exists = STATE.with(|s| s.borrow().as_ref().is_some_and(|st| st.menu.is_some()));
    if !exists {
        let Some(hwnd) = create_menu_window(&rect) else {
            return;
        };
        let renderer = match Renderer::new(hwnd, rect.w, rect.h) {
            Ok(r) => r,
            Err(e) => {
                log::error!("start menu renderer failed: {e:#}");
                let _ = unsafe { DestroyWindow(hwnd) };
                return;
            }
        };
        STATE.with(|s| {
            if let Some(st) = s.borrow_mut().as_mut() {
                st.menu = Some(StartMenu {
                    hwnd: hwnd.0 as isize,
                    renderer,
                    dpi,
                    size: (rect.w, rect.h),
                    layout: startmenu::menu_layout(rect.w, rect.h, dpi, 0, 0),
                    state: startmenu::MenuState::default(),
                    apps: Vec::new(),
                    filtered: Vec::new(),
                    hovered: None,
                    mouse_tracking: false,
                    visible: false,
                    dismissed_at: 0,
                });
            }
        });
    }

    let Some(hwnd) = STATE.with(move |s| {
        let mut s = s.borrow_mut();
        let m = s.as_mut()?.menu.as_mut()?;
        m.filtered = (0..count).collect();
        m.apps = apps;
        m.state = startmenu::MenuState::default();
        m.hovered = None;
        m.dpi = dpi;
        m.layout = startmenu::menu_layout(rect.w, rect.h, dpi, count, 0);
        // A reopen on another monitor resizes the swapchain (no pump: D3D).
        if m.size != (rect.w, rect.h) {
            if let Err(e) = m.renderer.resize(rect.w, rect.h) {
                log::error!("start menu swapchain resize failed: {e}");
            }
        }
        m.size = (rect.w, rect.h);
        m.visible = true;
        Some(m.hwnd)
    }) else {
        return;
    };

    let hwnd = HWND(hwnd as _);
    unsafe {
        // No SWP_NOACTIVATE: unlike the bars, the menu wants the focus so
        // typing filters and deactivation dismisses.
        let _ = SetWindowPos(
            hwnd,
            if topmost { HWND_TOPMOST } else { HWND_TOP },
            rect.x,
            rect.y,
            rect.w,
            rect.h,
            SWP_SHOWWINDOW,
        );
        if !SetForegroundWindow(hwnd).as_bool() {
            // Focus lock (e.g. a posted, not real, click): the menu is up but
            // keyboard and click-away dismissal won't work — Esc via a real
            // focus later, another Start click, or launching still close it.
            log::warn!("start menu: foreground refused; keyboard filter unavailable");
        }
    }
    log::info!("start menu opened: {count} apps");
    redraw_menu();
}

/// Hide the menu. Safe re-entrantly: the `visible` flag flips before the
/// `ShowWindow` whose `WA_INACTIVE` re-enters this function.
fn hide_menu() {
    let hwnd = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let m = s.as_mut()?.menu.as_mut()?;
        if !m.visible {
            return None;
        }
        m.visible = false;
        m.dismissed_at = tick_ms();
        Some(m.hwnd)
    });
    if let Some(hwnd) = hwnd {
        let _ = unsafe { ShowWindow(HWND(hwnd as _), SW_HIDE) };
        log::info!("start menu closed");
    }
}

/// Repaint the menu (no-op while hidden). Holds the borrow across the draw
/// like `redraw_bars`; D2D/DXGI don't pump.
fn redraw_menu() {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(st) = s.as_mut() else { return };
        let State { config, menu, .. } = st;
        let Some(m) = menu.as_mut() else { return };
        if !m.visible {
            return;
        }
        let rows: Vec<MenuRow> = m
            .layout
            .rows
            .iter()
            .map(|(fidx, rect)| MenuRow {
                rect: *rect,
                name: m.apps[m.filtered[*fidx]].name.as_str(),
                selected: *fidx == m.state.selected,
                hovered: m.hovered == Some(*fidx),
            })
            .collect();
        let frame = MenuFrame {
            bar: &config.taskbar,
            dpi: m.dpi,
            search: m.layout.search,
            filter: &m.state.filter,
            rows: &rows,
            scrollbar: m.layout.scrollbar,
            no_matches: m.filtered.is_empty() && !m.state.filter.is_empty(),
        };
        match m.renderer.draw_menu(&frame) {
            Ok(()) => {}
            Err(e) if e.code() == D2DERR_RECREATE_TARGET => {
                log::warn!("start menu render target lost; recreating renderer");
                match Renderer::new(HWND(m.hwnd as _), m.size.0, m.size.1) {
                    Ok(r) => {
                        m.renderer = r;
                        if let Err(e) = m.renderer.draw_menu(&frame) {
                            log::error!("menu draw after renderer rebuild failed: {e}");
                        }
                    }
                    Err(e) => log::error!("start menu renderer rebuild failed: {e:#}"),
                }
            }
            Err(e) => log::error!("start menu draw failed: {e}"),
        }
    });
}

/// Recompute the filtered list and layout after the filter changed.
fn refilter_menu(m: &mut StartMenu) {
    m.filtered = apps::filter_indices(&m.apps, &m.state.filter);
    m.hovered = None;
    m.layout = startmenu::menu_layout(m.size.0, m.size.1, m.dpi, m.filtered.len(), m.state.scroll);
}

/// Launch the app at filtered index `fidx` (`None` = current selection) and
/// dismiss the menu. The launch pumps; the path is snapshotted first.
fn launch_from_menu(fidx: Option<usize>) {
    let path = STATE.with(|s| {
        let s = s.borrow();
        let m = s.as_ref()?.menu.as_ref()?;
        let idx = *m.filtered.get(fidx.unwrap_or(m.state.selected))?;
        Some(m.apps.get(idx)?.path.clone())
    });
    if let Some(path) = path {
        hide_menu();
        winlist::launch_app(&path);
    }
}

fn on_menu_key(wparam: WPARAM) {
    let vk = wparam.0 as u16;
    if vk == VK_ESCAPE.0 {
        hide_menu();
        return;
    }
    if vk == VK_RETURN.0 {
        launch_from_menu(None);
        return;
    }
    let delta = if vk == VK_UP.0 {
        -1
    } else if vk == VK_DOWN.0 {
        1
    } else {
        return;
    };
    let changed = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(m) = s.as_mut().and_then(|st| st.menu.as_mut()) else {
            return false;
        };
        let before = m.state.clone();
        m.state
            .move_selection(delta, m.filtered.len(), m.layout.fit);
        if m.state == before {
            return false;
        }
        m.layout =
            startmenu::menu_layout(m.size.0, m.size.1, m.dpi, m.filtered.len(), m.state.scroll);
        true
    });
    if changed {
        redraw_menu();
    }
}

fn on_menu_char(wparam: WPARAM) {
    // WM_CHAR delivers UTF-16 units; a lone surrogate half isn't a char and
    // is dropped (a filter can live without astral-plane characters).
    let Some(c) = char::from_u32(wparam.0 as u32) else {
        return;
    };
    let changed = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(m) = s.as_mut().and_then(|st| st.menu.as_mut()) else {
            return false;
        };
        if !m.state.on_char(c) {
            return false;
        }
        refilter_menu(m);
        true
    });
    if changed {
        redraw_menu();
    }
}

fn on_menu_wheel(delta: i32) {
    let rows = -delta / 120 * 3; // three rows per notch; wheel-up scrolls up
    if rows == 0 {
        return;
    }
    let changed = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(m) = s.as_mut().and_then(|st| st.menu.as_mut()) else {
            return false;
        };
        let before = m.state.scroll;
        m.state.on_wheel(rows, m.filtered.len(), m.layout.fit);
        if m.state.scroll == before {
            return false;
        }
        m.layout =
            startmenu::menu_layout(m.size.0, m.size.1, m.dpi, m.filtered.len(), m.state.scroll);
        // The row under the resting cursor changed; re-derived on next move.
        m.hovered = None;
        true
    });
    if changed {
        redraw_menu();
    }
}

fn on_menu_mouse_move(hwnd: HWND, x: i32, y: i32) {
    let (hover_changed, arm) = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(m) = s.as_mut().and_then(|st| st.menu.as_mut()) else {
            return (false, false);
        };
        let hovered = m.layout.hit_row(x, y);
        let changed = hovered != m.hovered;
        m.hovered = hovered;
        let arm = !m.mouse_tracking;
        m.mouse_tracking = true;
        (changed, arm)
    });
    if arm {
        let mut track = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        if unsafe { TrackMouseEvent(&mut track) }.is_err() {
            STATE.with(|s| {
                if let Some(m) = s.borrow_mut().as_mut().and_then(|st| st.menu.as_mut()) {
                    m.mouse_tracking = false; // retry on the next move
                }
            });
        }
    }
    if hover_changed {
        redraw_menu();
    }
}

extern "system" fn menu_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ACTIVATE => {
            // Clicking anywhere else deactivates the menu — that IS dismissal.
            if (wparam.0 & 0xffff) as u32 == WA_INACTIVE {
                hide_menu();
            }
            // Always fall through: DefWindowProc's WM_ACTIVATE handling is
            // what gives an activated window the keyboard focus.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_KEYDOWN => {
            on_menu_key(wparam);
            LRESULT(0)
        }
        WM_CHAR => {
            on_menu_char(wparam);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let hit = STATE.with(|s| {
                s.borrow()
                    .as_ref()
                    .and_then(|st| st.menu.as_ref())
                    .and_then(|m| m.layout.hit_row(mouse_x(lparam), mouse_y(lparam)))
            });
            if let Some(fidx) = hit {
                launch_from_menu(Some(fidx));
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            on_menu_mouse_move(hwnd, mouse_x(lparam), mouse_y(lparam));
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            let had_hover = STATE.with(|s| {
                let mut s = s.borrow_mut();
                let Some(m) = s.as_mut().and_then(|st| st.menu.as_mut()) else {
                    return false;
                };
                m.mouse_tracking = false;
                m.hovered.take().is_some()
            });
            if had_hover {
                redraw_menu();
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            on_menu_wheel(((wparam.0 >> 16) as u16 as i16) as i32);
            LRESULT(0)
        }
        // Deliberate teardown only (bar rebuild); never quits the pump.
        WM_DESTROY => LRESULT(0),
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

// ---------------------------------------------------------------------------
// Tray host (swapped sessions only; see ADR 0005)
// ---------------------------------------------------------------------------

/// Create the hidden `Shell_TrayWnd` window that applications' calls to
/// `Shell_NotifyIcon` find and send `WM_COPYDATA` registrations to. Returns
/// 0 (no hosting) on failure — a taskbar without a tray beats no taskbar.
fn create_tray_host() -> isize {
    let instance = match unsafe { GetModuleHandleW(None) } {
        Ok(i) => i,
        Err(_) => return 0,
    };
    let class_name = wide("Shell_TrayWnd");
    let class = WNDCLASSW {
        lpfnWndProc: Some(tray_wndproc),
        hInstance: instance.into(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        log::warn!("tray host: class registration failed; tray hosting off");
        return 0;
    }
    match unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            PCWSTR(class_name.as_ptr()),
            windows::core::w!(""),
            WS_POPUP, // never WS_VISIBLE: apps find it by class, not by sight
            0,
            0,
            0,
            0,
            None,
            None,
            instance,
            None,
        )
    } {
        Ok(hwnd) => hwnd.0 as isize,
        Err(e) => {
            log::warn!("tray host: window creation failed: {e}; tray hosting off");
            0
        }
    }
}

extern "system" fn tray_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_COPYDATA {
        return on_tray_copydata(lparam);
    }
    if msg == WM_DESTROY {
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// A `Shell_NotifyIcon` registration arrived. This runs re-entrantly under
/// whatever our thread was doing whenever that code pumps sent messages —
/// which is exactly why nothing in this file holds a `STATE` borrow across
/// a pumping call.
fn on_tray_copydata(lparam: LPARAM) -> LRESULT {
    let cds = unsafe { &*(lparam.0 as *const COPYDATASTRUCT) };
    if cds.dwData != 1 || cds.lpData.is_null() {
        // dwData 0 is the appbar channel (ABM_*); not hosted in this slice.
        return LRESULT(0);
    }
    let buf = unsafe { std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize) };
    let Some(cmd) = tray::parse(buf) else {
        return LRESULT(0);
    };
    let key = (cmd.data.owner, cmd.data.uid);
    // Decode the (foreign, shared — never destroy) HICON into pixels before
    // touching STATE mutably — and only when the handle actually changed;
    // apps re-send the same icon constantly and the GDI decode isn't free.
    let sends_icon = cmd.data.flags & tray::NIF_ICON != 0 && cmd.data.hicon != 0;
    let icon_changed = sends_icon
        && STATE.with(|s| {
            s.borrow().as_ref().is_some_and(|st| {
                st.tray
                    .iter()
                    .find(|t| (t.owner, t.uid) == key)
                    .is_none_or(|t| t.hicon != cmd.data.hicon)
            })
        });
    let pixels = icon_changed
        .then(|| winlist::foreign_icon_pixels(cmd.data.hicon))
        .flatten();
    let (applied, membership_changed) = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(st) = s.as_mut() else {
            return (
                tray::Applied {
                    handled: false,
                    repaint: false,
                },
                false,
            );
        };
        let before = visible_tray(&st.tray).count();
        let applied = tray::apply(&mut st.tray, &cmd);
        if applied.handled {
            match cmd.op {
                tray::NIM_DELETE => {
                    st.tray_pixels.remove(&key);
                }
                _ if icon_changed => {
                    st.tray_pixels.insert(key, pixels);
                }
                _ => {}
            }
        }
        let after = visible_tray(&st.tray).count();
        if applied.handled {
            log::info!(
                "tray: op {} from {:#x} uid {} ({} visible)",
                cmd.op,
                cmd.data.owner,
                cmd.data.uid,
                after
            );
        }
        (applied, before != after)
    });
    if membership_changed {
        relayout_all();
    } else if applied.repaint {
        redraw_bars(None);
    }
    // The BOOL Shell_NotifyIcon hands back to the app: success even for
    // handled-but-invisible updates (tip, version), failure only for
    // rejected commands.
    LRESULT(if applied.handled { 1 } else { 0 })
}

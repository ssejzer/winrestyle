//! The taskbar window: creation, positioning, and the message pump.
//!
//! Everything runs on the main thread (the test-flag threads in `main` are
//! the only others). Repaints happen on demand — clock ticks, config changes,
//! display changes — not per frame, keeping idle cost near zero.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{D2DERR_RECREATE_TARGET, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::HiDpi::{
    GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW,
    GetSystemMetrics, LoadCursorW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
    SetTimer, SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, IDC_ARROW, MSG, SM_CXSCREEN, SM_CYSCREEN,
    SWP_NOACTIVATE, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_LBUTTONDOWN, WM_MOUSEMOVE,
    WM_TIMER, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_POPUP,
    WS_VISIBLE,
};

use wr_core::config::{Config, ConfigStore};

use crate::layout;
use crate::render::{Frame, Renderer};
use crate::tasks::{self, TaskWindow};
use crate::winlist;

const CLOCK_TIMER: usize = 1;

/// `windows-rs` files this under `UI_Controls`; not worth a whole feature
/// for one well-known message id.
const WM_MOUSELEAVE: u32 = 0x02A3;

struct State {
    store: Arc<ConfigStore>,
    config: Config,
    renderer: Renderer,
    dpi: u32,
    /// Topmost only in a real swapped session; in an unswapped dev/test run
    /// explorer's taskbar is live and we must not sit on top of it.
    topmost: bool,
    clock: String,
    /// Taskbar-worthy windows in stable button order.
    tasks: Vec<TaskWindow>,
    /// Chip rectangle for `tasks[i]` (the tail may be dropped on overflow).
    rects: Vec<layout::BarRect>,
    /// Square for the Start button at the bar's left edge.
    start: layout::BarRect,
    /// Foreground window handle (0 = none), for the highlighted chip.
    active: isize,
    /// Decoded icons per window; `Some(None)` remembers "asked, has none"
    /// so windows without icons aren't re-queried on every refresh.
    icons: HashMap<isize, Option<tasks::Icon>>,
    /// Index of the chip under the mouse.
    hovered: Option<usize>,
    /// Whether the mouse is over the Start button.
    start_hovered: bool,
    /// Whether a `WM_MOUSELEAVE` request is currently armed.
    mouse_tracking: bool,
    /// Id of the registered `CONFIG_CHANGED_MESSAGE` the shell posts to us.
    config_changed_msg: u32,
    /// Log the next successful draw (startup and config changes) so the VM
    /// harness can assert paints happen.
    log_next_paint: bool,
}

thread_local! {
    // One taskbar window per process; the window proc runs on this thread.
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn run(store: Arc<ConfigStore>) -> anyhow::Result<()> {
    // Physical pixels everywhere; per-monitor DPI handling proper (multiple
    // monitors, WM_DPICHANGED rescale) is a later Phase 2 item.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let dpi = unsafe { GetDpiForSystem() };
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

    let rect = layout::bar_rect(
        unsafe { GetSystemMetrics(SM_CXSCREEN) },
        unsafe { GetSystemMetrics(SM_CYSCREEN) },
        config.taskbar.height,
        config.taskbar.margin,
        dpi,
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
        )?
    };
    unsafe {
        SetWindowPos(
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
        )?;
    }

    let renderer = Renderer::new(hwnd, rect.w, rect.h)?;
    let config_changed_msg =
        unsafe { RegisterWindowMessageW(PCWSTR(wide(wr_core::CONFIG_CHANGED_MESSAGE).as_ptr())) };
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            store,
            config,
            renderer,
            dpi,
            topmost,
            clock: String::new(),
            tasks: Vec::new(),
            rects: Vec::new(),
            start: layout::start_rect(rect.h, dpi),
            active: 0,
            icons: HashMap::new(),
            hovered: None,
            start_hovered: false,
            mouse_tracking: false,
            config_changed_msg,
            log_next_paint: true,
        })
    });
    log::info!(
        "taskbar window up ({}x{} at {},{}; dpi {dpi}; {})",
        rect.w,
        rect.h,
        rect.x,
        rect.y,
        if topmost {
            "topmost"
        } else {
            "not topmost (another desktop shell is on screen)"
        }
    );
    // Buttons: event-driven from here on; the initial refresh paints too.
    winlist::install_hooks(hwnd);
    refresh_windows(hwnd);
    redraw(hwnd);
    unsafe { SetTimer(hwnd, CLOCK_TIMER, 1000, None) };

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        unsafe {
            DispatchMessageW(&msg);
        }
    }
    Ok(())
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
        on_config_changed(hwnd);
        return LRESULT(0);
    }
    match msg {
        winlist::WM_WINDOWS_CHANGED => {
            // Re-arm first: events landing during the refresh must re-post.
            winlist::ack_refresh();
            refresh_windows(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xffff) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xffff) as i16 as i32;
            let (on_start, target) = STATE.with(|s| {
                s.borrow().as_ref().map_or((false, None), |st| {
                    if st.start.contains(x, y) {
                        return (true, None);
                    }
                    let target = layout::hit_test(&st.rects, x, y)
                        .and_then(|i| st.tasks.get(i))
                        .map(|t| t.hwnd);
                    (false, target)
                })
            });
            if on_start {
                log::info!("start button clicked (stub: tapping the Win key)");
                winlist::open_start_menu();
            } else if let Some(target) = target {
                winlist::activate(target);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xffff) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xffff) as i16 as i32;
            let (hover_changed, arm) = STATE.with(|s| {
                let mut s = s.borrow_mut();
                let Some(st) = s.as_mut() else {
                    return (false, false);
                };
                let start_hovered = st.start.contains(x, y);
                let hovered = if start_hovered {
                    None
                } else {
                    layout::hit_test(&st.rects, x, y)
                };
                let changed = hovered != st.hovered || start_hovered != st.start_hovered;
                st.hovered = hovered;
                st.start_hovered = start_hovered;
                let arm = !st.mouse_tracking;
                st.mouse_tracking = true;
                (changed, arm)
            });
            if arm {
                // Ask for one WM_MOUSELEAVE so the hover highlight clears
                // when the mouse leaves the bar.
                let mut track = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                if unsafe { TrackMouseEvent(&mut track) }.is_err() {
                    STATE.with(|s| {
                        if let Some(st) = s.borrow_mut().as_mut() {
                            st.mouse_tracking = false; // retry on the next move
                        }
                    });
                }
            }
            if hover_changed {
                redraw(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            let had_hover = STATE.with(|s| {
                let mut s = s.borrow_mut();
                let Some(st) = s.as_mut() else { return false };
                st.mouse_tracking = false;
                let had_chip = st.hovered.take().is_some();
                std::mem::take(&mut st.start_hovered) || had_chip
            });
            if had_hover {
                redraw(hwnd);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == CLOCK_TIMER => {
            let stale = STATE.with(|s| {
                s.borrow()
                    .as_ref()
                    .is_some_and(|st| st.clock != clock_string())
            });
            if stale {
                redraw(hwnd);
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            apply_layout(hwnd);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let dpi = (wparam.0 & 0xffff) as u32;
            STATE.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut() {
                    st.dpi = dpi.max(1);
                }
            });
            apply_layout(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Re-snapshot the config from disk (the shell posts the config-changed
/// message *after* the file was rewritten) and re-apply geometry + paint.
fn on_config_changed(hwnd: HWND) {
    let changed = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(st) = s.as_mut() else { return false };
        let new = st.store.reload();
        if new == st.config {
            return false;
        }
        st.config = new;
        st.log_next_paint = true;
        true
    });
    if changed {
        apply_layout(hwnd);
    }
}

/// Recompute the bar rectangle and apply it: move/size the window, resize the
/// swapchain, repaint. `SetWindowPos` runs outside the state borrow — it
/// dispatches messages synchronously into `wndproc`.
fn apply_layout(hwnd: HWND) {
    let Some((rect, topmost)) = STATE.with(|s| {
        s.borrow().as_ref().map(|st| {
            (
                layout::bar_rect(
                    unsafe { GetSystemMetrics(SM_CXSCREEN) },
                    unsafe { GetSystemMetrics(SM_CYSCREEN) },
                    st.config.taskbar.height,
                    st.config.taskbar.margin,
                    st.dpi,
                ),
                st.topmost,
            )
        })
    }) else {
        return;
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
    STATE.with(|s| {
        if let Some(st) = s.borrow_mut().as_mut() {
            if let Err(e) = st.renderer.resize(rect.w, rect.h) {
                log::error!("swapchain resize failed: {e}");
            }
            st.rects = layout::button_rects(rect.w, rect.h, st.tasks.len(), st.dpi);
            st.start = layout::start_rect(rect.h, st.dpi);
        }
    });
    redraw(hwnd);
}

/// Re-enumerate the window population, merge it into the button list, and
/// repaint if anything the user can see changed. Triggered by the WinEvent
/// hooks (coalesced), and once at startup.
fn refresh_windows(hwnd: HWND) {
    let fresh = winlist::enumerate();
    let active = winlist::foreground();
    let (changed, added, removed) = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(st) = s.as_mut() else {
            return (false, Vec::new(), Vec::new());
        };
        let (merged, added, removed) = tasks::merge(&st.tasks, &fresh);
        for w in &added {
            log::info!("taskbar: window added: {:?}", w.title);
        }
        for w in &removed {
            log::info!("taskbar: window removed: {:?}", w.title);
        }
        let list_changed = merged != st.tasks;
        if list_changed {
            if merged.len() != st.tasks.len() {
                log::info!("taskbar windows: {}", merged.len());
            }
            let mut client = RECT::default();
            let _ = unsafe { GetClientRect(hwnd, &mut client) };
            st.rects = layout::button_rects(
                client.right - client.left,
                client.bottom - client.top,
                merged.len(),
                st.dpi,
            );
            st.tasks = merged;
            // The chip under the cursor may have shifted; the highlight is
            // re-derived on the next mouse move.
            if st.hovered.is_some_and(|i| i >= st.rects.len()) {
                st.hovered = None;
            }
        }
        let active_changed = st.active != active;
        st.active = active;
        (list_changed || active_changed, added, removed)
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
                    log::debug!(
                        "taskbar: icon for {hwnd:#x}: {}",
                        icon.as_ref()
                            .map_or("none".to_string(), |i| format!("{}x{}", i.width, i.height))
                    );
                    st.icons.insert(hwnd, icon);
                }
            }
        });
    }
    if changed {
        redraw(hwnd);
    }
}

fn redraw(hwnd: HWND) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(st) = s.as_mut() else { return };
        st.clock = clock_string();
        let frame = Frame {
            bar: &st.config.taskbar,
            clock: &st.clock,
            dpi: st.dpi,
            tasks: &st.tasks,
            rects: &st.rects,
            start: st.start,
            active: st.active,
            icons: &st.icons,
            hovered: st.hovered,
            start_hovered: st.start_hovered,
        };
        match st.renderer.draw(&frame) {
            Ok(()) => {
                if st.log_next_paint {
                    st.log_next_paint = false;
                    log::info!(
                        "taskbar painted: color {} alpha {}",
                        st.config.taskbar.color,
                        st.config.taskbar.alpha
                    );
                }
            }
            // The device was lost (driver reset, session change): rebuild the
            // whole rendering stack and try once more.
            Err(e) if e.code() == D2DERR_RECREATE_TARGET => {
                log::warn!("render target lost; recreating renderer");
                let mut rect = RECT::default();
                let _ = unsafe { GetClientRect(hwnd, &mut rect) };
                match Renderer::new(hwnd, rect.right - rect.left, rect.bottom - rect.top) {
                    Ok(r) => {
                        st.renderer = r;
                        let frame = Frame {
                            bar: &st.config.taskbar,
                            clock: &st.clock,
                            dpi: st.dpi,
                            tasks: &st.tasks,
                            rects: &st.rects,
                            start: st.start,
                            active: st.active,
                            icons: &st.icons,
                            hovered: st.hovered,
                            start_hovered: st.start_hovered,
                        };
                        if let Err(e) = st.renderer.draw(&frame) {
                            log::error!("draw after renderer rebuild failed: {e}");
                        }
                    }
                    Err(e) => log::error!("renderer rebuild failed: {e:#}"),
                }
            }
            Err(e) => log::error!("taskbar draw failed: {e}"),
        }
    });
}

fn clock_string() -> String {
    let t = unsafe { GetLocalTime() };
    format!("{:02}:{:02}", t.wHour, t.wMinute)
}

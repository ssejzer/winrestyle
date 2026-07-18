//! The taskbar window: creation, positioning, and the message pump.
//!
//! Everything runs on the main thread (the test-flag threads in `main` are
//! the only others). Repaints happen on demand — clock ticks, config changes,
//! display changes — not per frame, keeping idle cost near zero.

use std::cell::RefCell;
use std::sync::Arc;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{D2DERR_RECREATE_TARGET, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::HiDpi::{
    GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW,
    GetSystemMetrics, LoadCursorW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
    SetTimer, SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, IDC_ARROW, MSG, SM_CXSCREEN, SM_CYSCREEN,
    SWP_NOACTIVATE, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_TIMER, WNDCLASSW,
    WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE,
};

use wr_core::config::{Config, ConfigStore};

use crate::layout;
use crate::render::Renderer;

const CLOCK_TIMER: usize = 1;

struct State {
    store: Arc<ConfigStore>,
    config: Config,
    renderer: Renderer,
    dpi: u32,
    /// Topmost only in a real swapped session; in an unswapped dev/test run
    /// explorer's taskbar is live and we must not sit on top of it.
    topmost: bool,
    clock: String,
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
        }
    });
    redraw(hwnd);
}

fn redraw(hwnd: HWND) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(st) = s.as_mut() else { return };
        st.clock = clock_string();
        match st.renderer.draw(&st.config.taskbar, &st.clock, st.dpi) {
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
                        if let Err(e) = st.renderer.draw(&st.config.taskbar, &st.clock, st.dpi) {
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

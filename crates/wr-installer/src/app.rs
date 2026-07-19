//! The manager window: creation, the message pump, and the model state behind
//! the checklist. One ordinary top-level window, all on the main thread.
//!
//! ## Concurrency rule (borrowed from the taskbar)
//!
//! `MessageBoxW` and the apply/restore actions pump messages (a modal loop, a
//! spawned process). Anything that can pump must run with **no `STATE` borrow
//! held**, or a message re-entering `wndproc` panics the `RefCell`. The pattern
//! throughout: snapshot under a short borrow, drop it, do the pumping call,
//! re-borrow to store the result — exactly `bar.rs`'s discipline.

use std::cell::RefCell;
use std::collections::BTreeSet;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{D2DERR_RECREATE_TARGET, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, InvalidateRect, UpdateWindow, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetSystemMetrics, LoadCursorW,
    MessageBoxW, PostQuitMessage, RegisterClassW, SetWindowPos, ShowWindow, CW_USEDEFAULT,
    IDC_ARROW, IDYES, MB_ICONINFORMATION, MB_ICONQUESTION, MB_ICONWARNING, MB_OK, MB_YESNO,
    MINMAXINFO, MSG, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOZORDER, SW_SHOW, WHEEL_DELTA,
    WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_GETMINMAXINFO, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_PAINT, WM_SIZE, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

/// `windows-rs` files `WM_MOUSELEAVE` under `UI_Controls`; not worth a whole
/// feature for one well-known message id (same as the taskbar's `bar.rs`).
const WM_MOUSELEAVE: u32 = 0x02A3;

use wr_core::autostart::AutostartEntry;
use wr_core::components::Registry;
use wr_core::config::{Config, ConfigStore};

use crate::render::{client_size, Frame, Renderer, Row};
use crate::view::{self, Hit};

const CLASS_NAME: &str = "WinRestyleManager";

/// The whole manager state, behind the thread-local `STATE` cell.
struct Manager {
    registry: Registry,
    /// The config as last loaded/applied — the base every edit builds on.
    base_config: Config,
    /// Checked component ids.
    selected: BTreeSet<String>,
    /// Enumerated logon-startup entries.
    entries: Vec<AutostartEntry>,
    /// Per-entry checkbox: true = "let it run" (i.e. not on the disabled list).
    entry_checked: Vec<bool>,
    scroll: i32,
    hovered: Option<Hit>,
    dpi: u32,
    /// One-line status shown in the footer.
    status: String,
    /// Subtitle: whether WinRestyle is currently the registered shell.
    subtitle: String,
    /// Buttons dim and stop responding while an apply/restore runs.
    busy: bool,
    renderer: Renderer,
    mouse_tracking: bool,
}

thread_local! {
    static STATE: RefCell<Option<Manager>> = const { RefCell::new(None) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Load the current config + startup entries into a fresh model.
fn build_model(dpi: u32, renderer: Renderer) -> Manager {
    let registry = Registry::all();
    let base_config = ConfigStore::load_default().get();
    let selected = registry.installed_ids(&base_config);
    let entries = wr_core::autostart::enumerate();
    let entry_checked = entries
        .iter()
        .map(|e| !base_config.autostart.is_disabled(&e.id))
        .collect();
    Manager {
        registry,
        selected,
        entry_checked,
        entries,
        scroll: 0,
        hovered: None,
        dpi,
        status: String::new(),
        subtitle: shell_status_line(),
        busy: false,
        renderer,
        mouse_tracking: false,
        base_config,
    }
}

/// A one-line summary of whether WinRestyle is the registered shell right now.
fn shell_status_line() -> String {
    match wr_core::shell::has_backup() {
        Ok(true) if wr_core::process::any_named(wr_core::WATCHDOG_EXE) => {
            "WinRestyle is installed and active in this session.".to_string()
        }
        Ok(true) => "WinRestyle is installed — active at the next login (or Restyle Now → \
                     activate)."
            .to_string(),
        Ok(false) => "Not installed — the standard Windows shell is active.".to_string(),
        Err(e) => format!("Could not read shell state: {e}"),
    }
}

pub fn run() -> anyhow::Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let instance = unsafe { GetModuleHandleW(None)? };
    let class_name = wide(CLASS_NAME);
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

    // Center a 96-DPI 560x680 window on the primary screen (DPI is applied
    // after creation, once the window's real monitor DPI is known).
    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let (w, h) = (560, 680);
    let (x, y) = (((sw - w) / 2).max(0), ((sh - h) / 2).max(0));
    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            PCWSTR(class_name.as_ptr()),
            w!("WinRestyle"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            if x == 0 { CW_USEDEFAULT } else { x },
            if y == 0 { CW_USEDEFAULT } else { y },
            w,
            h,
            None,
            None,
            instance,
            None,
        )?
    };

    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    let (cw, ch) = client_size(hwnd);
    let renderer = Renderer::new(hwnd, cw, ch)?;
    STATE.with(|s| *s.borrow_mut() = Some(build_model(dpi, renderer)));

    log::info!("manager window up ({cw}x{ch}, dpi {dpi})");
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
    }

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        unsafe {
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            do_paint(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1), // fully painted in WM_PAINT
        WM_MOUSEMOVE => {
            on_mouse_move(hwnd, mouse_x(lparam), mouse_y(lparam));
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            let changed = STATE.with(|s| {
                let mut s = s.borrow_mut();
                match s.as_mut() {
                    Some(st) => {
                        st.mouse_tracking = false;
                        st.hovered.take().is_some()
                    }
                    None => false,
                }
            });
            if changed {
                redraw(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            on_wheel(hwnd, ((wparam.0 >> 16) & 0xffff) as i16 as i32);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            on_click(hwnd, mouse_x(lparam), mouse_y(lparam));
            LRESULT(0)
        }
        WM_SIZE => {
            let (w, h) = (
                (lparam.0 & 0xffff) as i32,
                ((lparam.0 >> 16) & 0xffff) as i32,
            );
            STATE.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut() {
                    if let Err(e) = st.renderer.resize(w.max(1), h.max(1)) {
                        log::error!("render target resize failed: {e}");
                    }
                }
            });
            redraw(hwnd);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let dpi = ((wparam.0 & 0xffff) as u32).max(96);
            let suggested = unsafe { &*(lparam.0 as *const windows::Win32::Foundation::RECT) };
            STATE.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut() {
                    st.dpi = dpi;
                }
            });
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    suggested.left,
                    suggested.top,
                    suggested.right - suggested.left,
                    suggested.bottom - suggested.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            redraw(hwnd);
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            let dpi = STATE.with(|s| s.borrow().as_ref().map_or(96, |st| st.dpi));
            let mmi = unsafe { &mut *(lparam.0 as *mut MINMAXINFO) };
            mmi.ptMinTrackSize.x = view::scale(380, dpi);
            mmi.ptMinTrackSize.y = view::scale(320, dpi);
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
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

fn redraw(hwnd: HWND) {
    unsafe {
        let _ = InvalidateRect(hwnd, None, false);
    }
}

/// Compute the layout for the current client size + model.
fn current_layout(hwnd: HWND, st: &Manager) -> view::Layout {
    let (w, h) = client_size(hwnd);
    view::layout(
        w,
        h,
        st.dpi,
        st.registry.components().len(),
        st.entries.len(),
    )
}

fn on_wheel(hwnd: HWND, delta: i32) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(st) = s.as_mut() else { return };
        let layout = current_layout(hwnd, st);
        // Three rows per wheel notch feels right; scale the step to DPI.
        let step = view::scale(52, st.dpi) * 3 * delta / WHEEL_DELTA as i32;
        st.scroll = layout.clamp_scroll(st.scroll - step);
    });
    redraw(hwnd);
}

fn on_mouse_move(hwnd: HWND, x: i32, y: i32) {
    let (changed, arm) = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(st) = s.as_mut() else {
            return (false, false);
        };
        let layout = current_layout(hwnd, st);
        let hovered = layout.hit_test(x, y, st.scroll);
        let changed = hovered != st.hovered;
        st.hovered = hovered;
        let arm = !st.mouse_tracking;
        st.mouse_tracking = true;
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
                if let Some(st) = s.borrow_mut().as_mut() {
                    st.mouse_tracking = false;
                }
            });
        }
    }
    if changed {
        redraw(hwnd);
    }
}

fn on_click(hwnd: HWND, x: i32, y: i32) {
    let hit = STATE.with(|s| {
        let s = s.borrow();
        let st = s.as_ref()?;
        if st.busy {
            return None;
        }
        current_layout(hwnd, st).hit_test(x, y, st.scroll)
    });
    match hit {
        Some(Hit::Component(i)) => {
            STATE.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut() {
                    if let Some(c) = st.registry.components().get(i) {
                        let id = c.id().to_string();
                        if !st.selected.remove(&id) {
                            st.selected.insert(id);
                        }
                    }
                }
            });
            redraw(hwnd);
        }
        Some(Hit::Startup(i)) => {
            STATE.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut() {
                    if let Some(c) = st.entry_checked.get_mut(i) {
                        *c = !*c;
                    }
                }
            });
            redraw(hwnd);
        }
        Some(Hit::RestyleNow) => do_apply(hwnd),
        Some(Hit::Restore) => do_restore(hwnd),
        None => {}
    }
}

/// Build the config the current selection describes: component toggles plus the
/// per-entry startup opt-outs, on top of the last-loaded base.
fn build_selected_config(st: &Manager) -> Config {
    let mut config = st.registry.apply(&st.base_config, &st.selected);
    for (entry, checked) in st.entries.iter().zip(&st.entry_checked) {
        config.autostart.set_disabled(&entry.id, !checked);
    }
    config
}

fn do_apply(hwnd: HWND) {
    // Snapshot what to apply, mark busy, and force an immediate repaint so the
    // "Applying…" state shows before the (blocking) trial run.
    let built = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let st = s.as_mut()?;
        st.busy = true;
        st.status = "Applying… running a trial launch before touching the registry.".to_string();
        let config = build_selected_config(st);
        let path = wr_core::config::default_path();
        Some((config, path))
    });
    let Some((config, path)) = built else { return };
    unsafe {
        let _ = InvalidateRect(hwnd, None, false);
        let _ = UpdateWindow(hwnd);
    }

    // No STATE borrow held: apply_restyle spawns a trial process and MessageBoxW
    // pumps a modal loop — both would re-enter wndproc.
    let outcome = match path {
        Some(path) => wr_core::manager::apply_restyle(&path, &config),
        None => Err(anyhow::anyhow!(
            "APPDATA is not set, so there is nowhere to write the config"
        )),
    };
    match &outcome {
        Ok(o) => message_box(hwnd, "Restyle applied", &o.instructions, false),
        Err(e) => message_box(hwnd, "Could not apply", &format!("{e:#}"), true),
    }

    // Offer live activation (ADR 0008). Recovery instructions were shown
    // first, above, so the user has the hotkey before the desktop churns.
    let mut status = match &outcome {
        Ok(o) => o.headline.clone(),
        Err(e) => format!("Apply failed: {e}"),
    };
    if outcome.is_ok()
        && confirm_box(
            hwnd,
            "Activate now?",
            "Switch this session to the WinRestyle desktop right now?\n\
             \n\
             This restarts the Windows desktop — open File Explorer windows will \
             close. Choosing No keeps the standard desktop until your next sign-in.",
        )
    {
        STATE.with(|s| {
            if let Some(st) = s.borrow_mut().as_mut() {
                st.status = "Activating — switching this session to WinRestyle…".to_string();
            }
        });
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
            let _ = UpdateWindow(hwnd);
        }
        // activate_now sweeps, stops explorer, spawns the watchdog, and
        // sleeps while the new desktop settles — all pumping or blocking,
        // none of it under a STATE borrow.
        status = match wr_core::manager::activate_now() {
            Ok(o) => o.headline,
            Err(e) => {
                message_box(hwnd, "Could not activate", &format!("{e:#}"), true);
                format!("Live activation failed: {e}")
            }
        };
    }

    STATE.with(|s| {
        if let Some(st) = s.borrow_mut().as_mut() {
            st.busy = false;
            st.subtitle = shell_status_line();
            if outcome.is_ok() {
                st.base_config = config;
            }
            st.status = status;
        }
    });
    redraw(hwnd);
}

fn do_restore(hwnd: HWND) {
    STATE.with(|s| {
        if let Some(st) = s.borrow_mut().as_mut() {
            st.busy = true;
            st.status = "Restoring the Windows shell…".to_string();
        }
    });
    unsafe {
        let _ = InvalidateRect(hwnd, None, false);
        let _ = UpdateWindow(hwnd);
    }

    let outcome = wr_core::manager::uninstall();
    match &outcome {
        Ok(o) => message_box(
            hwnd,
            "Windows shell restored",
            &format!(
                "{o:?}. If WinRestyle was live in this session, it has been swept and \
                 the standard desktop is back — no sign-out needed.",
            ),
            false,
        ),
        Err(e) => message_box(hwnd, "Could not restore", &format!("{e:#}"), true),
    }

    STATE.with(|s| {
        if let Some(st) = s.borrow_mut().as_mut() {
            st.busy = false;
            st.subtitle = shell_status_line();
            st.status = match outcome {
                Ok(o) => format!("Restore: {o:?}."),
                Err(e) => format!("Restore failed: {e}"),
            };
        }
    });
    redraw(hwnd);
}

fn message_box(hwnd: HWND, caption: &str, body: &str, warning: bool) {
    let body = wide(body);
    let caption = wide(caption);
    unsafe {
        MessageBoxW(
            hwnd,
            PCWSTR(body.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_OK
                | if warning {
                    MB_ICONWARNING
                } else {
                    MB_ICONINFORMATION
                },
        );
    }
}

/// A Yes/No question; `true` = Yes. Pumps a modal loop — same rule as
/// [`message_box`]: never call with a `STATE` borrow held.
fn confirm_box(hwnd: HWND, caption: &str, body: &str) -> bool {
    let body = wide(body);
    let caption = wide(caption);
    unsafe {
        MessageBoxW(
            hwnd,
            PCWSTR(body.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_YESNO | MB_ICONQUESTION,
        ) == IDYES
    }
}

fn do_paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let _hdc = unsafe { BeginPaint(hwnd, &mut ps) };

    // Clamp scroll under a short mutable borrow (client size may have shrunk).
    STATE.with(|s| {
        if let Some(st) = s.borrow_mut().as_mut() {
            let layout = current_layout(hwnd, st);
            st.scroll = layout.clamp_scroll(st.scroll);
        }
    });

    // Draw under a shared borrow (paint_once builds the frame and draws).
    let lost = STATE.with(|s| {
        let s = s.borrow();
        match s.as_ref() {
            Some(st) => {
                matches!(paint_once(hwnd, st), Err(e) if e.code() == D2DERR_RECREATE_TARGET)
            }
            None => false,
        }
    });
    if lost {
        log::warn!("render target lost; recreating");
        let (w, h) = client_size(hwnd);
        let recreated = STATE.with(|s| {
            s.borrow_mut()
                .as_mut()
                .map(|st| st.renderer.recreate(w, h).is_ok())
                .unwrap_or(false)
        });
        if recreated {
            STATE.with(|s| {
                if let Some(st) = s.borrow().as_ref() {
                    if let Err(e) = paint_once(hwnd, st) {
                        log::error!("draw after recreate failed: {e}");
                    }
                }
            });
        }
    }

    unsafe {
        let _ = EndPaint(hwnd, &ps);
    }
}

/// Build the render frame from the model (owning the transient strings the
/// `Row`s borrow for the draw) and paint it. Takes `st` by shared reference so
/// the frame and the renderer can borrow disjoint fields at once.
fn paint_once(hwnd: HWND, st: &Manager) -> windows::core::Result<()> {
    let layout = current_layout(hwnd, st);
    let components: Vec<Row> = st
        .registry
        .components()
        .iter()
        .map(|c| Row {
            name: c.name(),
            detail: c.summary(),
            checked: st.selected.contains(c.id()),
        })
        .collect();
    // Startup detail lines are owned here and outlive the draw.
    let startup_details: Vec<String> = st
        .entries
        .iter()
        .map(|e| format!("{} · {}", e.source.label(), e.detail))
        .collect();
    let startup: Vec<Row> = st
        .entries
        .iter()
        .zip(&startup_details)
        .zip(&st.entry_checked)
        .map(|((entry, detail), checked)| Row {
            name: &entry.name,
            detail,
            checked: *checked,
        })
        .collect();
    let frame = Frame {
        layout: &layout,
        dpi: st.dpi,
        scroll: st.scroll,
        title: "WinRestyle",
        subtitle: st.subtitle.as_str(),
        components: &components,
        startup: &startup,
        status: st.status.as_str(),
        hovered: st.hovered,
        busy: st.busy,
    };
    st.renderer.draw(&frame)
}

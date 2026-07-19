//! The Phase 1 wallpaper: a bottom-most window covering the virtual screen,
//! filled with the configured solid color and (optionally) an image.
//!
//! Runs on its own thread with its own message pump, so the main thread's
//! supervision loop and the guardian threads are untouched. The wallpaper is
//! cosmetic: any failure here is logged and the shell keeps running — it must
//! never take the process down or block recovery.
//!
//! Rendering is plain GDI (+ WIC for image decode); Direct2D/DirectComposition
//! arrives with the Phase 2 taskbar.

use std::cell::RefCell;
use std::path::Path;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, GENERIC_READ, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect,
    SetStretchBltMode, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    HALFTONE, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppBGR, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW,
    GetSystemMetrics, LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassW, SetWindowPos,
    HWND_BOTTOM, IDC_ARROW, MSG, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, WINDOWPOS, WM_APP, WM_DESTROY,
    WM_DISPLAYCHANGE, WM_ERASEBKGND, WM_PAINT, WM_WINDOWPOSCHANGING, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE,
};

use wr_core::config::{Config, ConfigStore};

/// Posted by [`notify_config_changed`] (from the IPC thread) to re-snapshot
/// the config and repaint.
const WM_CONFIG_CHANGED: u32 = WM_APP + 1;

/// The wallpaper window, for cross-thread notification. Zero until created.
static WALLPAPER_HWND: AtomicIsize = AtomicIsize::new(0);

/// Spawn the wallpaper thread. Never fails; failures are logged from the
/// thread itself.
pub fn start(store: Arc<ConfigStore>) {
    std::thread::spawn(move || {
        if let Err(e) = run(store) {
            log::error!("wallpaper disabled: {e:#}");
        }
    });
}

/// Tell the wallpaper the config changed (safe from any thread). A no-op
/// until the window exists — the first paint reads a fresh snapshot anyway.
pub fn notify_config_changed() {
    let hwnd = WALLPAPER_HWND.load(Ordering::SeqCst);
    if hwnd != 0 {
        unsafe {
            let _ = PostMessageW(HWND(hwnd as _), WM_CONFIG_CHANGED, WPARAM(0), LPARAM(0));
        }
    }
}

/// Decoded image: 32bpp BGRX, top-down rows.
struct Image {
    width: i32,
    height: i32,
    pixels: Vec<u8>,
}

struct State {
    store: Arc<ConfigStore>,
    config: Config,
    image: Option<Image>,
    /// Log the next WM_PAINT (set at startup and on config change) so the VM
    /// harness can assert paints actually happen.
    log_next_paint: bool,
}

thread_local! {
    // One wallpaper window per process; the window proc runs on this thread.
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn run(store: Arc<ConfigStore>) -> anyhow::Result<()> {
    // WIC needs COM on this thread.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };

    let config = store.get();
    let image = load_image(&config);
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            store,
            config,
            image,
            log_next_paint: true,
        })
    });

    let instance = unsafe { GetModuleHandleW(None)? };
    let class_name = windows::core::w!("WinRestyleWallpaper");
    let class = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: class_name,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
        ..Default::default()
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(windows::core::Error::from_win32().into());
    }

    let (x, y, w, h) = virtual_screen();
    let hwnd = unsafe {
        CreateWindowExW(
            // Never activates, never shows in a window list.
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class_name,
            windows::core::w!("WinRestyle Wallpaper"),
            WS_POPUP | WS_VISIBLE,
            x,
            y,
            w,
            h,
            None,
            None,
            instance,
            None,
        )?
    };
    unsafe {
        SetWindowPos(
            hwnd,
            HWND_BOTTOM,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )?;
    }
    WALLPAPER_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
    log::info!("wallpaper window up ({w}x{h} at {x},{y})");

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
            paint(hwnd);
            LRESULT(0)
        }
        // WM_PAINT covers every pixel; skipping the erase avoids flicker.
        WM_ERASEBKGND => LRESULT(1),
        WM_CONFIG_CHANGED => {
            refresh_config(hwnd);
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            fit_to_screen(hwnd);
            LRESULT(0)
        }
        WM_WINDOWPOSCHANGING => {
            // Pin to the bottom of the Z-order: everything paints above the
            // wallpaper, always.
            unsafe {
                let pos = lparam.0 as *mut WINDOWPOS;
                if !pos.is_null() {
                    (*pos).hwndInsertAfter = HWND_BOTTOM;
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn paint(hwnd: HWND) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(state) = s.as_mut() else { return };
        // `effective_*` fold in the wallpaper master switch: styling off paints
        // the neutral default and ignores the image (wr-core::config).
        let color = state.config.wallpaper.effective_color();
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);

            // Color first: the backdrop while an image is absent or broken.
            let brush = CreateSolidBrush(COLORREF(
                ((color.b as u32) << 16) | ((color.g as u32) << 8) | color.r as u32,
            ));
            FillRect(hdc, &rect, brush);
            let _ = DeleteObject(brush);

            if let Some(img) = &state.image {
                let bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: img.width,
                        biHeight: -img.height, // negative = top-down rows
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                SetStretchBltMode(hdc, HALFTONE);
                StretchDIBits(
                    hdc,
                    0,
                    0,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    0,
                    0,
                    img.width,
                    img.height,
                    Some(img.pixels.as_ptr().cast()),
                    &bmi,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                );
            }
            let _ = EndPaint(hwnd, &ps);
        }
        if state.log_next_paint {
            state.log_next_paint = false;
            log::info!(
                "wallpaper painted: color {}, image {}",
                color,
                state
                    .image
                    .as_ref()
                    .map_or("none".to_string(), |i| format!("{}x{}", i.width, i.height)),
            );
        }
    });
}

/// Re-snapshot the config (the `ReloadConfig` path) and repaint if anything
/// the wallpaper shows has changed.
fn refresh_config(hwnd: HWND) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(state) = s.as_mut() else { return };
        let new = state.store.get();
        if new == state.config {
            return;
        }
        // Reload on any change to what actually renders — including the master
        // switch flipping, which changes the effective image without touching
        // the raw `image` field.
        if new.wallpaper.effective_image() != state.config.wallpaper.effective_image() {
            state.image = load_image(&new);
        }
        state.config = new;
        state.log_next_paint = true;
        unsafe {
            let _ = InvalidateRect(hwnd, None, true);
        }
    });
}

fn virtual_screen() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
        )
    }
}

fn fit_to_screen(hwnd: HWND) {
    let (x, y, w, h) = virtual_screen();
    unsafe {
        let _ = SetWindowPos(hwnd, HWND_BOTTOM, x, y, w, h, SWP_NOACTIVATE);
    }
    log::info!("display changed; wallpaper resized to {w}x{h} at {x},{y}");
}

/// Decode the configured image, if any. A missing or broken image is a logged
/// warning and a `None` — the solid color shows instead (never fatal).
fn load_image(config: &Config) -> Option<Image> {
    let path = config.wallpaper.effective_image()?;
    match decode(path) {
        Ok(img) => {
            log::info!(
                "wallpaper image loaded: {} ({}x{})",
                path.display(),
                img.width,
                img.height
            );
            Some(img)
        }
        Err(e) => {
            log::warn!(
                "wallpaper image {} failed to load; using color: {e:#}",
                path.display()
            );
            None
        }
    }
}

fn decode(path: &Path) -> anyhow::Result<Image> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let decoder = factory.CreateDecoderFromFilename(
            PCWSTR(wide.as_ptr()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        )?;
        let frame = decoder.GetFrame(0)?;
        let converter = factory.CreateFormatConverter()?;
        converter.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppBGR,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let (mut w, mut h) = (0u32, 0u32);
        converter.GetSize(&mut w, &mut h)?;
        anyhow::ensure!(
            (1..=16384).contains(&w) && (1..=16384).contains(&h),
            "unreasonable image size {w}x{h}"
        );
        let stride = w * 4;
        let mut pixels = vec![0u8; (stride * h) as usize];
        converter.CopyPixels(std::ptr::null(), stride, &mut pixels)?;
        Ok(Image {
            width: w as i32,
            height: h as i32,
            pixels,
        })
    }
}

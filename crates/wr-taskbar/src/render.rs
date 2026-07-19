//! Direct2D-on-DirectComposition rendering for the bar.
//!
//! The pipeline (per `docs/ARCHITECTURE.md`): a D3D11 device (hardware, WARP
//! fallback so GPU-less VMs still render) backs a premultiplied-alpha
//! composition swapchain; DirectComposition puts the swapchain on the window;
//! Direct2D draws into the swapchain's back buffer. Translucency and rounded
//! corners come for free from the alpha channel — true acrylic/blur is a
//! later Phase 2 refinement.

use std::collections::HashMap;

use anyhow::Context;
use windows::core::{w, Interface};
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1DeviceContext, ID2D1Factory1, ID2D1Image,
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
    D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_ROUNDED_RECT,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD,
    DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TRIMMING,
    DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISurface, IDXGISwapChain1, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

use wr_core::config::Taskbar;

use crate::layout::{self, BarRect};
use crate::tasks::{Icon, TaskWindow};

/// Everything one paint needs, so `draw` doesn't grow a parameter per slice.
pub struct Frame<'a> {
    pub bar: &'a Taskbar,
    pub clock: &'a str,
    pub dpi: u32,
    pub tasks: &'a [TaskWindow],
    /// Bar-local chip rectangles; `rects[i]` belongs to `tasks[i]` (the tail
    /// of `tasks` may have no rect when the bar overflows).
    pub rects: &'a [BarRect],
    /// Raw handle of the foreground window (highlighted chip), 0 for none.
    pub active: isize,
    /// Decoded icons per window; `Some(None)` remembers "asked, has none".
    pub icons: &'a HashMap<isize, Option<Icon>>,
    /// Index of the chip under the mouse, if any.
    pub hovered: Option<usize>,
}

fn color(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

/// Chip fill strength: resting, hovered, focused, focused-and-hovered.
fn chip_alpha(active: bool, hovered: bool) -> f32 {
    match (active, hovered) {
        (false, false) => 0.08,
        (false, true) => 0.14,
        (true, false) => 0.22,
        (true, true) => 0.26,
    }
}

pub struct Renderer {
    swapchain: IDXGISwapChain1,
    dc: ID2D1DeviceContext,
    /// The back-buffer bitmap currently set as the context target. Dropped
    /// (and the target cleared) around `ResizeBuffers`, which refuses to run
    /// while references to the buffers are alive.
    target: Option<ID2D1Bitmap1>,
    dwrite: IDWriteFactory,
    /// Uploaded window icons, keyed by window handle. Pruned as windows
    /// close; dies (correctly) with the renderer on a device-loss rebuild.
    icon_bitmaps: HashMap<isize, ID2D1Bitmap1>,
    // Held only to keep the composition tree alive for the window's lifetime.
    _dcomp: IDCompositionDevice,
    _dcomp_target: IDCompositionTarget,
    _visual: IDCompositionVisual,
}

impl Renderer {
    pub fn new(hwnd: HWND, width: i32, height: i32) -> anyhow::Result<Self> {
        let d3d = create_d3d_device().context("creating D3D11 device")?;
        let dxgi_device: IDXGIDevice = d3d.cast()?;
        let factory: IDXGIFactory2 = unsafe { dxgi_device.GetAdapter()?.GetParent()? };

        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width.max(1) as u32,
            Height: height.max(1) as u32,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            ..Default::default()
        };
        let swapchain = unsafe { factory.CreateSwapChainForComposition(&d3d, &desc, None) }
            .context("creating composition swapchain")?;

        let d2d_factory: ID2D1Factory1 =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) }
                .context("creating D2D factory")?;
        let dc = unsafe {
            d2d_factory
                .CreateDevice(&dxgi_device)?
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?
        };

        let dcomp: IDCompositionDevice =
            unsafe { DCompositionCreateDevice(&dxgi_device) }.context("creating DComp device")?;
        let dcomp_target = unsafe { dcomp.CreateTargetForHwnd(hwnd, true)? };
        let visual = unsafe { dcomp.CreateVisual()? };
        unsafe {
            visual.SetContent(&swapchain)?;
            dcomp_target.SetRoot(&visual)?;
            dcomp.Commit()?;
        }

        let dwrite: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }
            .context("creating DirectWrite factory")?;

        let mut renderer = Renderer {
            swapchain,
            dc,
            target: None,
            dwrite,
            icon_bitmaps: HashMap::new(),
            _dcomp: dcomp,
            _dcomp_target: dcomp_target,
            _visual: visual,
        };
        renderer.bind_target().context("binding render target")?;
        Ok(renderer)
    }

    /// Wrap the current back buffer as the D2D target.
    fn bind_target(&mut self) -> windows::core::Result<()> {
        let surface: IDXGISurface = unsafe { self.swapchain.GetBuffer(0)? };
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            ..Default::default()
        };
        let bitmap = unsafe {
            self.dc
                .CreateBitmapFromDxgiSurface(&surface, Some(&props))?
        };
        unsafe { self.dc.SetTarget(&bitmap) };
        self.target = Some(bitmap);
        Ok(())
    }

    pub fn resize(&mut self, width: i32, height: i32) -> windows::core::Result<()> {
        unsafe { self.dc.SetTarget(None::<&ID2D1Image>) };
        self.target = None;
        unsafe {
            self.swapchain.ResizeBuffers(
                0,
                width.max(1) as u32,
                height.max(1) as u32,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            )?;
        }
        self.bind_target()
    }

    /// Draw the bar — rounded translucent fill, window-button chips, and the
    /// right-aligned clock — and present. `D2DERR_RECREATE_TARGET` bubbles up
    /// so the caller can rebuild the renderer.
    pub fn draw(&mut self, f: &Frame) -> windows::core::Result<()> {
        self.sync_icon_cache(f);
        let size = unsafe { self.dc.GetSize() };
        let fill = color(
            f.bar.color.r as f32 / 255.0,
            f.bar.color.g as f32 / 255.0,
            f.bar.color.b as f32 / 255.0,
            f.bar.alpha as f32 / 255.0,
        );
        let radius = layout::scale(f.bar.corner_radius, f.dpi) as f32;
        unsafe {
            self.dc.BeginDraw();
            self.dc.Clear(Some(&color(0.0, 0.0, 0.0, 0.0)));
            let brush = self.dc.CreateSolidColorBrush(&fill, None)?;
            self.dc.FillRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: D2D_RECT_F {
                        left: 0.0,
                        top: 0.0,
                        right: size.width,
                        bottom: size.height,
                    },
                    radiusX: radius,
                    radiusY: radius,
                },
                &brush,
            );
            let text_brush = self
                .dc
                .CreateSolidColorBrush(&color(1.0, 1.0, 1.0, 0.92), None)?;

            // Window buttons: translucent white chips over the bar color —
            // brighter when hovered, brighter still for the foreground
            // window — with the window icon (when it has one) before the
            // title. Theming comes later.
            if !f.rects.is_empty() {
                let chip = self
                    .dc
                    .CreateSolidColorBrush(&color(1.0, 1.0, 1.0, 0.08), None)?;
                let label = self.text_format(12.0, f.dpi, DWRITE_TEXT_ALIGNMENT_LEADING)?;
                let chip_radius = layout::scale(6, f.dpi) as f32;
                let pad = layout::scale(10, f.dpi) as f32;
                let icon_size = layout::scale(16, f.dpi) as f32;
                let icon_gap = layout::scale(6, f.dpi) as f32;
                for (i, (task, r)) in f.tasks.iter().zip(f.rects).enumerate() {
                    let rect = D2D_RECT_F {
                        left: r.x as f32,
                        top: r.y as f32,
                        right: (r.x + r.w) as f32,
                        bottom: (r.y + r.h) as f32,
                    };
                    chip.SetColor(&color(
                        1.0,
                        1.0,
                        1.0,
                        chip_alpha(task.hwnd == f.active, f.hovered == Some(i)),
                    ));
                    self.dc.FillRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect,
                            radiusX: chip_radius,
                            radiusY: chip_radius,
                        },
                        &chip,
                    );
                    let mut text_left = rect.left + pad;
                    if let Some(bitmap) = self.icon_bitmaps.get(&task.hwnd) {
                        let top = rect.top + (rect.bottom - rect.top - icon_size) / 2.0;
                        self.dc.DrawBitmap(
                            bitmap,
                            Some(&D2D_RECT_F {
                                left: text_left,
                                top,
                                right: text_left + icon_size,
                                bottom: top + icon_size,
                            }),
                            1.0,
                            D2D1_INTERPOLATION_MODE_LINEAR,
                            None,
                            None,
                        );
                        text_left += icon_size + icon_gap;
                    }
                    let title: Vec<u16> = task.title.encode_utf16().collect();
                    self.dc.DrawText(
                        &title,
                        &label,
                        &D2D_RECT_F {
                            left: text_left,
                            right: rect.right - pad,
                            ..rect
                        },
                        &text_brush,
                        D2D1_DRAW_TEXT_OPTIONS_CLIP,
                        DWRITE_MEASURING_MODE_NATURAL,
                    );
                }
            }

            let clock_format = self.text_format(14.0, f.dpi, DWRITE_TEXT_ALIGNMENT_TRAILING)?;
            let pad = layout::scale(16, f.dpi) as f32;
            let text: Vec<u16> = f.clock.encode_utf16().collect();
            self.dc.DrawText(
                &text,
                &clock_format,
                &D2D_RECT_F {
                    left: pad,
                    top: 0.0,
                    right: size.width - pad,
                    bottom: size.height,
                },
                &text_brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
            self.dc.EndDraw(None, None)?;
            self.swapchain.Present(1, DXGI_PRESENT(0)).ok()?;
        }
        Ok(())
    }

    /// Upload icons for windows that just appeared and drop entries for
    /// windows that are gone. Runs before `BeginDraw`; upload failures mean
    /// a text-only chip, never a draw error.
    fn sync_icon_cache(&mut self, f: &Frame) {
        self.icon_bitmaps
            .retain(|hwnd, _| f.tasks.iter().any(|t| t.hwnd == *hwnd));
        for task in f.tasks {
            if self.icon_bitmaps.contains_key(&task.hwnd) {
                continue;
            }
            let Some(Some(icon)) = f.icons.get(&task.hwnd) else {
                continue;
            };
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                ..Default::default()
            };
            let bitmap = unsafe {
                self.dc.CreateBitmap(
                    D2D_SIZE_U {
                        width: icon.width,
                        height: icon.height,
                    },
                    Some(icon.bgra.as_ptr().cast()),
                    icon.width * 4,
                    &props,
                )
            };
            match bitmap {
                Ok(b) => {
                    self.icon_bitmaps.insert(task.hwnd, b);
                }
                Err(e) => log::debug!("icon upload failed for {:?}: {e}", task.title),
            }
        }
    }

    /// A single-line, vertically centered text format with ellipsis trimming,
    /// sized in 96-DPI points and scaled to the monitor.
    fn text_format(
        &self,
        size_96: f32,
        dpi: u32,
        align: windows::Win32::Graphics::DirectWrite::DWRITE_TEXT_ALIGNMENT,
    ) -> windows::core::Result<IDWriteTextFormat> {
        unsafe {
            let format = self.dwrite.CreateTextFormat(
                w!("Segoe UI"),
                None,
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size_96 * dpi as f32 / 96.0,
                w!("en-us"),
            )?;
            format.SetTextAlignment(align)?;
            format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            let trimming = DWRITE_TRIMMING {
                granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                delimiter: 0,
                delimiterCount: 0,
            };
            let ellipsis = self.dwrite.CreateEllipsisTrimmingSign(&format)?;
            format.SetTrimming(&trimming, &ellipsis)?;
            Ok(format)
        }
    }
}

/// Hardware D3D11 device, falling back to WARP so a GPU-less VM still gets a
/// (software-rendered) taskbar.
fn create_d3d_device() -> anyhow::Result<ID3D11Device> {
    let mut last_err = None;
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let mut device = None;
        match unsafe {
            D3D11CreateDevice(
                None,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        } {
            Ok(()) => {
                if let Some(device) = device {
                    if driver == D3D_DRIVER_TYPE_WARP {
                        log::info!("no hardware D3D11 device; using WARP (software) rendering");
                    }
                    return Ok(device);
                }
            }
            Err(e) => {
                log::warn!("D3D11 device ({}) failed: {e}", driver_name(driver));
                last_err = Some(e);
            }
        }
    }
    Err(anyhow::Error::from(last_err.expect("at least one attempt"))
        .context("no D3D11 device available (hardware or WARP)"))
}

fn driver_name(driver: D3D_DRIVER_TYPE) -> &'static str {
    if driver == D3D_DRIVER_TYPE_WARP {
        "WARP"
    } else {
        "hardware"
    }
}

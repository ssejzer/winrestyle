//! Direct2D-on-DirectComposition rendering for the bar.
//!
//! The pipeline (per `docs/ARCHITECTURE.md`): a D3D11 device (hardware, WARP
//! fallback so GPU-less VMs still render) backs a premultiplied-alpha
//! composition swapchain; DirectComposition puts the swapchain on the window;
//! Direct2D draws into the swapchain's back buffer. Translucency and rounded
//! corners come from the alpha channel; the optional acrylic/mica material
//! behind them is DWM's (`bar::apply_backdrop`), not ours.

use std::collections::HashMap;
use std::path::PathBuf;

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
    DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER,
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

use crate::layout::{self, BarLayout, BarRect, Hit};
use crate::tasks::{Icon, TaskWindow};

/// A tray icon ready to draw: identity key plus decoded pixels (`None` while
/// the owner's icon is missing or undecodable — drawn as an empty cell).
pub struct TrayItem<'a> {
    /// Stable identity for the bitmap cache: (owner hwnd, icon uid).
    pub key: (isize, u32),
    /// Bumped by the bar whenever the owner swaps the icon (`NIM_MODIFY`),
    /// so the cache re-uploads instead of serving the stale image.
    pub rev: u32,
    pub icon: Option<&'a Icon>,
}

/// Everything one paint needs, so `draw` doesn't grow a parameter per slice.
pub struct Frame<'a> {
    pub bar: &'a Taskbar,
    pub clock: &'a str,
    /// Second line under the clock; empty hides it.
    pub date: &'a str,
    pub dpi: u32,
    pub tasks: &'a [TaskWindow],
    /// Where everything sits; `layout.tasks[i]` belongs to `tasks[i]` (the
    /// tail of `tasks` may have no chip when the bar overflows).
    pub layout: &'a BarLayout,
    /// Raw handle of the foreground window (highlighted chip), 0 for none.
    pub active: isize,
    /// Decoded icons per window; `Some(None)` remembers "asked, has none".
    pub icons: &'a HashMap<isize, Option<Icon>>,
    /// Pinned launchers in config order, with their decoded icons.
    pub pinned: &'a [(PathBuf, Option<Icon>)],
    /// Tray icons in cell order (`layout.tray[i]` belongs to `tray[i]`).
    pub tray: &'a [TrayItem<'a>],
    /// The element under the mouse, if any.
    pub hovered: Option<Hit>,
}

fn color(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

fn config_color(c: wr_core::config::Color, a: f32) -> D2D1_COLOR_F {
    color(
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        a,
    )
}

fn rect_f(r: &BarRect) -> D2D_RECT_F {
    D2D_RECT_F {
        left: r.x as f32,
        top: r.y as f32,
        right: (r.x + r.w) as f32,
        bottom: (r.y + r.h) as f32,
    }
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
    /// Uploaded pinned-launcher icons, keyed by path.
    pinned_bitmaps: HashMap<PathBuf, ID2D1Bitmap1>,
    /// Uploaded tray icons, keyed by (owner hwnd, uid), with the revision
    /// they were uploaded at.
    tray_bitmaps: HashMap<(isize, u32), (u32, ID2D1Bitmap1)>,
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
            pinned_bitmaps: HashMap::new(),
            tray_bitmaps: HashMap::new(),
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

    /// Draw the bar — rounded translucent fill, Start button, pinned chips,
    /// window-button chips, overflow chevron, tray icons, and the clock —
    /// and present. `D2DERR_RECREATE_TARGET` bubbles up so the caller can
    /// rebuild the renderer.
    pub fn draw(&mut self, f: &Frame) -> windows::core::Result<()> {
        self.sync_icon_caches(f);
        let size = unsafe { self.dc.GetSize() };
        let fill = config_color(f.bar.color, f.bar.alpha as f32 / 255.0);
        let radius = layout::scale(f.bar.corner_radius, f.dpi) as f32;
        let l = f.layout;
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
                .CreateSolidColorBrush(&config_color(f.bar.text_color, 0.92), None)?;
            // One chip brush, recolored per element (they only differ in
            // alpha).
            let chip = self
                .dc
                .CreateSolidColorBrush(&color(1.0, 1.0, 1.0, 0.08), None)?;
            let chip_radius = layout::scale(6, f.dpi) as f32;
            let fill_chip = |r: &BarRect, active: bool, hovered: bool| {
                chip.SetColor(&color(1.0, 1.0, 1.0, chip_alpha(active, hovered)));
                self.dc.FillRoundedRectangle(
                    &D2D1_ROUNDED_RECT {
                        rect: rect_f(r),
                        radiusX: chip_radius,
                        radiusY: chip_radius,
                    },
                    &chip,
                );
            };

            // Start button: chip + four-pane Windows-style glyph.
            fill_chip(&l.start, false, f.hovered == Some(Hit::Start));
            {
                let rect = rect_f(&l.start);
                let glyph = layout::scale(14, f.dpi) as f32;
                let gap = layout::scale(2, f.dpi).max(1) as f32;
                let pane = ((glyph - gap) / 2.0).max(1.0);
                let gx = rect.left + (l.start.w as f32 - glyph) / 2.0;
                let gy = rect.top + (l.start.h as f32 - glyph) / 2.0;
                for (ix, iy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let left = gx + ix as f32 * (pane + gap);
                    let top = gy + iy as f32 * (pane + gap);
                    self.dc.FillRectangle(
                        &D2D_RECT_F {
                            left,
                            top,
                            right: left + pane,
                            bottom: top + pane,
                        },
                        &text_brush,
                    );
                }
            }

            // Pinned launchers: icon centered in a square chip; a letter chip
            // when the icon couldn't be extracted. The letter format is
            // built at most once per paint, not per chip.
            let pinned_icon_size = layout::scale(20, f.dpi) as f32;
            let mut letter_format: Option<IDWriteTextFormat> = None;
            for (i, ((path, _), r)) in f.pinned.iter().zip(&l.pinned).enumerate() {
                fill_chip(r, false, f.hovered == Some(Hit::Pinned(i)));
                if let Some(bitmap) = self.pinned_bitmaps.get(path) {
                    self.dc.DrawBitmap(
                        bitmap,
                        Some(&centered(r, pinned_icon_size)),
                        1.0,
                        D2D1_INTERPOLATION_MODE_LINEAR,
                        None,
                        None,
                    );
                } else {
                    let letter: String = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().chars().take(1).collect())
                        .unwrap_or_default();
                    let format = match &letter_format {
                        Some(existing) => existing,
                        None => letter_format.insert(self.text_format(
                            13.0,
                            f.dpi,
                            DWRITE_TEXT_ALIGNMENT_CENTER,
                        )?),
                    };
                    self.draw_text(
                        &letter.to_uppercase(),
                        format,
                        &rect_f(r),
                        &text_brush,
                        false,
                    );
                }
            }

            // Window buttons: chip (brighter when hovered, brighter still for
            // the foreground window) + icon + ellipsized title.
            if !l.tasks.is_empty() {
                let label = self.text_format(12.0, f.dpi, DWRITE_TEXT_ALIGNMENT_LEADING)?;
                let pad = layout::scale(10, f.dpi) as f32;
                let icon_size = layout::scale(16, f.dpi) as f32;
                let icon_gap = layout::scale(6, f.dpi) as f32;
                for (i, (task, r)) in f.tasks.iter().zip(&l.tasks).enumerate() {
                    let rect = rect_f(r);
                    fill_chip(r, task.hwnd == f.active, f.hovered == Some(Hit::Task(i)));
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
                    self.draw_text(
                        &task.title,
                        &label,
                        &D2D_RECT_F {
                            left: text_left,
                            right: rect.right - pad,
                            ..rect
                        },
                        &text_brush,
                        true, // clip: titles overflow their chip
                    );
                }
            }

            // Overflow chevron for the windows whose chips didn't fit.
            if let Some(r) = &l.overflow {
                fill_chip(r, false, f.hovered == Some(Hit::Overflow));
                let format = self.text_format(13.0, f.dpi, DWRITE_TEXT_ALIGNMENT_CENTER)?;
                self.draw_text("\u{00bb}", &format, &rect_f(r), &text_brush, false);
            }

            // Tray icons.
            let tray_icon_size = layout::scale(16, f.dpi) as f32;
            for (i, (item, r)) in f.tray.iter().zip(&l.tray).enumerate() {
                if f.hovered == Some(Hit::Tray(i)) {
                    fill_chip(r, false, true);
                }
                if let Some((_, bitmap)) = self.tray_bitmaps.get(&item.key) {
                    self.dc.DrawBitmap(
                        bitmap,
                        Some(&centered(r, tray_icon_size)),
                        1.0,
                        D2D1_INTERPOLATION_MODE_LINEAR,
                        None,
                        None,
                    );
                }
            }

            // Clock, right-aligned; the date slides in underneath when shown.
            let pad = layout::scale(16, f.dpi) as f32;
            let clock_rect = |top: f32, bottom: f32| D2D_RECT_F {
                left: pad,
                top,
                right: size.width - pad,
                bottom,
            };
            let show_date = !f.date.is_empty();
            let time_bottom = if show_date {
                size.height * 0.56
            } else {
                size.height
            };
            let time_format = self.text_format(
                if show_date { 13.0 } else { 14.0 },
                f.dpi,
                DWRITE_TEXT_ALIGNMENT_TRAILING,
            )?;
            self.draw_text(
                f.clock,
                &time_format,
                &clock_rect(0.0, time_bottom),
                &text_brush,
                false,
            );
            if show_date {
                let date_format = self.text_format(9.0, f.dpi, DWRITE_TEXT_ALIGNMENT_TRAILING)?;
                let dim = self
                    .dc
                    .CreateSolidColorBrush(&config_color(f.bar.text_color, 0.72), None)?;
                self.draw_text(
                    f.date,
                    &date_format,
                    &clock_rect(time_bottom, size.height),
                    &dim,
                    false,
                );
            }
            self.dc.EndDraw(None, None)?;
            self.swapchain.Present(1, DXGI_PRESENT(0)).ok()?;
        }
        Ok(())
    }

    /// Draw one string with the bar's standard text options; every text
    /// element routes through here so options can't silently diverge
    /// between elements. `clip` is for text that can overflow its rect
    /// (window titles).
    fn draw_text(
        &self,
        text: &str,
        format: &IDWriteTextFormat,
        rect: &D2D_RECT_F,
        brush: &windows::Win32::Graphics::Direct2D::ID2D1SolidColorBrush,
        clip: bool,
    ) {
        let units: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            self.dc.DrawText(
                &units,
                format,
                rect,
                brush,
                if clip {
                    D2D1_DRAW_TEXT_OPTIONS_CLIP
                } else {
                    D2D1_DRAW_TEXT_OPTIONS_NONE
                },
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    /// Upload icons that just appeared and drop entries that are gone, for
    /// all three icon families. Runs before `BeginDraw`; upload failures
    /// mean an icon-less chip, never a draw error.
    fn sync_icon_caches(&mut self, f: &Frame) {
        self.icon_bitmaps
            .retain(|hwnd, _| f.tasks.iter().any(|t| t.hwnd == *hwnd));
        for task in f.tasks {
            if self.icon_bitmaps.contains_key(&task.hwnd) {
                continue;
            }
            let Some(Some(icon)) = f.icons.get(&task.hwnd) else {
                continue;
            };
            if let Some(b) = self.upload(icon) {
                self.icon_bitmaps.insert(task.hwnd, b);
            }
        }

        self.pinned_bitmaps
            .retain(|path, _| f.pinned.iter().any(|(p, _)| p == path));
        for (path, icon) in f.pinned {
            if self.pinned_bitmaps.contains_key(path) {
                continue;
            }
            if let Some(b) = icon.as_ref().and_then(|i| self.upload(i)) {
                self.pinned_bitmaps.insert(path.clone(), b);
            }
        }

        self.tray_bitmaps
            .retain(|key, _| f.tray.iter().any(|t| t.key == *key));
        for item in f.tray {
            let Some(icon) = item.icon else { continue };
            // NIM_MODIFY swaps the image under the same key; the revision
            // says whether the cached upload is still the current image.
            if self
                .tray_bitmaps
                .get(&item.key)
                .is_some_and(|(rev, _)| *rev == item.rev)
            {
                continue;
            }
            if let Some(b) = self.upload(icon) {
                self.tray_bitmaps.insert(item.key, (item.rev, b));
            }
        }
    }

    /// Upload one decoded icon as a premultiplied D2D bitmap.
    fn upload(&self, icon: &Icon) -> Option<ID2D1Bitmap1> {
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            ..Default::default()
        };
        unsafe {
            self.dc.CreateBitmap(
                D2D_SIZE_U {
                    width: icon.width,
                    height: icon.height,
                },
                Some(icon.bgra.as_ptr().cast()),
                icon.width * 4,
                &props,
            )
        }
        .map_err(|e| log::debug!("icon upload failed: {e}"))
        .ok()
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

/// One visible start-menu row, precomputed by the bar: display name plus
/// highlight state (the renderer never sees list indices or scroll math).
pub struct MenuRow<'a> {
    pub rect: BarRect,
    pub name: &'a str,
    pub selected: bool,
    pub hovered: bool,
}

/// Everything one start-menu paint needs (ADR 0007). The menu derives its
/// theme from the `[taskbar]` config; there is no menu config section yet.
pub struct MenuFrame<'a> {
    pub bar: &'a Taskbar,
    pub dpi: u32,
    /// The type-to-search box.
    pub search: BarRect,
    /// The typed filter; empty draws the hint text instead.
    pub filter: &'a str,
    pub rows: &'a [MenuRow<'a>],
    /// Scrollbar thumb, when the list overflows.
    pub scrollbar: Option<BarRect>,
    /// True when a non-empty filter matched nothing (draws "No matches").
    pub no_matches: bool,
}

impl Renderer {
    /// Draw the start menu and present. Same device-loss contract as
    /// [`Renderer::draw`]: `D2DERR_RECREATE_TARGET` bubbles up.
    pub fn draw_menu(&mut self, f: &MenuFrame) -> windows::core::Result<()> {
        let size = unsafe { self.dc.GetSize() };
        // The menu floats over application windows; keep it readable by
        // flooring the opacity above the bar's (often lower) setting.
        let alpha = f.bar.alpha.max(0xf0) as f32 / 255.0;
        let fill = config_color(f.bar.color, alpha);
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
                .CreateSolidColorBrush(&config_color(f.bar.text_color, 0.92), None)?;
            let dim_brush = self
                .dc
                .CreateSolidColorBrush(&config_color(f.bar.text_color, 0.5), None)?;
            let chip = self
                .dc
                .CreateSolidColorBrush(&color(1.0, 1.0, 1.0, 0.08), None)?;
            let chip_radius = layout::scale(6, f.dpi) as f32;
            let fill_chip = |r: &BarRect, active: bool, hovered: bool| {
                chip.SetColor(&color(1.0, 1.0, 1.0, chip_alpha(active, hovered)));
                self.dc.FillRoundedRectangle(
                    &D2D1_ROUNDED_RECT {
                        rect: rect_f(r),
                        radiusX: chip_radius,
                        radiusY: chip_radius,
                    },
                    &chip,
                );
            };
            let pad = layout::scale(10, f.dpi) as f32;
            let padded = |r: &BarRect| {
                let rect = rect_f(r);
                D2D_RECT_F {
                    left: rect.left + pad,
                    right: rect.right - pad,
                    ..rect
                }
            };

            // Search box: the typed filter, or a dim hint while empty.
            fill_chip(&f.search, false, false);
            let label = self.text_format(12.0, f.dpi, DWRITE_TEXT_ALIGNMENT_LEADING)?;
            if f.filter.is_empty() {
                self.draw_text(
                    "Type to search",
                    &label,
                    &padded(&f.search),
                    &dim_brush,
                    true,
                );
            } else {
                self.draw_text(f.filter, &label, &padded(&f.search), &text_brush, true);
            }

            for row in f.rows {
                fill_chip(&row.rect, row.selected, row.hovered);
                self.draw_text(row.name, &label, &padded(&row.rect), &text_brush, true);
            }
            if f.no_matches {
                // Where the first row would be; there are no rows to collide
                // with when this is set.
                let hint = BarRect {
                    x: f.search.x,
                    y: f.search.y + 2 * f.search.h,
                    w: f.search.w,
                    h: f.search.h,
                };
                self.draw_text("No matches", &label, &padded(&hint), &dim_brush, true);
            }
            if let Some(thumb) = &f.scrollbar {
                let track = self
                    .dc
                    .CreateSolidColorBrush(&config_color(f.bar.text_color, 0.25), None)?;
                let r = rect_f(thumb);
                let radius = thumb.w as f32 / 2.0;
                self.dc.FillRoundedRectangle(
                    &D2D1_ROUNDED_RECT {
                        rect: r,
                        radiusX: radius,
                        radiusY: radius,
                    },
                    &track,
                );
            }
            self.dc.EndDraw(None, None)?;
            self.swapchain.Present(1, DXGI_PRESENT(0)).ok()?;
        }
        Ok(())
    }
}

/// A square of `side` pixels centered inside `r`.
fn centered(r: &BarRect, side: f32) -> D2D_RECT_F {
    let left = r.x as f32 + (r.w as f32 - side) / 2.0;
    let top = r.y as f32 + (r.h as f32 - side) / 2.0;
    D2D_RECT_F {
        left,
        top,
        right: left + side,
        bottom: top + side,
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

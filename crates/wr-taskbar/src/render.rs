//! Direct2D-on-DirectComposition rendering for the bar.
//!
//! The pipeline (per `docs/ARCHITECTURE.md`): a D3D11 device (hardware, WARP
//! fallback so GPU-less VMs still render) backs a premultiplied-alpha
//! composition swapchain; DirectComposition puts the swapchain on the window;
//! Direct2D draws into the swapchain's back buffer. Translucency and rounded
//! corners come for free from the alpha channel — true acrylic/blur is a
//! later Phase 2 refinement.

use anyhow::Context;
use windows::core::{w, Interface};
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1DeviceContext, ID2D1Factory1, ID2D1Image,
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
    D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_ROUNDED_RECT,
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
    DWriteCreateFactory, IDWriteFactory, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_TRAILING,
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

use crate::layout;

pub struct Renderer {
    swapchain: IDXGISwapChain1,
    dc: ID2D1DeviceContext,
    /// The back-buffer bitmap currently set as the context target. Dropped
    /// (and the target cleared) around `ResizeBuffers`, which refuses to run
    /// while references to the buffers are alive.
    target: Option<ID2D1Bitmap1>,
    dwrite: IDWriteFactory,
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

    /// Draw the bar (rounded translucent fill + right-aligned clock) and
    /// present. `D2DERR_RECREATE_TARGET` bubbles up so the caller can rebuild.
    pub fn draw(&mut self, bar: &Taskbar, clock: &str, dpi: u32) -> windows::core::Result<()> {
        let size = unsafe { self.dc.GetSize() };
        let fill = D2D1_COLOR_F {
            r: bar.color.r as f32 / 255.0,
            g: bar.color.g as f32 / 255.0,
            b: bar.color.b as f32 / 255.0,
            a: bar.alpha as f32 / 255.0,
        };
        let radius = layout::scale(bar.corner_radius, dpi) as f32;
        unsafe {
            self.dc.BeginDraw();
            self.dc.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
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

            let format = self.dwrite.CreateTextFormat(
                w!("Segoe UI"),
                None,
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                14.0 * dpi as f32 / 96.0,
                w!("en-us"),
            )?;
            format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING)?;
            format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let text_brush = self.dc.CreateSolidColorBrush(
                &D2D1_COLOR_F {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 0.92,
                },
                None,
            )?;
            let pad = layout::scale(16, dpi) as f32;
            let text: Vec<u16> = clock.encode_utf16().collect();
            self.dc.DrawText(
                &text,
                &format,
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

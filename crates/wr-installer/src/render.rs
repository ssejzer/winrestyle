//! Direct2D rendering for the manager window.
//!
//! Unlike the taskbar (which needs a DirectComposition swapchain for per-pixel
//! translucency), the manager is an ordinary opaque top-level window, so it
//! draws through a plain Direct2D `ID2D1HwndRenderTarget` — the same Direct2D +
//! DirectWrite primitives, far less plumbing (no D3D/DXGI/DComp). The render
//! target is pinned to 96 DPI so one drawing unit is one physical pixel and all
//! DPI scaling stays in `view.rs`, exactly like the taskbar keeps it in
//! `layout.rs`.

use windows::core::w;
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_COLOR_F, D2D_POINT_2F, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Factory, ID2D1HwndRenderTarget, ID2D1SolidColorBrush,
    D2D1_ANTIALIAS_MODE_ALIASED, D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_ROUNDED_RECT,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
    DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD,
    DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT,
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TRIMMING,
    DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP,
};

use crate::view::{Hit, Layout, ViewRect};

/// One row's display data (a component or a startup entry).
pub struct Row<'a> {
    pub name: &'a str,
    /// Second line: a component's summary, or a startup entry's source + detail.
    pub detail: &'a str,
    pub checked: bool,
}

/// Everything one paint needs.
pub struct Frame<'a> {
    pub layout: &'a Layout,
    pub dpi: u32,
    pub scroll: i32,
    pub title: &'a str,
    pub subtitle: &'a str,
    pub components: &'a [Row<'a>],
    pub startup: &'a [Row<'a>],
    pub status: &'a str,
    pub hovered: Option<Hit>,
    /// Buttons are drawn dimmed and unresponsive while an apply is running.
    pub busy: bool,
}

// Dark theme, aligned with the taskbar defaults.
const BG: D2D1_COLOR_F = rgb(0x14, 0x14, 0x20);
const FOOTER_BG: D2D1_COLOR_F = rgb(0x0e, 0x0e, 0x18);
const TEXT: D2D1_COLOR_F = rgb(0xf0, 0xf0, 0xf6);
const DIM: D2D1_COLOR_F = rgb(0xa2, 0xa2, 0xb4);
const ACCENT: D2D1_COLOR_F = rgb(0x6c, 0x7c, 0xf0);
const ACCENT_HOT: D2D1_COLOR_F = rgb(0x82, 0x90, 0xff);
const DANGER: D2D1_COLOR_F = rgb(0xc8, 0x50, 0x50);
const ROW: D2D1_COLOR_F = rgba(0xff, 0xff, 0xff, 0.05);
const ROW_HOT: D2D1_COLOR_F = rgba(0xff, 0xff, 0xff, 0.10);
const BOX_BORDER: D2D1_COLOR_F = rgba(0xff, 0xff, 0xff, 0.35);
const BTN_SECONDARY: D2D1_COLOR_F = rgba(0xff, 0xff, 0xff, 0.09);
const BTN_SECONDARY_HOT: D2D1_COLOR_F = rgba(0xff, 0xff, 0xff, 0.15);

const fn rgb(r: u8, g: u8, b: u8) -> D2D1_COLOR_F {
    rgba(r, g, b, 1.0)
}

const fn rgba(r: u8, g: u8, b: u8, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a,
    }
}

fn rect_f(r: &ViewRect) -> D2D_RECT_F {
    D2D_RECT_F {
        left: r.x as f32,
        top: r.y as f32,
        right: (r.x + r.w) as f32,
        bottom: (r.y + r.h) as f32,
    }
}

fn translate(tx: f32, ty: f32) -> Matrix3x2 {
    Matrix3x2 {
        M11: 1.0,
        M12: 0.0,
        M21: 0.0,
        M22: 1.0,
        M31: tx,
        M32: ty,
    }
}

pub struct Renderer {
    factory: ID2D1Factory,
    target: ID2D1HwndRenderTarget,
    dwrite: IDWriteFactory,
    hwnd: HWND,
}

impl Renderer {
    pub fn new(hwnd: HWND, width: i32, height: i32) -> anyhow::Result<Self> {
        let factory: ID2D1Factory =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)? };
        let target = create_target(&factory, hwnd, width, height)?;
        // 1 drawing unit == 1 physical pixel; all DPI math stays in view.rs.
        unsafe { target.SetDpi(96.0, 96.0) };
        let dwrite: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        Ok(Renderer {
            factory,
            target,
            dwrite,
            hwnd,
        })
    }

    pub fn resize(&self, width: i32, height: i32) -> windows::core::Result<()> {
        unsafe {
            self.target.Resize(&D2D_SIZE_U {
                width: width.max(1) as u32,
                height: height.max(1) as u32,
            })
        }
    }

    /// Recreate the render target after a `D2DERR_RECREATE_TARGET`.
    pub fn recreate(&mut self, width: i32, height: i32) -> anyhow::Result<()> {
        self.target = create_target(&self.factory, self.hwnd, width, height)?;
        unsafe { self.target.SetDpi(96.0, 96.0) };
        Ok(())
    }

    pub fn draw(&self, f: &Frame) -> windows::core::Result<()> {
        let t = &self.target;
        let size = unsafe { t.GetSize() };
        let l = f.layout;
        unsafe {
            t.BeginDraw();
            t.Clear(Some(&BG));

            let text = t.CreateSolidColorBrush(&TEXT, None)?;
            let dim = t.CreateSolidColorBrush(&DIM, None)?;
            let accent = t.CreateSolidColorBrush(&ACCENT, None)?;
            let row_brush = t.CreateSolidColorBrush(&ROW, None)?;
            let border = t.CreateSolidColorBrush(&BOX_BORDER, None)?;

            let title_fmt = self.format(
                20.0,
                f.dpi,
                DWRITE_FONT_WEIGHT_BOLD,
                DWRITE_TEXT_ALIGNMENT_LEADING,
            )?;
            let sub_fmt = self.format(
                11.0,
                f.dpi,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_TEXT_ALIGNMENT_LEADING,
            )?;
            let section_fmt = self.format(
                11.0,
                f.dpi,
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                DWRITE_TEXT_ALIGNMENT_LEADING,
            )?;
            let name_fmt = self.format(
                13.0,
                f.dpi,
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                DWRITE_TEXT_ALIGNMENT_LEADING,
            )?;
            let detail_fmt = self.format(
                10.0,
                f.dpi,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_TEXT_ALIGNMENT_LEADING,
            )?;

            // --- Scrollable content, clipped to the viewport and translated. ---
            let viewport = D2D_RECT_F {
                left: 0.0,
                top: 0.0,
                right: size.width,
                bottom: l.footer.y as f32,
            };
            t.PushAxisAlignedClip(&viewport, D2D1_ANTIALIAS_MODE_ALIASED);
            t.SetTransform(&translate(0.0, -f.scroll as f32));

            draw_text(t, f.title, &title_fmt, &rect_f(&l.title), &text, false);
            draw_text(t, f.subtitle, &sub_fmt, &rect_f(&l.subtitle), &dim, true);
            draw_text(
                t,
                "COMPONENTS",
                &section_fmt,
                &rect_f(&l.components_header),
                &dim,
                false,
            );
            for (i, (row, rect)) in f.components.iter().zip(&l.components).enumerate() {
                self.draw_row(
                    f,
                    row,
                    rect,
                    f.hovered == Some(Hit::Component(i)),
                    &row_brush,
                    &accent,
                    &border,
                    &text,
                    &dim,
                    &name_fmt,
                    &detail_fmt,
                );
            }
            draw_text(
                t,
                "STARTUP PROGRAMS",
                &section_fmt,
                &rect_f(&l.startup_header),
                &dim,
                false,
            );
            if f.startup.is_empty() {
                // A helpful line where the list would be.
                let mut empty = rect_f(&l.startup_header);
                empty.top += l.startup_header.h as f32;
                empty.bottom = empty.top + l.startup_header.h as f32;
                draw_text(
                    t,
                    "No logon startup programs found.",
                    &detail_fmt,
                    &empty,
                    &dim,
                    true,
                );
            }
            for (i, (row, rect)) in f.startup.iter().zip(&l.startup).enumerate() {
                self.draw_row(
                    f,
                    row,
                    rect,
                    f.hovered == Some(Hit::Startup(i)),
                    &row_brush,
                    &accent,
                    &border,
                    &text,
                    &dim,
                    &name_fmt,
                    &detail_fmt,
                );
            }

            t.SetTransform(&translate(0.0, 0.0));
            t.PopAxisAlignedClip();

            // --- Footer (screen space): status + two buttons. ---
            let footer_bg = t.CreateSolidColorBrush(&FOOTER_BG, None)?;
            t.FillRectangle(&rect_f(&l.footer), &footer_bg);
            draw_text(t, f.status, &detail_fmt, &rect_f(&l.status), &dim, true);

            self.draw_button(
                f,
                &l.restore_btn,
                "Undo / Restore",
                ButtonKind::Secondary,
                f.hovered == Some(Hit::Restore),
                &text,
            )?;
            self.draw_button(
                f,
                &l.restyle_btn,
                "Restyle Now",
                ButtonKind::Primary,
                f.hovered == Some(Hit::RestyleNow),
                &text,
            )?;

            let _ = t.EndDraw(None, None);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_row(
        &self,
        f: &Frame,
        row: &Row,
        rect: &ViewRect,
        hot: bool,
        row_brush: &ID2D1SolidColorBrush,
        accent: &ID2D1SolidColorBrush,
        border: &ID2D1SolidColorBrush,
        text: &ID2D1SolidColorBrush,
        dim: &ID2D1SolidColorBrush,
        name_fmt: &IDWriteTextFormat,
        detail_fmt: &IDWriteTextFormat,
    ) {
        let t = &self.target;
        let radius = crate::view::scale(8, f.dpi) as f32;
        unsafe {
            row_brush.SetColor(&if hot { ROW_HOT } else { ROW });
            t.FillRoundedRectangle(&rounded(&rect_f(rect), radius), row_brush);

            // Checkbox.
            let cb = f.layout.checkbox_rect(rect, f.dpi);
            let cb_rect = rect_f(&cb);
            let cb_radius = crate::view::scale(5, f.dpi) as f32;
            if row.checked {
                accent.SetColor(&ACCENT);
                t.FillRoundedRectangle(&rounded(&cb_rect, cb_radius), accent);
                // Checkmark: two strokes.
                let sw = (cb.w as f32 / 9.0).max(1.5);
                let p0 = D2D_POINT_2F {
                    x: cb_rect.left + cb.w as f32 * 0.24,
                    y: cb_rect.top + cb.h as f32 * 0.52,
                };
                let p1 = D2D_POINT_2F {
                    x: cb_rect.left + cb.w as f32 * 0.43,
                    y: cb_rect.top + cb.h as f32 * 0.72,
                };
                let p2 = D2D_POINT_2F {
                    x: cb_rect.left + cb.w as f32 * 0.76,
                    y: cb_rect.top + cb.h as f32 * 0.30,
                };
                t.DrawLine(p0, p1, text, sw, None);
                t.DrawLine(p1, p2, text, sw, None);
            } else {
                t.DrawRoundedRectangle(&rounded(&cb_rect, cb_radius), border, 1.5, None);
            }

            // Two text lines to the right of the checkbox.
            let tx = cb.x + cb.w + crate::view::scale(12, f.dpi);
            let right = rect.x + rect.w - crate::view::scale(12, f.dpi);
            let mid = rect.y + rect.h / 2;
            let name_rect = D2D_RECT_F {
                left: tx as f32,
                top: rect.y as f32,
                right: right as f32,
                bottom: mid as f32,
            };
            let detail_rect = D2D_RECT_F {
                left: tx as f32,
                top: mid as f32,
                right: right as f32,
                bottom: (rect.y + rect.h) as f32,
            };
            draw_text(t, row.name, name_fmt, &name_rect, text, true);
            draw_text(t, row.detail, detail_fmt, &detail_rect, dim, true);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_button(
        &self,
        f: &Frame,
        rect: &ViewRect,
        label: &str,
        kind: ButtonKind,
        hot: bool,
        text: &ID2D1SolidColorBrush,
    ) -> windows::core::Result<()> {
        let t = &self.target;
        let radius = crate::view::scale(8, f.dpi) as f32;
        let color = match (kind, hot, f.busy) {
            (_, _, true) => rgba(0xff, 0xff, 0xff, 0.05),
            (ButtonKind::Primary, false, _) => ACCENT,
            (ButtonKind::Primary, true, _) => ACCENT_HOT,
            (ButtonKind::Secondary, false, _) => BTN_SECONDARY,
            (ButtonKind::Secondary, true, _) => BTN_SECONDARY_HOT,
        };
        unsafe {
            let brush = t.CreateSolidColorBrush(&color, None)?;
            t.FillRoundedRectangle(&rounded(&rect_f(rect), radius), &brush);
            // Restore gets a danger-tinted label so it reads as the exit path.
            let label_brush = if matches!(kind, ButtonKind::Secondary) && !f.busy {
                t.CreateSolidColorBrush(&DANGER, None)?
            } else {
                text.clone()
            };
            // Center the label vertically and horizontally.
            let centered = self.format(
                12.0,
                f.dpi,
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                DWRITE_TEXT_ALIGNMENT_CENTER,
            )?;
            draw_text(t, label, &centered, &rect_f(rect), &label_brush, false);
        }
        Ok(())
    }

    fn format(
        &self,
        size_96: f32,
        dpi: u32,
        weight: DWRITE_FONT_WEIGHT,
        align: DWRITE_TEXT_ALIGNMENT,
    ) -> windows::core::Result<IDWriteTextFormat> {
        unsafe {
            let format = self.dwrite.CreateTextFormat(
                w!("Segoe UI"),
                None,
                weight,
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

#[derive(Clone, Copy)]
enum ButtonKind {
    Primary,
    Secondary,
}

fn rounded(r: &D2D_RECT_F, radius: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT {
        rect: *r,
        radiusX: radius,
        radiusY: radius,
    }
}

fn draw_text(
    target: &ID2D1HwndRenderTarget,
    text: &str,
    format: &IDWriteTextFormat,
    rect: &D2D_RECT_F,
    brush: &ID2D1SolidColorBrush,
    clip: bool,
) {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.is_empty() {
        return;
    }
    unsafe {
        target.DrawText(
            &units,
            format,
            rect,
            brush,
            if clip {
                D2D1_DRAW_TEXT_OPTIONS_CLIP
            } else {
                windows::Win32::Graphics::Direct2D::D2D1_DRAW_TEXT_OPTIONS_NONE
            },
            DWRITE_MEASURING_MODE_NATURAL,
        );
    }
}

fn create_target(
    factory: &ID2D1Factory,
    hwnd: HWND,
    width: i32,
    height: i32,
) -> anyhow::Result<ID2D1HwndRenderTarget> {
    let rt_props = D2D1_RENDER_TARGET_PROPERTIES::default();
    let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
        hwnd,
        pixelSize: D2D_SIZE_U {
            width: width.max(1) as u32,
            height: height.max(1) as u32,
        },
        presentOptions: D2D1_PRESENT_OPTIONS_NONE,
    };
    let target = unsafe { factory.CreateHwndRenderTarget(&rt_props, &hwnd_props)? };
    Ok(target)
}

/// Client-area size of `hwnd` in physical pixels.
pub fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut rect = RECT::default();
    let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rect) };
    (
        (rect.right - rect.left).max(1),
        (rect.bottom - rect.top).max(1),
    )
}

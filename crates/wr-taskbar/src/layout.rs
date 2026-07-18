//! Pure geometry: where the bar sits on screen. No Win32 in here, so it
//! unit-tests on the Linux dev host like the rest of the workspace.

/// A window rectangle in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Scale a 96-DPI config length to physical pixels, rounding to nearest.
pub fn scale(px: u32, dpi: u32) -> i32 {
    ((px as u64 * dpi as u64 + 48) / 96) as i32
}

/// Bottom-of-screen bar: full width minus `margin` on each side, floating
/// `margin` above the bottom edge (`margin = 0` docks it edge to edge).
/// `screen_*` are physical pixels; `height`/`margin` are 96-DPI config values.
pub fn bar_rect(screen_w: i32, screen_h: i32, height: u32, margin: u32, dpi: u32) -> BarRect {
    let h = scale(height.max(1), dpi).max(1);
    let m = scale(margin, dpi);
    BarRect {
        x: m,
        y: (screen_h - h - m).max(0),
        w: (screen_w - 2 * m).max(1),
        h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_at_96_dpi() {
        assert_eq!(scale(48, 96), 48);
        assert_eq!(
            bar_rect(1920, 1080, 48, 8, 96),
            BarRect {
                x: 8,
                y: 1080 - 48 - 8,
                w: 1920 - 16,
                h: 48
            }
        );
    }

    #[test]
    fn scales_at_150_percent() {
        assert_eq!(scale(48, 144), 72);
        assert_eq!(scale(8, 144), 12);
        let r = bar_rect(2880, 1620, 48, 8, 144);
        assert_eq!((r.w, r.h), (2880 - 24, 72));
        assert_eq!((r.x, r.y), (12, 1620 - 72 - 12));
    }

    #[test]
    fn scale_rounds_to_nearest() {
        assert_eq!(scale(10, 120), 13); // 12.5 rounds up
        assert_eq!(scale(10, 110), 11); // 11.458 rounds down
    }

    #[test]
    fn zero_margin_docks_edge_to_edge() {
        let r = bar_rect(1920, 1080, 48, 0, 96);
        assert_eq!((r.x, r.y, r.w, r.h), (0, 1080 - 48, 1920, 48));
    }

    #[test]
    fn degenerate_inputs_never_produce_an_empty_window() {
        // A zero-height config or a tiny screen must still yield a valid rect;
        // window creation with zero extents would fail.
        let r = bar_rect(100, 50, 0, 0, 96);
        assert!(r.w >= 1 && r.h >= 1 && r.y >= 0);
        let r = bar_rect(10, 10, 500, 500, 96);
        assert!(r.w >= 1 && r.h >= 1 && r.y >= 0);
    }
}

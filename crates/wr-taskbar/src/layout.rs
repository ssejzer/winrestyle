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

impl BarRect {
    /// Whether the point (same coordinate space as the rect) lies inside.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
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

/// 96-DPI width reserved for the clock at the bar's right edge. Shared by
/// the button layout (stops before it) and the renderer (draws into it).
pub const CLOCK_RESERVE: u32 = 88;

const BUTTON_MAX_W: u32 = 180;
const BUTTON_MIN_W: u32 = 48;
const BUTTON_GAP: u32 = 6;
const BUTTON_VPAD: u32 = 6;
const EDGE_PAD: u32 = 10;

/// Bar-local square for the Start button: the leftmost element, inset by the
/// same edge/vertical padding as the window chips. Window buttons lay out
/// after it.
pub fn start_rect(bar_h: i32, dpi: u32) -> BarRect {
    let vpad = scale(BUTTON_VPAD, dpi);
    let side = (bar_h - 2 * vpad).max(1);
    BarRect {
        x: scale(EDGE_PAD, dpi),
        y: vpad,
        w: side,
        h: side,
    }
}

/// Bar-local rectangles for `count` window buttons: left-aligned after the
/// Start button, stopping before the clock reserve. Buttons shrink from
/// `BUTTON_MAX_W` down to `BUTTON_MIN_W` as the bar fills; windows that
/// still don't fit get no button (dropped from the end — grouping/overflow
/// UI is a later slice).
pub fn button_rects(bar_w: i32, bar_h: i32, count: usize, dpi: u32) -> Vec<BarRect> {
    if count == 0 {
        return Vec::new();
    }
    let gap = scale(BUTTON_GAP, dpi);
    let vpad = scale(BUTTON_VPAD, dpi);
    let start = start_rect(bar_h, dpi);
    let pad = start.x + start.w + gap;
    let avail = bar_w - pad - scale(CLOCK_RESERVE, dpi);
    let max_w = scale(BUTTON_MAX_W, dpi).max(1);
    let min_w = scale(BUTTON_MIN_W, dpi).max(1);

    let wanted = count as i32;
    let width_if_all_fit = (avail - (wanted - 1) * gap) / wanted;
    let (w, n) = if width_if_all_fit >= min_w {
        (width_if_all_fit.min(max_w), wanted)
    } else {
        // Shrunk to the floor and still too many: drop the tail.
        ((min_w), ((avail + gap) / (min_w + gap)).clamp(0, wanted))
    };
    let h = (bar_h - 2 * vpad).max(1);
    (0..n)
        .map(|i| BarRect {
            x: pad + i * (w + gap),
            y: vpad,
            w,
            h,
        })
        .collect()
}

/// Index of the rect containing the point, if any.
pub fn hit_test(rects: &[BarRect], x: i32, y: i32) -> Option<usize> {
    rects.iter().position(|r| r.contains(x, y))
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
    fn start_button_is_a_padded_square() {
        let s = start_rect(48, 96);
        assert_eq!((s.x, s.y, s.w, s.h), (10, 6, 36, 36));
        // Scales with DPI, stays square.
        let s = start_rect(72, 144);
        assert_eq!((s.x, s.y, s.w, s.h), (15, 9, 54, 54));
        // A degenerate bar height never yields an empty rect.
        let s = start_rect(1, 96);
        assert!(s.w >= 1 && s.h >= 1);
    }

    #[test]
    fn few_buttons_take_max_width() {
        let rects = button_rects(1920, 48, 3, 96);
        assert_eq!(rects.len(), 3);
        assert!(rects.iter().all(|r| r.w == 180));
        // Buttons begin after the Start button (10 + 36 + 6 gap).
        let start = start_rect(48, 96);
        assert_eq!(rects[0].x, start.x + start.w + 6);
        assert_eq!(rects[1].x, rects[0].x + 180 + 6);
        // Vertical padding leaves a slimmer chip inside the bar.
        assert!(rects.iter().all(|r| r.y == 6 && r.h == 48 - 12));
    }

    #[test]
    fn buttons_shrink_when_the_bar_fills() {
        let rects = button_rects(1920, 48, 12, 96);
        assert_eq!(rects.len(), 12);
        assert!(rects[0].w < 180 && rects[0].w >= 48);
        // The last button still ends before the clock reserve.
        let last = rects.last().unwrap();
        assert!(last.x + last.w <= 1920 - 88);
    }

    #[test]
    fn overflow_drops_buttons_at_min_width() {
        let rects = button_rects(800, 48, 50, 96);
        assert!(!rects.is_empty());
        assert!(rects.len() < 50);
        assert!(rects.iter().all(|r| r.w == 48));
        let last = rects.last().unwrap();
        assert!(last.x + last.w <= 800 - 88);
    }

    #[test]
    fn no_buttons_no_rects_and_tiny_bars_survive() {
        assert!(button_rects(1920, 48, 0, 96).is_empty());
        // A bar narrower than the clock reserve fits nothing but must not
        // panic or return negative geometry.
        for r in button_rects(60, 48, 4, 96) {
            assert!(r.w >= 1 && r.h >= 1);
        }
    }

    #[test]
    fn hit_test_finds_the_right_button() {
        let rects = button_rects(1920, 48, 3, 96);
        assert_eq!(hit_test(&rects, rects[1].x + 1, rects[1].y + 1), Some(1));
        // The gap between buttons belongs to nobody.
        assert_eq!(hit_test(&rects, rects[1].x - 1, 20), None);
        // The bar background outside any chip is a miss.
        assert_eq!(hit_test(&rects, 1919, 20), None);
        assert_eq!(hit_test(&[], 10, 10), None);
    }

    #[test]
    fn start_button_and_window_buttons_never_overlap() {
        let start = start_rect(48, 96);
        assert!(start.contains(start.x, start.y));
        assert!(start.contains(start.x + start.w - 1, start.y + start.h - 1));
        assert!(!start.contains(start.x + start.w, start.y));
        // The first window chip starts past the Start button, so a point
        // inside the Start square never also hits a chip.
        let rects = button_rects(1920, 48, 3, 96);
        assert_eq!(hit_test(&rects, start.x + start.w - 1, 20), None);
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

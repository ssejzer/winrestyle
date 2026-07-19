//! Pure geometry: where the bar sits on screen and where every element sits
//! on the bar. No Win32 in here, so it unit-tests on the Linux dev host like
//! the rest of the workspace.

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

/// Bottom-of-monitor bar: full width minus `margin` on each side, floating
/// `margin` above the bottom edge (`margin = 0` docks it edge to edge).
/// `mon_*` describe the monitor in physical pixels (multi-monitor rects have
/// offsets, possibly negative); `height`/`margin` are 96-DPI config values.
pub fn bar_rect(
    mon_x: i32,
    mon_y: i32,
    mon_w: i32,
    mon_h: i32,
    height: u32,
    margin: u32,
    dpi: u32,
) -> BarRect {
    let h = scale(height.max(1), dpi).max(1);
    let m = scale(margin, dpi);
    BarRect {
        x: mon_x + m,
        y: mon_y + (mon_h - h - m).max(0),
        w: (mon_w - 2 * m).max(1),
        h,
    }
}

/// 96-DPI width reserved for the clock (and date) at the bar's right edge.
pub const CLOCK_RESERVE: u32 = 88;

const BUTTON_MAX_W: u32 = 180;
const BUTTON_MIN_W: u32 = 48;
const BUTTON_GAP: u32 = 6;
const BUTTON_VPAD: u32 = 6;
const EDGE_PAD: u32 = 10;
/// Width of the overflow chevron chip shown when window buttons are dropped.
const OVERFLOW_W: u32 = 28;
/// Width of one tray-icon cell and the gap between cells.
const TRAY_CELL_W: u32 = 26;
const TRAY_GAP: u32 = 2;

/// Everything on the bar, in bar-local physical pixels. Built by
/// [`bar_layout`]; hit-tested by [`BarLayout::hit_test`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BarLayout {
    /// Start button square (always present, leftmost).
    pub start: BarRect,
    /// One square per pinned app (the tail is dropped if the bar is absurdly
    /// narrow).
    pub pinned: Vec<BarRect>,
    /// Chip for window `i`; may be shorter than the window count — dropped
    /// windows are reachable through the overflow chip instead.
    pub tasks: Vec<BarRect>,
    /// Chevron chip after the last window chip, present only when windows
    /// were dropped (and there is room to show it).
    pub overflow: Option<BarRect>,
    /// Tray-icon cells, right-aligned against the clock reserve.
    pub tray: Vec<BarRect>,
}

/// What a point on the bar belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Start,
    Pinned(usize),
    Task(usize),
    Overflow,
    Tray(usize),
}

impl BarLayout {
    pub fn hit_test(&self, x: i32, y: i32) -> Option<Hit> {
        if self.start.contains(x, y) {
            return Some(Hit::Start);
        }
        if let Some(i) = self.pinned.iter().position(|r| r.contains(x, y)) {
            return Some(Hit::Pinned(i));
        }
        if let Some(i) = self.tasks.iter().position(|r| r.contains(x, y)) {
            return Some(Hit::Task(i));
        }
        if self.overflow.is_some_and(|r| r.contains(x, y)) {
            return Some(Hit::Overflow);
        }
        if let Some(i) = self.tray.iter().position(|r| r.contains(x, y)) {
            return Some(Hit::Tray(i));
        }
        None
    }
}

/// Lay out the whole bar. Left to right: Start square, pinned squares,
/// window chips (shrinking from `BUTTON_MAX_W` to `BUTTON_MIN_W` as the bar
/// fills, then dropping the tail behind an overflow chevron), tray icons,
/// clock reserve. Degenerate sizes never panic and never produce
/// zero-extent rects.
pub fn bar_layout(
    bar_w: i32,
    bar_h: i32,
    dpi: u32,
    pinned_count: usize,
    task_count: usize,
    tray_count: usize,
) -> BarLayout {
    let pad = scale(EDGE_PAD, dpi);
    let gap = scale(BUTTON_GAP, dpi);
    let vpad = scale(BUTTON_VPAD, dpi);
    let side = (bar_h - 2 * vpad).max(1);
    let chip_h = side;
    let right_edge = bar_w - scale(CLOCK_RESERVE, dpi);

    let start = BarRect {
        x: pad,
        y: vpad,
        w: side,
        h: side,
    };

    // Pinned squares, dropping any that would cross into the clock reserve.
    let mut pinned = Vec::with_capacity(pinned_count);
    let mut x = start.x + start.w + gap;
    for _ in 0..pinned_count {
        if x + side > right_edge {
            break;
        }
        pinned.push(BarRect {
            x,
            y: vpad,
            w: side,
            h: side,
        });
        x += side + gap;
    }
    let tasks_left = x;

    // Tray cells, right-aligned against the clock. Keeps the first icons and
    // drops the tail when the bar cannot hold them all (same drop policy as
    // window buttons). When there are windows, the tray must leave room for
    // at least one minimum-width chip plus the overflow chevron — otherwise
    // a tray-heavy session would evict every window button AND the chevron,
    // making open windows unreachable from the bar.
    let cell = scale(TRAY_CELL_W, dpi).max(1);
    let tray_gap = scale(TRAY_GAP, dpi);
    let task_reserve = if task_count > 0 {
        scale(BUTTON_MIN_W, dpi).max(1) + gap + scale(OVERFLOW_W, dpi).max(1) + gap
    } else {
        0
    };
    let tray_min_left = tasks_left + task_reserve;
    let max_cells = if right_edge <= tray_min_left {
        0
    } else {
        (((right_edge - tray_min_left + tray_gap) / (cell + tray_gap)) as usize).min(tray_count)
    };
    let tray_total = if max_cells == 0 {
        0
    } else {
        max_cells as i32 * cell + (max_cells as i32 - 1) * tray_gap
    };
    let tray_left = right_edge - tray_total;
    let tray = (0..max_cells)
        .map(|i| BarRect {
            x: tray_left + i as i32 * (cell + tray_gap),
            y: vpad,
            w: cell,
            h: chip_h,
        })
        .collect();

    // Window chips fill what is left between the pinned area and the tray.
    let tasks_right = if max_cells == 0 {
        right_edge
    } else {
        tray_left - gap
    };
    let (tasks, overflow) = task_rects(tasks_left, tasks_right, vpad, chip_h, task_count, dpi, gap);

    BarLayout {
        start,
        pinned,
        tasks,
        overflow,
        tray,
    }
}

/// Chips for `count` windows between `left` and `right`, plus the overflow
/// chevron when not all fit.
#[allow(clippy::too_many_arguments)]
fn task_rects(
    left: i32,
    right: i32,
    y: i32,
    h: i32,
    count: usize,
    dpi: u32,
    gap: i32,
) -> (Vec<BarRect>, Option<BarRect>) {
    if count == 0 {
        return (Vec::new(), None);
    }
    let avail = right - left;
    let max_w = scale(BUTTON_MAX_W, dpi).max(1);
    let min_w = scale(BUTTON_MIN_W, dpi).max(1);
    let ovf_w = scale(OVERFLOW_W, dpi).max(1);

    let wanted = count as i32;
    let width_if_all_fit = (avail - (wanted - 1) * gap) / wanted;
    let (w, n) = if width_if_all_fit >= min_w {
        (width_if_all_fit.min(max_w), wanted)
    } else {
        // Shrunk to the floor and still too many: drop the tail behind the
        // overflow chevron, which needs its own slot.
        let avail = avail - ovf_w - gap;
        (min_w, ((avail + gap) / (min_w + gap)).clamp(0, wanted))
    };
    let rects: Vec<BarRect> = (0..n)
        .map(|i| BarRect {
            x: left + i * (w + gap),
            y,
            w,
            h,
        })
        .collect();
    let overflow = if n < wanted {
        let x = left + n * (w + gap);
        (x + ovf_w <= right).then_some(BarRect { x, y, w: ovf_w, h })
    } else {
        None
    };
    (rects, overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The common case: one 1920×48 bar at 96 DPI, no pinned/tray.
    fn plain(tasks: usize) -> BarLayout {
        bar_layout(1920, 48, 96, 0, tasks, 0)
    }

    #[test]
    fn identity_at_96_dpi() {
        assert_eq!(scale(48, 96), 48);
        assert_eq!(
            bar_rect(0, 0, 1920, 1080, 48, 8, 96),
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
        let r = bar_rect(0, 0, 2880, 1620, 48, 8, 144);
        assert_eq!((r.w, r.h), (2880 - 24, 72));
        assert_eq!((r.x, r.y), (12, 1620 - 72 - 12));
    }

    #[test]
    fn scale_rounds_to_nearest() {
        assert_eq!(scale(10, 120), 13); // 12.5 rounds up
        assert_eq!(scale(10, 110), 11); // 11.458 rounds down
    }

    #[test]
    fn bar_rect_respects_monitor_offsets() {
        // A second monitor to the left of the primary has negative x.
        let r = bar_rect(-1920, 0, 1920, 1080, 48, 8, 96);
        assert_eq!((r.x, r.y), (-1920 + 8, 1080 - 48 - 8));
        // One below-and-right of origin keeps its offsets too.
        let r = bar_rect(1920, 200, 2560, 1440, 48, 0, 96);
        assert_eq!((r.x, r.y, r.w), (1920, 200 + 1440 - 48, 2560));
    }

    #[test]
    fn zero_margin_docks_edge_to_edge() {
        let r = bar_rect(0, 0, 1920, 1080, 48, 0, 96);
        assert_eq!((r.x, r.y, r.w, r.h), (0, 1080 - 48, 1920, 48));
    }

    #[test]
    fn start_button_is_a_padded_square() {
        let l = plain(0);
        assert_eq!(
            (l.start.x, l.start.y, l.start.w, l.start.h),
            (10, 6, 36, 36)
        );
        // Scales with DPI, stays square.
        let l = bar_layout(2880, 72, 144, 0, 0, 0);
        assert_eq!(
            (l.start.x, l.start.y, l.start.w, l.start.h),
            (15, 9, 54, 54)
        );
    }

    #[test]
    fn few_buttons_take_max_width() {
        let l = plain(3);
        assert_eq!(l.tasks.len(), 3);
        assert!(l.tasks.iter().all(|r| r.w == 180));
        // Buttons begin after the Start button (10 + 36 + 6 gap).
        assert_eq!(l.tasks[0].x, l.start.x + l.start.w + 6);
        assert_eq!(l.tasks[1].x, l.tasks[0].x + 180 + 6);
        // Vertical padding leaves a slimmer chip inside the bar.
        assert!(l.tasks.iter().all(|r| r.y == 6 && r.h == 48 - 12));
        assert_eq!(l.overflow, None);
    }

    #[test]
    fn pinned_squares_sit_between_start_and_buttons() {
        let l = bar_layout(1920, 48, 96, 2, 3, 0);
        assert_eq!(l.pinned.len(), 2);
        assert!(l.pinned.iter().all(|r| r.w == 36 && r.h == 36));
        assert_eq!(l.pinned[0].x, l.start.x + l.start.w + 6);
        assert_eq!(l.pinned[1].x, l.pinned[0].x + 36 + 6);
        assert_eq!(l.tasks[0].x, l.pinned[1].x + 36 + 6);
    }

    #[test]
    fn buttons_shrink_when_the_bar_fills() {
        let l = plain(12);
        assert_eq!(l.tasks.len(), 12);
        assert!(l.tasks[0].w < 180 && l.tasks[0].w >= 48);
        // The last button still ends before the clock reserve.
        let last = l.tasks.last().unwrap();
        assert!(last.x + last.w <= 1920 - 88);
    }

    #[test]
    fn overflow_chip_appears_when_buttons_are_dropped() {
        let l = bar_layout(800, 48, 96, 0, 50, 0);
        assert!(!l.tasks.is_empty());
        assert!(l.tasks.len() < 50);
        assert!(l.tasks.iter().all(|r| r.w == 48));
        let ovf = l.overflow.expect("dropped windows need the chevron");
        let last = l.tasks.last().unwrap();
        assert_eq!(ovf.x, last.x + last.w + 6);
        assert!(ovf.x + ovf.w <= 800 - 88);
        // Without drops there is no chevron.
        assert_eq!(plain(3).overflow, None);
    }

    #[test]
    fn tray_is_right_aligned_and_buttons_stop_before_it() {
        let l = bar_layout(1920, 48, 96, 0, 12, 3);
        assert_eq!(l.tray.len(), 3);
        let last_tray = l.tray.last().unwrap();
        assert_eq!(last_tray.x + last_tray.w, 1920 - 88);
        assert_eq!(l.tray[1].x, l.tray[0].x + 26 + 2);
        // Window chips end before the tray begins.
        let last_task = l.tasks.last().unwrap();
        assert!(last_task.x + last_task.w <= l.tray[0].x - 6);
    }

    #[test]
    fn tray_never_starves_the_window_buttons() {
        // A tray-heavy narrow bar: with open windows, at least one chip (or
        // its overflow chevron) must survive so windows stay reachable.
        let l = bar_layout(800, 48, 96, 0, 5, 40);
        assert!(!l.tasks.is_empty(), "at least one window chip must fit");
        assert!(l.overflow.is_some(), "dropped windows need the chevron");
        assert!(l.tray.len() < 40, "tray drops its tail instead");
        let ovf = l.overflow.unwrap();
        assert!(ovf.x + ovf.w <= l.tray.first().unwrap().x);
        // With no windows the tray may take the whole middle.
        let l = bar_layout(800, 48, 96, 0, 0, 40);
        assert!(l.tray.len() > 20);
    }

    #[test]
    fn degenerate_bars_survive_everything() {
        // A bar narrower than the clock reserve fits nothing but must not
        // panic or return zero-extent geometry.
        for w in [1, 10, 60, 120] {
            let l = bar_layout(w, 48, 96, 3, 40, 5);
            for r in std::iter::once(&l.start)
                .chain(&l.pinned)
                .chain(&l.tasks)
                .chain(&l.tray)
                .chain(l.overflow.as_ref())
            {
                assert!(r.w >= 1 && r.h >= 1, "bar_w={w} rect={r:?}");
            }
        }
        // Degenerate height too.
        let l = bar_layout(1920, 1, 96, 1, 1, 1);
        assert!(l.start.h >= 1);
        // Zero of everything is still a valid layout.
        let l = plain(0);
        assert!(l.tasks.is_empty() && l.overflow.is_none() && l.tray.is_empty());
    }

    #[test]
    fn hit_test_resolves_every_element() {
        let l = bar_layout(1920, 48, 96, 2, 3, 2);
        assert_eq!(l.hit_test(l.start.x + 1, l.start.y + 1), Some(Hit::Start));
        assert_eq!(
            l.hit_test(l.pinned[1].x + 1, l.pinned[1].y + 1),
            Some(Hit::Pinned(1))
        );
        assert_eq!(
            l.hit_test(l.tasks[2].x + 1, l.tasks[2].y + 1),
            Some(Hit::Task(2))
        );
        assert_eq!(
            l.hit_test(l.tray[0].x + 1, l.tray[0].y + 1),
            Some(Hit::Tray(0))
        );
        // The gap between chips belongs to nobody, as does the background.
        assert_eq!(l.hit_test(l.tasks[1].x - 1, 20), None);
        assert_eq!(l.hit_test(l.tasks[2].x + l.tasks[2].w + 1, 20), None);
        // The chevron, when present.
        let l = bar_layout(800, 48, 96, 0, 50, 0);
        let ovf = l.overflow.unwrap();
        assert_eq!(l.hit_test(ovf.x + 1, ovf.y + 1), Some(Hit::Overflow));
    }
}

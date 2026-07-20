//! Pure start-menu logic (ADR 0007): where the menu window sits, where the
//! search box and each visible row sit inside it, scrollbar math, hit-testing,
//! and the keyboard/wheel state machine. No Win32, so it unit-tests on the
//! Linux dev host like `layout.rs`.

use crate::layout::{scale, BarRect};

/// Menu window size at 96 DPI; height shrinks to fit above the bar.
pub const MENU_W: u32 = 360;
pub const MENU_MAX_H: u32 = 520;
/// Gap between the menu's bottom edge and the bar's top edge.
const GAP_ABOVE_BAR: u32 = 8;
/// Inner padding on every side.
const PAD: u32 = 10;
const SEARCH_H: u32 = 34;
const ROW_H: u32 = 32;
const ROW_GAP: u32 = 2;
const SCROLLBAR_W: u32 = 4;
/// Minimum scrollbar-thumb height, so it stays grabbable-looking on huge lists.
const THUMB_MIN_H: u32 = 24;

/// Where the menu window sits on screen: above the bar, left-aligned with it,
/// clamped to the monitor. `mon` is (x, y, w, h) in virtual-screen pixels;
/// `bar` is the bar window's screen rect.
pub fn menu_rect(mon: (i32, i32, i32, i32), bar: BarRect, dpi: u32) -> BarRect {
    let (mon_x, mon_y, mon_w, _mon_h) = mon;
    let gap = scale(GAP_ABOVE_BAR, dpi);
    let w = scale(MENU_W, dpi).min(mon_w.max(1));
    let bottom = bar.y - gap;
    let h = scale(MENU_MAX_H, dpi).min((bottom - mon_y).max(1));
    let x = bar.x.min(mon_x + mon_w - w).max(mon_x);
    BarRect {
        x,
        y: bottom - h,
        w,
        h,
    }
}

/// Everything inside the menu window, in menu-local physical pixels, for the
/// current filtered-list length and scroll offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuLayout {
    /// The type-to-search box at the top.
    pub search: BarRect,
    /// Visible rows: (index into the *filtered* list, rect), top to bottom.
    pub rows: Vec<(usize, BarRect)>,
    /// Scrollbar thumb at the right edge, present only when rows overflow.
    pub scrollbar: Option<BarRect>,
    /// How many rows fit — the page size for wheel and ensure-visible math.
    pub fit: usize,
}

impl MenuLayout {
    /// The filtered-list index of the row under a menu-local point.
    pub fn hit_row(&self, x: i32, y: i32) -> Option<usize> {
        self.rows
            .iter()
            .find(|(_, r)| r.contains(x, y))
            .map(|(i, _)| *i)
    }
}

/// Lay the menu out. Degenerate sizes produce empty-but-valid layouts, never
/// zero-extent rects.
pub fn menu_layout(w: i32, h: i32, dpi: u32, count: usize, scroll: usize) -> MenuLayout {
    let pad = scale(PAD, dpi);
    let search = BarRect {
        x: pad,
        y: pad,
        w: (w - 2 * pad).max(1),
        h: scale(SEARCH_H, dpi).max(1),
    };
    let row_h = scale(ROW_H, dpi).max(1);
    let row_gap = scale(ROW_GAP, dpi);
    let top = search.y + search.h + pad;
    let bottom = h - pad;
    let fit = if bottom <= top {
        0
    } else {
        ((bottom - top + row_gap) / (row_h + row_gap)) as usize
    };
    let scroll = scroll.min(count.saturating_sub(fit));
    let overflow = count > fit && fit > 0;
    let scrollbar_w = scale(SCROLLBAR_W, dpi).max(1);
    let row_w = if overflow {
        (search.w - scrollbar_w - row_gap).max(1)
    } else {
        search.w
    };
    let rows: Vec<(usize, BarRect)> = (0..fit.min(count - scroll.min(count)))
        .map(|i| {
            (
                scroll + i,
                BarRect {
                    x: pad,
                    y: top + i as i32 * (row_h + row_gap),
                    w: row_w,
                    h: row_h,
                },
            )
        })
        .collect();
    let scrollbar = overflow.then(|| {
        let track_h = (bottom - top).max(1);
        let thumb_h = ((track_h as i64 * fit as i64 / count as i64) as i32)
            .max(scale(THUMB_MIN_H, dpi))
            .min(track_h);
        let span = count - fit; // > 0, since overflow
        let thumb_y = top + ((track_h - thumb_h) as i64 * scroll as i64 / span as i64) as i32;
        BarRect {
            x: pad + row_w + row_gap,
            y: thumb_y,
            w: scrollbar_w,
            h: thumb_h,
        }
    });
    MenuLayout {
        search,
        rows,
        scrollbar,
        fit,
    }
}

/// The interaction state: the typed filter and where the selection/viewport
/// are within the *filtered* list. All transitions clamp; the caller
/// recomputes the filtered list after `on_char` reports a change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MenuState {
    pub filter: String,
    /// Index into the filtered list. Meaningless (0) when it is empty.
    pub selected: usize,
    /// First visible filtered index.
    pub scroll: usize,
}

impl MenuState {
    /// A typed character: printable characters extend the filter, backspace
    /// (`\u{8}`) shortens it, everything else is ignored. Returns whether the
    /// filter changed (selection and viewport reset to the top).
    pub fn on_char(&mut self, c: char) -> bool {
        let changed = if c == '\u{8}' {
            self.filter.pop().is_some()
        } else if !c.is_control() {
            self.filter.push(c);
            true
        } else {
            false
        };
        if changed {
            self.selected = 0;
            self.scroll = 0;
        }
        changed
    }

    /// Scroll the viewport by `rows` (wheel), clamped; the selection stays.
    pub fn on_wheel(&mut self, rows: i32, count: usize, fit: usize) {
        let max = count.saturating_sub(fit) as i64;
        self.scroll = (self.scroll as i64 + rows as i64).clamp(0, max) as usize;
    }

    /// Move the selection one step in `delta`'s direction to the next
    /// *selectable* row (skipping group headers, marked `false` in
    /// `selectable`), then scroll just enough to keep it visible in a
    /// `fit`-row viewport. A no-op if there is nothing selectable that way.
    pub fn move_selection_skipping(&mut self, delta: i32, selectable: &[bool], fit: usize) {
        let count = selectable.len();
        if count == 0 {
            self.selected = 0;
            self.scroll = 0;
            return;
        }
        let step: i64 = if delta >= 0 { 1 } else { -1 };
        let mut idx = (self.selected.min(count - 1)) as i64;
        loop {
            let next = idx + step;
            if next < 0 || next >= count as i64 {
                break; // clamped at an end; stay put
            }
            idx = next;
            if selectable[idx as usize] {
                self.selected = idx as usize;
                break;
            }
        }
        // Keep the selection (and the header just above it, when there is one)
        // visible.
        if self.selected < self.scroll {
            // Show the preceding header too if the selection tops the viewport.
            let top = if self.selected > 0 && !selectable[self.selected - 1] {
                self.selected - 1
            } else {
                self.selected
            };
            self.scroll = top;
        } else if fit > 0 && self.selected >= self.scroll + fit {
            self.scroll = self.selected + 1 - fit;
        }
    }
}

/// The first selectable index (skipping leading headers), or 0 if none.
pub fn first_selectable(selectable: &[bool]) -> usize {
    selectable.iter().position(|&s| s).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MON: (i32, i32, i32, i32) = (0, 0, 1920, 1080);
    const BAR: BarRect = BarRect {
        x: 8,
        y: 1024,
        w: 1904,
        h: 48,
    };

    #[test]
    fn menu_sits_above_the_bar_left_aligned() {
        let r = menu_rect(MON, BAR, 96);
        assert_eq!(r.x, BAR.x);
        assert_eq!(r.y + r.h, BAR.y - 8);
        assert_eq!((r.w, r.h), (360, 520));
    }

    #[test]
    fn menu_scales_with_dpi() {
        let r = menu_rect(
            (0, 0, 2880, 1620),
            BarRect {
                x: 12,
                y: 1536,
                w: 2856,
                h: 72,
            },
            144,
        );
        assert_eq!((r.w, r.h), (540, 780));
        assert_eq!(r.y + r.h, 1536 - 12);
    }

    #[test]
    fn menu_clamps_to_short_and_narrow_monitors() {
        // Not enough room above the bar: height shrinks.
        let r = menu_rect(
            (0, 0, 1920, 300),
            BarRect {
                x: 8,
                y: 244,
                w: 1904,
                h: 48,
            },
            96,
        );
        assert_eq!(r.y, 0);
        assert_eq!(r.h, 244 - 8);
        // A monitor narrower than the menu: width shrinks, x stays on-screen.
        let r = menu_rect(
            (0, 0, 200, 1080),
            BarRect {
                x: 8,
                y: 1024,
                w: 184,
                h: 48,
            },
            96,
        );
        assert_eq!((r.x, r.w), (0, 200));
        // A bar near the monitor's right edge on a wide monitor: x pulls left.
        let r = menu_rect(
            MON,
            BarRect {
                x: 1800,
                y: 1024,
                w: 100,
                h: 48,
            },
            96,
        );
        assert_eq!(r.x + r.w, 1920);
        // Negative-offset monitors keep their coordinates.
        let r = menu_rect(
            (-1920, 0, 1920, 1080),
            BarRect {
                x: -1912,
                y: 1024,
                w: 1904,
                h: 48,
            },
            96,
        );
        assert_eq!(r.x, -1912);
    }

    #[test]
    fn layout_places_search_then_rows() {
        let l = menu_layout(360, 520, 96, 5, 0);
        assert_eq!(
            l.search,
            BarRect {
                x: 10,
                y: 10,
                w: 340,
                h: 34
            }
        );
        assert_eq!(l.rows.len(), 5);
        assert_eq!(l.rows[0].1.y, 10 + 34 + 10);
        assert_eq!(l.rows[1].1.y, l.rows[0].1.y + 32 + 2);
        // No overflow: full-width rows, no scrollbar.
        assert!(l.rows.iter().all(|(_, r)| r.w == 340));
        assert_eq!(l.scrollbar, None);
        // fit is the page size even when fewer rows exist.
        assert!(l.fit > 5);
    }

    #[test]
    fn layout_windows_the_list_by_scroll() {
        let l = menu_layout(360, 520, 96, 100, 7);
        let indices: Vec<usize> = l.rows.iter().map(|(i, _)| *i).collect();
        assert_eq!(indices[0], 7);
        assert_eq!(indices.len(), l.fit);
        assert!(indices.windows(2).all(|w| w[1] == w[0] + 1));
        // Scroll past the end clamps.
        let l = menu_layout(360, 520, 96, 100, 10_000);
        assert_eq!(l.rows.last().unwrap().0, 99);
    }

    #[test]
    fn scrollbar_appears_only_on_overflow_and_tracks_scroll() {
        assert_eq!(menu_layout(360, 520, 96, 3, 0).scrollbar, None);
        let top = menu_layout(360, 520, 96, 100, 0);
        let thumb_top = top.scrollbar.expect("overflow needs a thumb");
        let bottom = menu_layout(360, 520, 96, 100, 100);
        let thumb_bottom = bottom.scrollbar.unwrap();
        assert!(thumb_bottom.y > thumb_top.y);
        // Thumb ends exactly at the track bottom when fully scrolled.
        assert_eq!(thumb_bottom.y + thumb_bottom.h, 520 - 10);
        // Rows shrink to make room for the thumb.
        assert!(top.rows[0].1.w < top.search.w);
        assert_eq!(thumb_top.x + thumb_top.w, top.search.x + top.search.w);
    }

    #[test]
    fn degenerate_sizes_never_panic_or_produce_zero_rects() {
        for (w, h) in [(1, 1), (20, 40), (360, 30), (360, 60)] {
            let l = menu_layout(w, h, 96, 50, 3);
            assert!(l.search.w >= 1 && l.search.h >= 1);
            for (_, r) in &l.rows {
                assert!(r.w >= 1 && r.h >= 1, "w={w} h={h}");
            }
        }
        let l = menu_layout(360, 520, 96, 0, 0);
        assert!(l.rows.is_empty());
        assert_eq!(l.scrollbar, None);
    }

    #[test]
    fn hit_row_resolves_visible_rows_only() {
        let l = menu_layout(360, 520, 96, 100, 10);
        let (idx, r) = l.rows[2];
        assert_eq!(l.hit_row(r.x + 1, r.y + 1), Some(idx));
        assert_eq!(idx, 12);
        // The search box and gaps belong to no row.
        assert_eq!(l.hit_row(l.search.x + 1, l.search.y + 1), None);
        assert_eq!(l.hit_row(r.x + 1, r.y - 1), None);
    }

    #[test]
    fn typing_edits_the_filter_and_resets_the_viewport() {
        let mut s = MenuState {
            filter: String::new(),
            selected: 9,
            scroll: 5,
        };
        assert!(s.on_char('a'));
        assert_eq!((s.filter.as_str(), s.selected, s.scroll), ("a", 0, 0));
        assert!(s.on_char('B'));
        assert_eq!(s.filter, "aB");
        // Backspace shortens; on an empty filter it is a no-op.
        assert!(s.on_char('\u{8}'));
        assert_eq!(s.filter, "a");
        assert!(s.on_char('\u{8}'));
        assert!(!s.on_char('\u{8}'));
        // Control characters (Enter, Esc arrive as WM_CHAR too) are ignored.
        assert!(!s.on_char('\r'));
        assert!(!s.on_char('\u{1b}'));
        assert!(s.filter.is_empty());
    }

    #[test]
    fn selection_all_selectable_clamps_and_keeps_itself_visible() {
        // With no headers, stepping behaves like a plain clamped move.
        let sel = [true; 10];
        let mut s = MenuState::default();
        s.move_selection_skipping(-1, &sel, 3);
        assert_eq!((s.selected, s.scroll), (0, 0));
        s.move_selection_skipping(1, &sel, 3);
        s.move_selection_skipping(1, &sel, 3);
        s.move_selection_skipping(1, &sel, 3); // selected 3, past a 3-row view
        assert_eq!((s.selected, s.scroll), (3, 1));
        // An empty list pins everything to zero.
        s.selected = 4;
        s.move_selection_skipping(1, &[], 3);
        assert_eq!((s.selected, s.scroll), (0, 0));
    }

    #[test]
    fn selection_skips_group_headers() {
        // Layout: [Header, A, B, Header, C] — indices 0 and 3 are headers.
        let sel = [false, true, true, false, true];
        let mut s = MenuState {
            selected: 1,
            ..Default::default()
        };
        // Down from A(1) → B(2).
        s.move_selection_skipping(1, &sel, 5);
        assert_eq!(s.selected, 2);
        // Down from B(2) hops over the header at 3 → C(4).
        s.move_selection_skipping(1, &sel, 5);
        assert_eq!(s.selected, 4);
        // Down at the end is a no-op (no selectable past 4).
        s.move_selection_skipping(1, &sel, 5);
        assert_eq!(s.selected, 4);
        // Up from C(4) hops back over header 3 → B(2).
        s.move_selection_skipping(-1, &sel, 5);
        assert_eq!(s.selected, 2);
        // first_selectable skips the leading header.
        assert_eq!(first_selectable(&sel), 1);
        assert_eq!(first_selectable(&[false, false]), 0);
        assert_eq!(first_selectable(&[]), 0);
    }

    #[test]
    fn selection_scroll_reveals_the_group_header_above_it() {
        // A header at 3 with items 4,5,6; a 2-row viewport.
        let sel = [true, true, true, false, true, true, true];
        let mut s = MenuState {
            selected: 3, // pretend just below; step down to 4
            scroll: 5,
            ..Default::default()
        };
        s.move_selection_skipping(1, &sel, 2); // → 4, above current scroll(5)
        assert_eq!(s.selected, 4);
        // Scroll pulls back to show the header (3) sitting above item 4.
        assert_eq!(s.scroll, 3);
    }

    #[test]
    fn wheel_scrolls_without_moving_the_selection() {
        let mut s = MenuState {
            selected: 2,
            ..Default::default()
        };
        s.on_wheel(3, 10, 3);
        assert_eq!((s.selected, s.scroll), (2, 3));
        s.on_wheel(100, 10, 3);
        assert_eq!(s.scroll, 7);
        s.on_wheel(-100, 10, 3);
        assert_eq!(s.scroll, 0);
        // No overflow: wheel is a no-op.
        s.on_wheel(5, 3, 10);
        assert_eq!(s.scroll, 0);
    }
}

//! Pure geometry and hit-testing for the manager window: where every row,
//! header, and button sits, and what a click lands on. No Win32 and no D2D in
//! here, so it unit-tests on the Linux dev host exactly like the taskbar's
//! `layout.rs` — which matters doubly because the window itself can only be
//! *seen* in the VM (manual T3).
//!
//! The window is a fixed **footer** (the two action buttons + a status line)
//! over a **scrollable content region** (title, the component checklist, and
//! the startup-programs list). Content rects are in content space (y grows from
//! 0 at the top of the scrollable area); footer/button rects are in screen
//! space. [`Layout::hit_test`] takes the current scroll offset and reconciles
//! the two.

/// A rectangle in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl ViewRect {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// Scale a 96-DPI length to physical pixels, rounding to nearest (same rule as
/// the taskbar).
pub fn scale(px: i32, dpi: u32) -> i32 {
    ((px as i64 * dpi as i64 + 48) / 96) as i32
}

// 96-DPI layout constants.
const MARGIN: i32 = 18;
const TITLE_H: i32 = 34;
const SUBTITLE_H: i32 = 22;
const SECTION_H: i32 = 30;
const ROW_H: i32 = 52;
const ROW_GAP: i32 = 6;
const SECTION_GAP: i32 = 14;
const FOOTER_H: i32 = 66;
const BTN_W: i32 = 132;
const BTN_H: i32 = 38;
const BTN_GAP: i32 = 10;
const CHECKBOX: i32 = 22;
const CHECKBOX_PAD: i32 = 14;

/// What a point in the window belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// Toggle component row `i`.
    Component(usize),
    /// Toggle startup-entry row `i`.
    Startup(usize),
    /// The "Restyle Now" button.
    RestyleNow,
    /// The "Undo / Restore Windows" button.
    Restore,
}

/// Everything laid out for one paint of the manager window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Title line (content space).
    pub title: ViewRect,
    /// Subtitle / current-state line (content space).
    pub subtitle: ViewRect,
    /// "Components" section header (content space).
    pub components_header: ViewRect,
    /// One row per component (content space).
    pub components: Vec<ViewRect>,
    /// "Startup programs" section header (content space).
    pub startup_header: ViewRect,
    /// One row per startup entry (content space).
    pub startup: Vec<ViewRect>,
    /// Total height of the scrollable content.
    pub content_height: i32,
    /// Height of the scroll viewport (client height minus the footer).
    pub viewport_h: i32,
    /// The footer bar (screen space).
    pub footer: ViewRect,
    /// Status-text area in the footer (screen space).
    pub status: ViewRect,
    /// "Undo / Restore" button (screen space).
    pub restore_btn: ViewRect,
    /// "Restyle Now" button (screen space).
    pub restyle_btn: ViewRect,
}

/// Lay out the whole window. `client_w`/`client_h` are the client area in
/// physical pixels; `dpi` scales the 96-DPI constants. Degenerate sizes never
/// panic and never produce zero-extent rects.
pub fn layout(
    client_w: i32,
    client_h: i32,
    dpi: u32,
    component_count: usize,
    startup_count: usize,
) -> Layout {
    let m = scale(MARGIN, dpi);
    let row_h = scale(ROW_H, dpi).max(1);
    let row_gap = scale(ROW_GAP, dpi);
    let section_gap = scale(SECTION_GAP, dpi);
    let footer_h = scale(FOOTER_H, dpi).max(1);
    let content_w = (client_w - 2 * m).max(1);

    // Content region (scrollable), laid out top-down in content space.
    let mut y = m;
    let title = ViewRect {
        x: m,
        y,
        w: content_w,
        h: scale(TITLE_H, dpi).max(1),
    };
    y += title.h;
    let subtitle = ViewRect {
        x: m,
        y,
        w: content_w,
        h: scale(SUBTITLE_H, dpi).max(1),
    };
    y += subtitle.h + section_gap;

    let components_header = ViewRect {
        x: m,
        y,
        w: content_w,
        h: scale(SECTION_H, dpi).max(1),
    };
    y += components_header.h;
    let components = stack_rows(m, &mut y, content_w, row_h, row_gap, component_count);

    y += section_gap;
    let startup_header = ViewRect {
        x: m,
        y,
        w: content_w,
        h: scale(SECTION_H, dpi).max(1),
    };
    y += startup_header.h;
    let startup = stack_rows(m, &mut y, content_w, row_h, row_gap, startup_count);

    let content_height = y + m;

    // Footer (fixed, screen space).
    let footer_y = (client_h - footer_h).max(0);
    let viewport_h = footer_y;
    let footer = ViewRect {
        x: 0,
        y: footer_y,
        w: client_w.max(1),
        h: footer_h,
    };
    let btn_w = scale(BTN_W, dpi).max(1);
    let btn_h = scale(BTN_H, dpi).max(1);
    let btn_gap = scale(BTN_GAP, dpi);
    let btn_y = footer_y + (footer_h - btn_h) / 2;
    let restyle_btn = ViewRect {
        x: (client_w - m - btn_w).max(0),
        y: btn_y,
        w: btn_w,
        h: btn_h,
    };
    let restore_btn = ViewRect {
        x: (restyle_btn.x - btn_gap - btn_w).max(0),
        y: btn_y,
        w: btn_w,
        h: btn_h,
    };
    let status = ViewRect {
        x: m,
        y: footer_y,
        w: (restore_btn.x - 2 * m).max(1),
        h: footer_h,
    };

    Layout {
        title,
        subtitle,
        components_header,
        components,
        startup_header,
        startup,
        content_height,
        viewport_h,
        footer,
        status,
        restore_btn,
        restyle_btn,
    }
}

/// Stack `count` full-width rows starting at `*y`, advancing `*y` past them.
fn stack_rows(x: i32, y: &mut i32, w: i32, h: i32, gap: i32, count: usize) -> Vec<ViewRect> {
    let mut rows = Vec::with_capacity(count);
    for _ in 0..count {
        rows.push(ViewRect { x, y: *y, w, h });
        *y += h + gap;
    }
    rows
}

impl Layout {
    /// The largest valid scroll offset (0 when the content fits).
    pub fn max_scroll(&self) -> i32 {
        (self.content_height - self.viewport_h).max(0)
    }

    /// Clamp a proposed scroll offset to `[0, max_scroll]`.
    pub fn clamp_scroll(&self, scroll: i32) -> i32 {
        scroll.clamp(0, self.max_scroll())
    }

    /// The checkbox square inside a row (for drawing and centered hit feedback).
    pub fn checkbox_rect(&self, row: &ViewRect, dpi: u32) -> ViewRect {
        let side = scale(CHECKBOX, dpi).max(1);
        ViewRect {
            x: row.x + scale(CHECKBOX_PAD, dpi),
            y: row.y + (row.h - side) / 2,
            w: side,
            h: side,
        }
    }

    /// Where a click at screen point `(x, y)` lands, given the current
    /// `scroll`. Footer buttons are tested in screen space; content rows are
    /// tested after translating into content space, and only within the
    /// viewport (never under the footer).
    pub fn hit_test(&self, x: i32, y: i32, scroll: i32) -> Option<Hit> {
        if y >= self.footer.y {
            if self.restyle_btn.contains(x, y) {
                return Some(Hit::RestyleNow);
            }
            if self.restore_btn.contains(x, y) {
                return Some(Hit::Restore);
            }
            return None;
        }
        // Inside the scroll viewport: map to content space.
        let cy = y + scroll;
        if let Some(i) = self.components.iter().position(|r| r.contains(x, cy)) {
            return Some(Hit::Component(i));
        }
        if let Some(i) = self.startup.iter().position(|r| r.contains(x, cy)) {
            return Some(Hit::Startup(i));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide() -> Layout {
        layout(560, 640, 96, 3, 4)
    }

    #[test]
    fn identity_at_96_dpi() {
        assert_eq!(scale(52, 96), 52);
        assert_eq!(scale(18, 96), 18);
    }

    #[test]
    fn scales_at_150_percent() {
        assert_eq!(scale(52, 144), 78);
        let l = layout(840, 960, 144, 3, 0);
        // Rows are the scaled row height.
        assert!(l.components.iter().all(|r| r.h == scale(ROW_H, 144)));
    }

    #[test]
    fn one_row_per_component_and_startup_entry() {
        let l = layout(560, 640, 96, 3, 5);
        assert_eq!(l.components.len(), 3);
        assert_eq!(l.startup.len(), 5);
    }

    #[test]
    fn rows_stack_without_overlap_and_in_order() {
        let l = wide();
        for pair in l.components.windows(2) {
            assert!(pair[1].y >= pair[0].y + pair[0].h, "rows overlap");
        }
        // Components sit above the startup header, which sits above its rows.
        assert!(l.components.last().unwrap().y < l.startup_header.y);
        assert!(l.startup_header.y < l.startup[0].y);
    }

    #[test]
    fn content_height_grows_with_entries() {
        let few = layout(560, 640, 96, 1, 1).content_height;
        let many = layout(560, 640, 96, 3, 20).content_height;
        assert!(many > few);
    }

    #[test]
    fn footer_is_pinned_to_the_bottom_with_two_buttons() {
        let l = wide();
        assert_eq!(l.footer.y + l.footer.h, 640);
        assert_eq!(l.footer.w, 560);
        // Restyle is right of restore, both inside the footer band.
        assert!(l.restore_btn.x + l.restore_btn.w <= l.restyle_btn.x);
        for b in [l.restore_btn, l.restyle_btn] {
            assert!(b.y >= l.footer.y && b.y + b.h <= l.footer.y + l.footer.h);
        }
        // Status text does not run under the restore button.
        assert!(l.status.x + l.status.w <= l.restore_btn.x);
    }

    #[test]
    fn scroll_clamps_to_content() {
        // Tall content, short window → scrollable.
        let l = layout(560, 300, 96, 3, 40);
        assert!(l.max_scroll() > 0);
        assert_eq!(l.clamp_scroll(-100), 0);
        assert_eq!(l.clamp_scroll(l.max_scroll() + 999), l.max_scroll());
        // Everything fits → no scroll.
        let l = layout(560, 2000, 96, 1, 1);
        assert_eq!(l.max_scroll(), 0);
    }

    #[test]
    fn hit_test_resolves_buttons_and_rows() {
        let l = wide();
        // Buttons (screen space, in the footer).
        assert_eq!(
            l.hit_test(l.restyle_btn.x + 1, l.restyle_btn.y + 1, 0),
            Some(Hit::RestyleNow)
        );
        assert_eq!(
            l.hit_test(l.restore_btn.x + 1, l.restore_btn.y + 1, 0),
            Some(Hit::Restore)
        );
        // First component row, no scroll (content y == screen y).
        let r = l.components[0];
        assert_eq!(l.hit_test(r.x + 5, r.y + 5, 0), Some(Hit::Component(0)));
        // A startup row.
        let s = l.startup[2];
        assert_eq!(l.hit_test(s.x + 5, s.y + 5, 0), Some(Hit::Startup(2)));
        // Empty space between rows hits nothing.
        assert_eq!(l.hit_test(r.x + 5, r.y + r.h + 1, 0), None);
    }

    #[test]
    fn scrolled_content_hit_tests_in_content_space() {
        // Short window so the list scrolls; scroll a row up under the top edge.
        let l = layout(560, 260, 96, 3, 20);
        let target = l.startup[10];
        let scroll = l.clamp_scroll(target.y - 40); // bring it near the top
                                                    // Its screen y is content y minus scroll.
        let screen_y = target.y - scroll + 5;
        assert!(screen_y < l.footer.y, "row should be in the viewport");
        assert_eq!(
            l.hit_test(target.x + 5, screen_y, scroll),
            Some(Hit::Startup(10))
        );
    }

    #[test]
    fn clicks_under_the_footer_never_hit_a_row() {
        // Even if a content row would map under the footer, footer clicks only
        // ever resolve to buttons (or nothing), never a toggle.
        let l = layout(560, 260, 96, 3, 20);
        let in_footer_empty = l.footer.y + 2;
        // A point in the footer away from the buttons.
        assert_eq!(l.hit_test(l.status.x + 1, in_footer_empty, 999), None);
    }

    #[test]
    fn checkbox_sits_inside_its_row() {
        let l = wide();
        let row = l.components[1];
        let cb = l.checkbox_rect(&row, 96);
        assert!(cb.x >= row.x && cb.x + cb.w <= row.x + row.w);
        assert!(cb.y >= row.y && cb.y + cb.h <= row.y + row.h);
    }

    #[test]
    fn degenerate_sizes_survive() {
        for (w, h) in [(1, 1), (10, 10), (200, 60), (560, 1)] {
            let l = layout(w, h, 96, 3, 8);
            for r in [l.title, l.footer, l.restyle_btn, l.restore_btn, l.status] {
                assert!(r.w >= 1 && r.h >= 1, "w={w} h={h} rect={r:?}");
            }
            assert!(l.max_scroll() >= 0);
        }
    }
}

//! Pure bookkeeping for the window-button list. No Win32 in here, so the
//! ordering and click-policy logic unit-tests on the Linux dev host.

/// One taskbar-worthy top-level window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskWindow {
    /// The HWND as an integer, so this module stays platform-free.
    pub hwnd: isize,
    pub title: String,
}

/// Merge a fresh enumeration into the current button list, keeping button
/// order stable the way a taskbar should: surviving windows keep their
/// position (with refreshed titles), closed windows drop out, new windows
/// append at the end in enumeration order.
///
/// Returns `(merged, added, removed)`; compare `merged` with the old list to
/// know whether anything (including a title) changed.
pub fn merge(
    old: &[TaskWindow],
    fresh: &[TaskWindow],
) -> (Vec<TaskWindow>, Vec<TaskWindow>, Vec<TaskWindow>) {
    let mut merged = Vec::with_capacity(fresh.len());
    let mut removed = Vec::new();
    for w in old {
        match fresh.iter().find(|f| f.hwnd == w.hwnd) {
            Some(f) => merged.push(f.clone()),
            None => removed.push(w.clone()),
        }
    }
    let mut added = Vec::new();
    for f in fresh {
        if !old.iter().any(|w| w.hwnd == f.hwnd) {
            added.push(f.clone());
            merged.push(f.clone());
        }
    }
    (merged, added, removed)
}

/// A decoded window icon: premultiplied BGRA, top-down rows — exactly what
/// the renderer uploads. Produced from raw GDI bits by [`build_icon`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Icon {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// Turn raw icon bits into a renderable [`Icon`].
///
/// `color` is the icon's 32bpp BGRA color plane. Icons drawn with a real
/// alpha channel are used as-is; legacy icons carry alpha in a separate
/// AND-mask instead (`mask`, also expanded to 32bpp: white = transparent,
/// black = opaque). If the color plane has no alpha at all and no mask is
/// available, the icon is treated as fully opaque. Output is premultiplied.
pub fn build_icon(
    width: u32,
    height: u32,
    mut color: Vec<u8>,
    mask: Option<&[u8]>,
) -> Option<Icon> {
    let len = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    if len == 0 || color.len() != len {
        return None;
    }
    let has_alpha = color.chunks_exact(4).any(|px| px[3] != 0);
    if !has_alpha {
        match mask {
            Some(mask) if mask.len() == len => {
                for (px, m) in color.chunks_exact_mut(4).zip(mask.chunks_exact(4)) {
                    px[3] = if m[0] == 0 && m[1] == 0 && m[2] == 0 {
                        255
                    } else {
                        0
                    };
                }
            }
            _ => {
                for px in color.chunks_exact_mut(4) {
                    px[3] = 255;
                }
            }
        }
    }
    for px in color.chunks_exact_mut(4) {
        let a = px[3] as u32;
        px[0] = ((px[0] as u32 * a) / 255) as u8;
        px[1] = ((px[1] as u32 * a) / 255) as u8;
        px[2] = ((px[2] as u32 * a) / 255) as u8;
    }
    Some(Icon {
        width,
        height,
        bgra: color,
    })
}

/// What a click on a window's button does. The rules every taskbar follows:
/// clicking the focused window minimizes it, clicking a minimized one brings
/// it back, clicking any other window focuses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickAction {
    Minimize,
    RestoreAndFocus,
    Focus,
}

pub fn click_action(is_foreground: bool, is_minimized: bool) -> ClickAction {
    if is_minimized {
        ClickAction::RestoreAndFocus
    } else if is_foreground {
        ClickAction::Minimize
    } else {
        ClickAction::Focus
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(hwnd: isize, title: &str) -> TaskWindow {
        TaskWindow {
            hwnd,
            title: title.to_string(),
        }
    }

    #[test]
    fn merge_keeps_stable_order_and_appends_new() {
        let old = [w(1, "a"), w(2, "b")];
        // Fresh enumeration in a different (z) order, plus a newcomer.
        let fresh = [w(3, "c"), w(2, "b"), w(1, "a")];
        let (merged, added, removed) = merge(&old, &fresh);
        assert_eq!(merged, vec![w(1, "a"), w(2, "b"), w(3, "c")]);
        assert_eq!(added, vec![w(3, "c")]);
        assert!(removed.is_empty());
    }

    #[test]
    fn merge_drops_closed_windows() {
        let old = [w(1, "a"), w(2, "b"), w(3, "c")];
        let fresh = [w(3, "c"), w(1, "a")];
        let (merged, added, removed) = merge(&old, &fresh);
        assert_eq!(merged, vec![w(1, "a"), w(3, "c")]);
        assert!(added.is_empty());
        assert_eq!(removed, vec![w(2, "b")]);
    }

    #[test]
    fn merge_refreshes_titles_in_place() {
        let old = [w(1, "Document1 - Editor")];
        let fresh = [w(1, "Document2 - Editor")];
        let (merged, added, removed) = merge(&old, &fresh);
        assert_eq!(merged, vec![w(1, "Document2 - Editor")]);
        assert!(added.is_empty() && removed.is_empty());
        assert_ne!(merged, old.to_vec()); // callers detect the repaint this way
    }

    #[test]
    fn merge_of_identical_lists_is_equal() {
        let old = [w(1, "a"), w(2, "b")];
        let (merged, added, removed) = merge(&old, &old.to_vec());
        assert_eq!(merged, old.to_vec());
        assert!(added.is_empty() && removed.is_empty());
    }

    #[test]
    fn icon_with_alpha_is_premultiplied() {
        // One pixel: b=200 g=100 r=50, a=128 → channels scaled by 128/255.
        let icon = build_icon(1, 1, vec![200, 100, 50, 128], None).unwrap();
        assert_eq!(icon.bgra, vec![100, 50, 25, 128]);
    }

    #[test]
    fn legacy_icon_gets_alpha_from_the_mask() {
        // Two pixels, no alpha; mask says: first opaque (black), second
        // transparent (white).
        let color = vec![10, 20, 30, 0, 40, 50, 60, 0];
        let mask = vec![0, 0, 0, 0, 255, 255, 255, 0];
        let icon = build_icon(2, 1, color, Some(&mask)).unwrap();
        assert_eq!(&icon.bgra[..4], &[10, 20, 30, 255]);
        assert_eq!(&icon.bgra[4..], &[0, 0, 0, 0]); // premultiplied to nothing
    }

    #[test]
    fn no_alpha_no_mask_means_opaque() {
        let icon = build_icon(1, 1, vec![10, 20, 30, 0], None).unwrap();
        assert_eq!(icon.bgra, vec![10, 20, 30, 255]);
    }

    #[test]
    fn bad_icon_dimensions_are_rejected() {
        assert!(build_icon(2, 2, vec![0; 4], None).is_none()); // too short
        assert!(build_icon(0, 0, Vec::new(), None).is_none());
        // A mask of the wrong size is ignored, not fatal.
        let icon = build_icon(1, 1, vec![1, 2, 3, 0], Some(&[0, 0])).unwrap();
        assert_eq!(icon.bgra[3], 255);
    }

    #[test]
    fn click_rules() {
        assert_eq!(click_action(false, false), ClickAction::Focus);
        assert_eq!(click_action(true, false), ClickAction::Minimize);
        assert_eq!(click_action(false, true), ClickAction::RestoreAndFocus);
        // A minimized-but-foreground oddity restores rather than re-minimizes.
        assert_eq!(click_action(true, true), ClickAction::RestoreAndFocus);
    }
}

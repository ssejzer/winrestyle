//! The `Shell_NotifyIcon` host protocol, data side: parse the `WM_COPYDATA`
//! payloads applications send to the `Shell_TrayWnd` window, and keep the
//! resulting icon registry. Pure bytes-and-state (no Win32), so the fiddly
//! wire format unit-tests on the Linux dev host; the host window itself and
//! everything HICON lives in `bar`/`winlist`.
//!
//! The wire format is ancient and undocumented but frozen by three decades
//! of compatibility: `COPYDATASTRUCT.dwData == 1` carries a 8-byte header
//! (magic, message) followed by a `NOTIFYICONDATAW` laid out for 32-bit —
//! every handle field is 4 bytes on the wire regardless of bitness, and
//! handles are sign-extended back to pointer width (they fit in 32 bits by
//! kernel contract).

/// `Shell_NotifyIcon` messages (`dwMessage` on the wire).
pub const NIM_ADD: u32 = 0;
pub const NIM_MODIFY: u32 = 1;
pub const NIM_DELETE: u32 = 2;
pub const NIM_SETFOCUS: u32 = 3;
pub const NIM_SETVERSION: u32 = 4;

/// `NOTIFYICONDATA.uFlags` bits — which fields of the struct are valid.
pub const NIF_MESSAGE: u32 = 0x1;
pub const NIF_ICON: u32 = 0x2;
pub const NIF_TIP: u32 = 0x4;
pub const NIF_STATE: u32 = 0x8;

/// `dwState` bit: the icon is registered but not shown.
pub const NIS_HIDDEN: u32 = 0x1;

/// `NOTIFYICON_VERSION_4` (Vista+): coordinates-in-wparam callback encoding.
pub const VERSION_4: u32 = 4;

/// `NIN_SELECT` (`WM_USER + 0`): the v4 "icon activated" callback event the
/// host sends alongside the raw left-button-up.
pub const NIN_SELECT: u32 = 0x0400;
/// `WM_CONTEXTMENU`: v4 owners key their menus off this callback event, sent
/// by the host alongside the raw right-button-up.
pub const WM_CONTEXTMENU: u32 = 0x007B;

/// One `NOTIFYICONDATA` as parsed off the wire. Only the fields flagged
/// valid by `flags` are meaningful (mirrors the protocol).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconData {
    /// The owner window (sign-extended from the 32-bit wire field).
    pub owner: isize,
    pub uid: u32,
    pub flags: u32,
    pub callback: u32,
    /// Raw `HICON` value (sign-extended); 0 when the sender passed none.
    pub hicon: isize,
    /// Tooltip text (`NIF_TIP`).
    pub tip: String,
    /// `dwState` / `dwStateMask` (`NIF_STATE`).
    pub state: u32,
    pub state_mask: u32,
    /// The `uTimeout`/`uVersion` union, meaningful for `NIM_SETVERSION`.
    pub version: u32,
}

/// A parsed tray command: which `NIM_*` operation, on which icon data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayCommand {
    pub op: u32,
    pub data: IconData,
}

fn u32_at(buf: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(buf.get(off..off + 4)?.try_into().ok()?))
}

/// Sign-extend a 32-bit wire handle to pointer width.
fn handle_at(buf: &[u8], off: usize) -> Option<isize> {
    Some(u32_at(buf, off)? as i32 as isize)
}

/// UTF-16 string at `off`, at most `wchars` units, NUL-terminated.
fn string_at(buf: &[u8], off: usize, wchars: usize) -> String {
    let mut units = Vec::with_capacity(wchars);
    for i in 0..wchars {
        let Some(b) = buf.get(off + i * 2..off + i * 2 + 2) else {
            break;
        };
        let u = u16::from_le_bytes([b[0], b[1]]);
        if u == 0 {
            break;
        }
        units.push(u);
    }
    String::from_utf16_lossy(&units)
}

/// Parse one `WM_COPYDATA` tray payload (`dwData == 1`). `None` for buffers
/// too short to carry the fixed header — never a panic, whatever a hostile
/// or ancient sender ships.
pub fn parse(buf: &[u8]) -> Option<TrayCommand> {
    // 8-byte prefix: dwSignature (unchecked — senders vary), dwMessage.
    let op = u32_at(buf, 4)?;
    let nid = &buf[8..];
    // Fixed head through hIcon: cbSize..hIcon = 24 bytes.
    if nid.len() < 24 {
        return None;
    }
    // The V1 struct (Win9x) ends after a 64-wchar tip; everything newer has
    // a 128-wchar tip and the state/version fields. Trust the buffer length,
    // not cbSize — senders lie about cbSize more often than about length.
    let long_form = nid.len() >= 288;
    let tip_wchars = if long_form { 128 } else { 64 };
    Some(TrayCommand {
        op,
        data: IconData {
            owner: handle_at(nid, 4)?,
            uid: u32_at(nid, 8)?,
            flags: u32_at(nid, 12)?,
            callback: u32_at(nid, 16)?,
            hicon: handle_at(nid, 20)?,
            tip: string_at(nid, 24, tip_wchars),
            state: if long_form {
                u32_at(nid, 280).unwrap_or(0)
            } else {
                0
            },
            state_mask: if long_form {
                u32_at(nid, 284).unwrap_or(0)
            } else {
                0
            },
            version: u32_at(nid, 800).unwrap_or(0),
        },
    })
}

/// One live tray icon in the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayIcon {
    pub owner: isize,
    pub uid: u32,
    pub callback: u32,
    /// Protocol version the owner negotiated (`NIM_SETVERSION`); 0 = legacy.
    pub version: u32,
    pub hidden: bool,
    pub tip: String,
    /// Latest raw `HICON` the owner supplied (0 = none yet).
    pub hicon: isize,
    /// Bumped whenever `hicon` changes, so icon-pixel caches know to refresh.
    pub rev: u32,
}

/// Outcome of applying one command to the registry. `handled` is the answer
/// the host returns to the sending app (`Shell_NotifyIcon`'s BOOL result) —
/// success even when nothing visibly changed. `repaint` is what the host
/// must do: something a viewer could see changed (set membership, icon
/// image, visibility). Tooltip and version updates are handled but need no
/// repaint (tips are not rendered yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    pub handled: bool,
    pub repaint: bool,
}

const REJECTED: Applied = Applied {
    handled: false,
    repaint: false,
};

/// Apply a parsed command to the registry. `NIM_ADD` upserts: stricter
/// hosts reject re-adds, but forgiving beats dropping a live app's icon.
pub fn apply(icons: &mut Vec<TrayIcon>, cmd: &TrayCommand) -> Applied {
    let key = (cmd.data.owner, cmd.data.uid);
    let pos = icons.iter().position(|i| (i.owner, i.uid) == key);
    match cmd.op {
        NIM_ADD | NIM_MODIFY => {
            let icon = match pos {
                Some(p) => &mut icons[p],
                None => {
                    if cmd.op == NIM_MODIFY {
                        // Modify of an unknown icon: the protocol says fail.
                        return REJECTED;
                    }
                    icons.push(TrayIcon {
                        owner: cmd.data.owner,
                        uid: cmd.data.uid,
                        callback: 0,
                        version: 0,
                        hidden: false,
                        tip: String::new(),
                        hicon: 0,
                        rev: 0,
                    });
                    let last = icons.len() - 1;
                    &mut icons[last]
                }
            };
            let d = &cmd.data;
            let mut repaint = pos.is_none();
            if d.flags & NIF_MESSAGE != 0 {
                icon.callback = d.callback;
            }
            if d.flags & NIF_ICON != 0 && icon.hicon != d.hicon {
                icon.hicon = d.hicon;
                icon.rev = icon.rev.wrapping_add(1);
                repaint = true;
            }
            if d.flags & NIF_TIP != 0 && icon.tip != d.tip {
                icon.tip = d.tip.clone();
            }
            if d.flags & NIF_STATE != 0 {
                let hidden = (d.state & d.state_mask & NIS_HIDDEN) != 0;
                let mask_hits = d.state_mask & NIS_HIDDEN != 0;
                if mask_hits && icon.hidden != hidden {
                    icon.hidden = hidden;
                    repaint = true;
                }
            }
            Applied {
                handled: true,
                repaint,
            }
        }
        NIM_DELETE => {
            let Some(p) = pos else { return REJECTED };
            icons.remove(p);
            Applied {
                handled: true,
                repaint: true,
            }
        }
        NIM_SETVERSION => match pos {
            Some(p) => {
                icons[p].version = cmd.data.version;
                Applied {
                    handled: true,
                    repaint: false,
                }
            }
            None => REJECTED,
        },
        // Focus requests are acknowledged but meaningless without keyboard
        // tray navigation.
        NIM_SETFOCUS => Applied {
            handled: pos.is_some(),
            repaint: false,
        },
        _ => REJECTED,
    }
}

/// `(wparam, lparam)` for forwarding a mouse event to an icon's owner, per
/// the version its owner negotiated. Legacy (< 4): wparam = uid, lparam =
/// the mouse message. Version 4: wparam = packed anchor coords, lparam =
/// low word event, high word uid.
pub fn callback_params(icon: &TrayIcon, msg: u32, x: i32, y: i32) -> (usize, isize) {
    if icon.version >= VERSION_4 {
        let wparam = (x as u16 as usize) | ((y as u16 as usize) << 16);
        let lparam = ((msg as u16 as u32) | ((icon.uid as u16 as u32) << 16)) as i32 as isize;
        (wparam, lparam)
    } else {
        (icon.uid as usize, msg as isize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a wire buffer: 8-byte header + a `long_form` NOTIFYICONDATA32.
    fn wire(op: u32, owner: u32, uid: u32, flags: u32, callback: u32, hicon: u32) -> Vec<u8> {
        let mut b = vec![0u8; 8 + 956];
        b[0..4].copy_from_slice(&0x34753423u32.to_le_bytes()); // magic (unchecked)
        b[4..8].copy_from_slice(&op.to_le_bytes());
        let nid = &mut b[8..];
        nid[0..4].copy_from_slice(&956u32.to_le_bytes()); // cbSize
        nid[4..8].copy_from_slice(&owner.to_le_bytes());
        nid[8..12].copy_from_slice(&uid.to_le_bytes());
        nid[12..16].copy_from_slice(&flags.to_le_bytes());
        nid[16..20].copy_from_slice(&callback.to_le_bytes());
        nid[20..24].copy_from_slice(&hicon.to_le_bytes());
        b
    }

    fn set_tip(b: &mut [u8], tip: &str) {
        for (i, u) in tip.encode_utf16().enumerate() {
            b[8 + 24 + i * 2..8 + 24 + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
    }

    #[test]
    fn parses_a_modern_add() {
        let mut b = wire(
            NIM_ADD,
            0x1234,
            7,
            NIF_MESSAGE | NIF_ICON | NIF_TIP,
            0x8001,
            0xbeef,
        );
        set_tip(&mut b, "My App");
        let cmd = parse(&b).unwrap();
        assert_eq!(cmd.op, NIM_ADD);
        assert_eq!(cmd.data.owner, 0x1234);
        assert_eq!(cmd.data.uid, 7);
        assert_eq!(cmd.data.callback, 0x8001);
        assert_eq!(cmd.data.hicon, 0xbeef);
        assert_eq!(cmd.data.tip, "My App");
    }

    #[test]
    fn wire_handles_sign_extend() {
        // A 32-bit handle with the high bit set must sign-extend, matching
        // how the kernel widens user handles.
        let b = wire(NIM_ADD, 0xfffe_1234, 1, NIF_MESSAGE, 0x8001, 0x8000_0001);
        let d = parse(&b).unwrap().data;
        assert_eq!(d.owner, 0xfffe_1234u32 as i32 as isize);
        assert_eq!(d.hicon, 0x8000_0001u32 as i32 as isize);
    }

    #[test]
    fn parses_the_ancient_short_form() {
        // V1 (Win9x-era): 152-byte struct, 64-wchar tip, nothing after it.
        let mut b = vec![0u8; 8 + 152];
        b[4..8].copy_from_slice(&NIM_ADD.to_le_bytes());
        let nid = &mut b[8..];
        nid[0..4].copy_from_slice(&152u32.to_le_bytes());
        nid[4..8].copy_from_slice(&42u32.to_le_bytes());
        nid[12..16].copy_from_slice(&NIF_TIP.to_le_bytes());
        for (i, u) in "old".encode_utf16().enumerate() {
            nid[24 + i * 2..24 + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        let cmd = parse(&b).unwrap();
        assert_eq!(cmd.data.owner, 42);
        assert_eq!(cmd.data.tip, "old");
        assert_eq!(cmd.data.state, 0);
    }

    #[test]
    fn hostile_buffers_never_panic() {
        for len in 0..30 {
            let _ = parse(&vec![0xa5u8; len]);
        }
        assert!(parse(&[]).is_none());
        assert!(parse(&[0; 8]).is_none()); // header but no struct
                                           // An unterminated max-length tip stops at the field edge.
        let mut b = wire(NIM_ADD, 1, 1, NIF_TIP, 0, 0);
        for i in 0..128 {
            b[8 + 24 + i * 2..8 + 24 + i * 2 + 2].copy_from_slice(&('x' as u16).to_le_bytes());
        }
        assert_eq!(parse(&b).unwrap().data.tip.len(), 128);
    }

    fn add(icons: &mut Vec<TrayIcon>, owner: isize, uid: u32, hicon: isize) -> Applied {
        apply(
            icons,
            &TrayCommand {
                op: NIM_ADD,
                data: IconData {
                    owner,
                    uid,
                    flags: NIF_MESSAGE | NIF_ICON,
                    callback: 0x8001,
                    hicon,
                    tip: String::new(),
                    state: 0,
                    state_mask: 0,
                    version: 0,
                },
            },
        )
    }

    fn cmd(op: u32, owner: isize, uid: u32, flags: u32, hicon: isize) -> TrayCommand {
        TrayCommand {
            op,
            data: IconData {
                owner,
                uid,
                flags,
                callback: 0,
                hicon,
                tip: String::new(),
                state: 0,
                state_mask: 0,
                version: 0,
            },
        }
    }

    #[test]
    fn registry_add_modify_delete() {
        let mut icons = Vec::new();
        assert!(add(&mut icons, 100, 1, 0xa).repaint);
        assert!(add(&mut icons, 100, 2, 0xb).handled);
        assert_eq!(icons.len(), 2);
        let rev = icons[0].rev;

        // Modify the icon image: rev bumps, repaint required.
        let r = apply(&mut icons, &cmd(NIM_MODIFY, 100, 1, NIF_ICON, 0xc));
        assert!(r.handled && r.repaint);
        assert_eq!(icons[0].hicon, 0xc);
        assert_eq!(icons[0].rev, rev + 1);
        // Callback stays: NIF_MESSAGE wasn't set.
        assert_eq!(icons[0].callback, 0x8001);

        // Modify of an unknown icon fails without inventing one.
        assert_eq!(
            apply(&mut icons, &cmd(NIM_MODIFY, 999, 9, NIF_ICON, 1)),
            REJECTED
        );
        assert_eq!(icons.len(), 2);

        let r = apply(&mut icons, &cmd(NIM_DELETE, 100, 1, 0, 0));
        assert!(r.handled && r.repaint);
        assert_eq!(icons.len(), 1);
        assert_eq!(icons[0].uid, 2);
        // Deleting again is a no-op failure, not a panic.
        assert_eq!(apply(&mut icons, &cmd(NIM_DELETE, 100, 1, 0, 0)), REJECTED);
    }

    #[test]
    fn re_add_upserts_and_hidden_state_tracks_the_mask() {
        let mut icons = Vec::new();
        add(&mut icons, 100, 1, 0xa);
        // Re-add with the same key updates in place instead of duplicating,
        // and reports success to the sender even though nothing changed.
        let r = add(&mut icons, 100, 1, 0xa);
        assert!(r.handled && !r.repaint);
        assert_eq!(icons.len(), 1);

        let hide = TrayCommand {
            op: NIM_MODIFY,
            data: IconData {
                owner: 100,
                uid: 1,
                flags: NIF_STATE,
                callback: 0,
                hicon: 0,
                tip: String::new(),
                state: NIS_HIDDEN,
                state_mask: NIS_HIDDEN,
                version: 0,
            },
        };
        let r = apply(&mut icons, &hide);
        assert!(r.handled && r.repaint);
        assert!(icons[0].hidden);
        // A state write whose mask doesn't cover NIS_HIDDEN leaves it alone:
        // handled, but nothing to repaint.
        let unrelated = TrayCommand {
            op: NIM_MODIFY,
            data: IconData {
                state: 0,
                state_mask: 0x2, // NIS_SHAREDICON
                ..hide.data.clone()
            },
        };
        let r = apply(&mut icons, &unrelated);
        assert!(r.handled && !r.repaint);
        assert!(icons[0].hidden);
    }

    #[test]
    fn tip_only_modify_is_handled_without_repaint() {
        let mut icons = Vec::new();
        add(&mut icons, 100, 1, 0xa);
        let mut c = cmd(NIM_MODIFY, 100, 1, NIF_TIP, 0);
        c.data.tip = "Battery: 73%".into();
        let r = apply(&mut icons, &c);
        assert!(r.handled && !r.repaint);
        assert_eq!(icons[0].tip, "Battery: 73%");
    }

    #[test]
    fn setfocus_and_unknown_ops_change_nothing() {
        let mut icons = Vec::new();
        add(&mut icons, 100, 1, 0xa);
        let before = icons.clone();
        // SETFOCUS on a live icon is acknowledged but is a visual no-op.
        let r = apply(&mut icons, &cmd(NIM_SETFOCUS, 100, 1, NIF_ICON, 0xff));
        assert!(r.handled && !r.repaint);
        // Unknown ops are rejected outright.
        assert_eq!(
            apply(&mut icons, &cmd(99, 100, 1, NIF_ICON, 0xff)),
            REJECTED
        );
        assert_eq!(icons, before);
    }

    #[test]
    fn setversion_switches_callback_encoding() {
        let mut icons = Vec::new();
        add(&mut icons, 100, 7, 0xa);
        // Legacy: wparam = uid, lparam = message.
        let (w, l) = callback_params(&icons[0], 0x0201, 50, 60);
        assert_eq!((w, l), (7, 0x0201));

        // The version switch must be REPORTED as success (the app decides
        // its own encoding based on this answer) while needing no repaint.
        let r = apply(
            &mut icons,
            &TrayCommand {
                op: NIM_SETVERSION,
                data: IconData {
                    owner: 100,
                    uid: 7,
                    flags: 0,
                    callback: 0,
                    hicon: 0,
                    tip: String::new(),
                    state: 0,
                    state_mask: 0,
                    version: VERSION_4,
                },
            },
        );
        assert!(r.handled && !r.repaint);
        // ...but a SETVERSION for an unknown icon is rejected, so the app
        // never ends up on an encoding the host didn't record.
        let mut unknown = cmd(NIM_SETVERSION, 999, 1, 0, 0);
        unknown.data.version = VERSION_4;
        assert_eq!(apply(&mut icons, &unknown), REJECTED);
        assert_eq!(icons[0].version, VERSION_4);
        // V4: coords in wparam; event + uid packed in lparam.
        let (w, l) = callback_params(&icons[0], 0x0201, 50, 60);
        assert_eq!(w, 50 | (60 << 16));
        assert_eq!(l, (0x0201 | (7 << 16)) as isize);
    }
}

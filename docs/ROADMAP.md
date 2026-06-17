# WinRestyle Roadmap

Phased plan. Each phase is shippable/demoable on its own and de-risks the next.

## Phase 0 — Safety harness & shell-swap spike ⭐ (current)

> The make-or-break phase. **No UI.** Goal: prove swap → crash → restore is
> bulletproof with a throwaway dummy shell. Do not build the taskbar until this
> is solid.

- [x] Cargo workspace + crate skeleton, docs, license.
- [ ] `wr-core`: read/backup/set/restore the per-user `Winlogon\Shell` value.
- [ ] `wr-shell` (dummy): a trivial window proving "I am the shell," with a
      "simulate crash" trigger for testing the watchdog.
- [ ] `wr-watchdog`: `Win+Ctrl+F1` global hotkey → emergency restore; monitor
      `wr-shell`; crash-loop fallback to `explorer.exe`.
- [ ] **Validate the mid-session full-shell restore mechanism** (the key unknown).
- [ ] Manual test protocol in a Win11 VM (swap, crash, hotkey, crash-loop, uninstall).

## Phase 1 — Minimal shell

- [ ] `wr-shell` paints desktop background / wallpaper.
- [ ] `wr-shell` spawns and supervises child surfaces (the taskbar).
- [ ] `wr-ipc` named-pipe protocol wired across watchdog ⇄ shell ⇄ installer.
- [ ] Config load from `%APPDATA%\WinRestyle\config.toml`.

## Phase 2 — Taskbar (flagship)

- [ ] Direct2D/DirectComposition rendering (acrylic, rounded, themeable).
- [ ] Running-window enumeration → buttons; activate / minimize / restore.
- [ ] Clock + basic widgets; Start button (stub launch).
- [ ] Pinned apps.
- [ ] **System tray hosting** (`Shell_TrayWnd` / `Shell_NotifyIcon` protocol).
- [ ] Multi-monitor + per-monitor DPI.

## Phase 3 — Installer / manager UI

- [ ] Component registry + `Component` trait (`install`/`uninstall`/`apply`).
- [ ] One-screen UI: component checklist + **Restyle Now** + uninstall.
- [ ] Safe apply: trial run → registry backup → swap; recovery instructions.
- [ ] **This is where the target UX ships.**

## Phase 4+ — Beyond

- [ ] Start menu (`wr-startmenu`).
- [ ] Theming engine (`wr-theme`): icons, accent colors, msstyles interop.
- [ ] Icon packs & themes; live customization UI.
- [ ] Plugin API for third-party components.
- [ ] Code signing, packaging (MSI/winget), auto-update.

## Definition of done for a component

A component is "done" when it: applies and reverts cleanly, survives a Windows
restart, recovers via the watchdog if it crashes, and has a manual test entry in
the VM test protocol.

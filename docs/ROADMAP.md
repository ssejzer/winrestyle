# WinRestyle Roadmap

Phased plan. Each phase is shippable/demoable on its own and de-risks the next.

## Phase 0 — Safety harness & shell-swap spike ✅ (complete 2026-07-18)

> The make-or-break phase. **No UI.** Goal: prove swap → crash → restore is
> bulletproof with a throwaway dummy shell. Do not build the taskbar until this
> is solid.

- [x] Cargo workspace + crate skeleton, docs, license.
- [x] `wr-core`: read/backup/set/restore the per-user `Winlogon\Shell` value. (T0 ✅)
- [x] `wr-shell` (dummy): a trivial window proving "I am the shell," with a
      "simulate crash" trigger for testing the watchdog.
- [x] `wr-watchdog`: `Win+Ctrl+F1` global hotkey → emergency restore; monitor
      `wr-shell`; crash-loop fallback to `explorer.exe`. (T1/T2/T3 ✅; a supervisor
      deadlock on the hotkey path was found and fixed during VM testing.)
- [x] **Validate the mid-session full-shell restore mechanism** (the key unknown).
      ✅ Confirmed 2026-07-18: explorer re-adopts the shell role on restore. See
      `docs/ARCHITECTURE.md`.
- [x] Manual test protocol in a Win11 VM — T0–T4 ✅ (swap, crash, hotkey,
      crash-loop, uninstall all pass; 2026-07-18, Win11 22H2 build 22621).
- [x] **Watchdog liveness** — the original bet (Winlogon `AutoRestartShell`,
      ADR 0001) was falsified by T5; replaced with mutual supervision
      (ADR 0002). Revised T5–T7 ✅ (watchdog relaunch, no duplicate desktop,
      crash-loop → full self-restore; 2026-07-18).

## Phase 1 — Minimal shell ✅ (complete 2026-07-19)

- [x] `wr-shell` paints desktop background / wallpaper (bottom-most
      virtual-screen window, config color + optional WIC image, repaint on
      `ReloadConfig`; automated T11 green 2026-07-18 — logs only; eyeball the
      visual half at the next manual T3).
- [x] `wr-shell` spawns and supervises child surfaces (the taskbar) —
      shipped as the Phase 2 taskbar skeleton, ADR 0005 (relaunch, crash-loop
      give-up, stray sweep on every recovery path). Automated T13 green
      2026-07-19.
- [x] `wr-ipc` named-pipe protocol wired across watchdog ⇄ shell (installer
      client lands with the Phase 3 UI; `RequestRestore` is already served).
- [x] `ShellHeartbeat` over `wr-ipc` — upgraded ADR 0002's process-handle
      mutual supervision to hang detection (both directions) and removed the
      PID-reuse race. (ADR 0003 + amendments; automated suite green 11/11,
      2026-07-18.)
- [x] Automated VM test harness (`scripts\vm-test.ps1`): T0–T2, T5–T9 run
      hands-off against release binaries; only T3 stays manual.
- [x] Config load from `%APPDATA%\WinRestyle\config.toml` (`wr-core::config`;
      hot reload wired to `ReloadConfig`; unit tests + automated T10 green in
      the VM 2026-07-18).
- [x] **Logon autostart** — run what explorer would at shell start so the user's
      session isn't degraded: `Run` / `RunOnce` keys (HKCU + HKLM) and the
      per-user + common Startup folders, plus session helpers like `rdpclip.exe`
      (clipboard/redirection in remote/VM sessions). Each entry is enumerable and
      **individually opt-in/out via config**; default mirrors Windows behavior.
      (Scheduled-task "at logon" items are launched by Task Scheduler, not the
      shell, so they still fire — not our responsibility.)
      Done per ADR 0004 (HKLM RunOnce skipped, once-per-logon-session guard,
      unswapped-session guard); automated T12 green 2026-07-19. Real-logon
      half (actual startup apps + no re-run after a crash relaunch) gets
      verified at the next manual T3.

## Phase 2 — Taskbar (flagship) ✅ (complete 2026-07-19)

> Suite 29/29 + manual T3 pass 2026-07-19: real swapped logon, live tray
> registration from a real app, and `Win+Ctrl+F1` restored the desktop.
> Still outstanding within these items (tracked, not blocking): visual
> checks that need specific setups — overflow menu and backdrop looks in a
> swapped session, tray *click* forwarding against picky v4 apps, and
> multi-monitor on real hardware. Deferred follow-ups: per-app grouping,
> appbar channel + balloons, more widgets.

- [x] Taskbar process skeleton (ADR 0005): spawned/supervised by the shell,
      `[taskbar]` config section (enabled/height/color/alpha/radius/margin)
      with hot reload and opt-out; non-topmost when unswapped. (Automated
      T13 green 2026-07-19.)
- [x] Direct2D/DirectComposition rendering, first slice: premultiplied-alpha
      composition swapchain (WARP fallback for GPU-less VMs), rounded
      translucent themed bar, DPI-aware, DirectWrite clock. (2026-07-19.)
- [x] Acrylic/mica backdrop + richer theming: `backdrop = "acrylic"|"mica"|
      "none"` via the documented DWM system-backdrop API (graceful log-and-
      fall-back on builds without it), `text_color`. Deeper theming is the
      Phase 4 engine. (2026-07-19; T15 asserts the code path, visuals at
      the next manual T3.)
- [x] Running-window enumeration → buttons; activate / minimize / restore.
      Event-driven (WinEvent hooks, no polling); stable button order;
      overflow drops the tail (grouping UI later); foreground chip
      highlighted. Automated T14 green 2026-07-19; interaction half gets
      verified at the next manual T3.
- [x] Button icons + hover states. Icons via `WM_GETICON` (abort-if-hung
      timeout) with class-icon fallback, decoded through GDI (legacy AND-mask
      alpha handled), premultiplied, uploaded once and cached per window.
      Hover via mouse-move hit-testing + `TrackMouseEvent` leave tracking.
      (2026-07-19; visual check at the next manual T3.)
- [x] Start button (stub launch): leftmost square chip, four-pane glyph,
      hover state; clicking taps the Win key — opens the system Start menu
      unswapped, lands on nothing in a swapped session (the real menu is
      `wr-startmenu`, Phase 4). (2026-07-19; visual + click check at the
      next manual T3.)
- [x] Overflow UI: dropped windows go behind a `»` chevron chip; clicking
      it opens a menu (bottom-aligned popup) and picking an entry focuses
      that window. Per-app *grouping* is deliberately deferred to a later
      polish pass — the overflow menu removes the lost-window problem.
      (2026-07-19.)
- [x] Widgets beyond the clock: date line (Eng. weekday/day/month) under
      the clock, `show_date` opt-out. Further widgets (battery, volume…)
      belong to later phases. (2026-07-19.)
- [x] Pinned apps: `pinned = [paths]`, icon chips after the Start button
      (SHGetFileInfo icons, letter fallback), click launches via
      `ShellExecuteW`. Pin/unpin UI is Phase 3 (manager) — config-only for
      now. (2026-07-19; T15 covers load + a real posted click.)
- [x] **System tray hosting** first slice (`Shell_TrayWnd` /
      `Shell_NotifyIcon`): swapped sessions only; NIM add/modify/delete/
      setversion (modern + ancient wire layouts, parser unit-tested),
      TaskbarCreated broadcast, icons right of the buttons, L/R click
      forwarding (legacy + v4 encodings), dead-owner pruning. Prereq
      resolved: `desktop_shell_running()` now counts only explorer-owned
      tray windows (ADR 0005 amendment). Appbar channel + balloons are
      out of scope for this slice. (2026-07-19; live behavior verifies at
      the next manual T3 — the automated suite runs unswapped by design.)
- [x] Multi-monitor + per-monitor DPI: one bar per monitor
      (`EnumDisplayMonitors`), per-bar swapchain/DPI/hover, display-change
      rebuild. Single-monitor VM asserts one-bar behavior (T15); real
      multi-monitor verification needs hardware and rides the T3
      checklist. (2026-07-19.)

## Phase 3 — Installer / manager UI ⭐ (code complete 2026-07-19; window rides next T3)

> Logic in cross-platform, unit-tested `wr-core` (`components`, `autostart`,
> `manager`, `config` write + `Wallpaper.enabled`); a thin Direct2D
> `wr-installer` window over it (`view.rs` pure + unit-tested like the taskbar's
> `layout.rs`). Suite green on host + type-checked on the Windows target;
> clippy/fmt clean. Automated **T16** covers the `--selftest` trial-run
> primitive. The manager *window* — checklist, startup list, Restyle Now, Undo —
> is visual and rides the next manual T3. No watchdog/IPC changes (ADR 0006).

- [x] Component registry + `Component` trait (`install`/`uninstall`; the
      registry's `apply(base, selected)` is the plural "apply"). Taskbar,
      Wallpaper, and Startup-programs components. (`wr-core::components`.)
- [x] One-screen UI: component checklist + **Restyle Now** + **Undo / Restore**,
      Direct2D-rendered (opaque `ID2D1HwndRenderTarget`), scrollable, DPI-aware.
      (`wr-installer` `view`/`render`/`app`; the CLI stays for T0/T4.)
- [x] **Startup-programs manager UI** — the Phase 1 logon-autostart entries as a
      per-entry on/off list, writing `[autostart].disabled`. Ids are shared with
      the shell's launch filter (`wr-core::autostart`) so they cannot drift.
- [x] Safe apply: preflight → write config → trial run (`wr-shell --selftest`) →
      registry backup + swap → recovery instructions. (`wr-core::manager`;
      teardown reuses the proven restore + sweep + conditional-explorer path.)
- [x] **This is where the target UX ships.** (Window visuals verify at T3.)

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

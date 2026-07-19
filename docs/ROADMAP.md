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

## Phase 1 — Minimal shell ✅ (complete 2026-07-19; T13 VM validation pending)

- [x] `wr-shell` paints desktop background / wallpaper (bottom-most
      virtual-screen window, config color + optional WIC image, repaint on
      `ReloadConfig`; automated T11 green 2026-07-18 — logs only; eyeball the
      visual half at the next manual T3).
- [x] `wr-shell` spawns and supervises child surfaces (the taskbar) —
      shipped as the Phase 2 taskbar skeleton, ADR 0005 (relaunch, crash-loop
      give-up, stray sweep on every recovery path). Automated T13 written;
      first VM run pending.
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

## Phase 2 — Taskbar (flagship) ⭐ (current)

- [x] Taskbar process skeleton (ADR 0005): spawned/supervised by the shell,
      `[taskbar]` config section (enabled/height/color/alpha/radius/margin)
      with hot reload and opt-out; non-topmost when unswapped. (2026-07-19;
      T13 VM run pending.)
- [x] Direct2D/DirectComposition rendering, first slice: premultiplied-alpha
      composition swapchain (WARP fallback for GPU-less VMs), rounded
      translucent themed bar, DPI-aware, DirectWrite clock. (2026-07-19.)
- [ ] Acrylic/blur backdrop; richer theming.
- [x] Running-window enumeration → buttons; activate / minimize / restore.
      Event-driven (WinEvent hooks, no polling); stable button order;
      overflow drops the tail (grouping UI later); foreground chip
      highlighted. Automated T14; interaction half verified at the next
      manual T3. (2026-07-19.)
- [ ] Button icons; hover states; overflow/grouping UI.
- [ ] Widgets beyond the clock (shipped with the first slice); Start button
      (stub launch).
- [ ] Pinned apps.
- [ ] **System tray hosting** (`Shell_TrayWnd` / `Shell_NotifyIcon` protocol).
      Prereq recorded in ADR 0005: `desktop_shell_running()` must first learn
      to tell explorer's `Shell_TrayWnd` from ours, or recovery idempotence
      breaks.
- [ ] Multi-monitor + per-monitor DPI.

## Phase 3 — Installer / manager UI

- [ ] Component registry + `Component` trait (`install`/`uninstall`/`apply`).
- [ ] One-screen UI: component checklist + **Restyle Now** + uninstall.
- [ ] **Startup-programs manager UI** — surface the Phase 1 logon-autostart
      entries as a per-entry on/off list, so the opt-in/out has a real UX.
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

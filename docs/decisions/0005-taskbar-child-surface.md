# ADR 0005 — The taskbar is a supervised child process

Date: 2026-07-19. Status: accepted. Amended 2026-07-19 (tray hosting; see
bottom).

## Context

Phase 2 begins the flagship taskbar. It is the largest and fastest-moving UI
surface we will ship: GPU rendering, window enumeration, later the system
tray protocol — by far the most likely place for crashes and hangs. The
process that hosts it must not be the process that carries the safety
harness.

The Phase 1 roadmap already anticipated this: "`wr-shell` spawns and
supervises child surfaces (the taskbar)".

## Decisions

1. **`wr-taskbar.exe` is a separate process, spawned and supervised by
   `wr-shell`.** Crash isolation is the point: a rendering bug takes down
   the bar, not the shell's guardian threads, heartbeat, or wallpaper. The
   binary sits next to `wr-shell.exe`, like the shell sits next to the
   watchdog.

2. **Surfaces are cosmetic; their supervision is deliberately weaker.** The
   shell relaunches a dead taskbar (crash-loop cap: >3 exits in 20 s, same
   shape as the watchdog's). On exhaustion it **gives up on the taskbar and
   keeps running** — a logged error, nothing more. A missing taskbar
   degrades the desktop; escalating it to recovery (or any registry action)
   would violate "never add a second recovery mechanism" for a component
   whose absence is survivable. Death detection only for now: no heartbeat —
   a *hung* taskbar is a frozen bar, not a safety problem. If that ever
   matters, extend the existing `wr-ipc` heartbeat pattern; do not invent a
   new mechanism.

3. **Every recovery path sweeps `wr-taskbar.exe`.** The taskbar is the
   watchdog's *grandchild*: killing the shell never reaps it, and Windows
   does not kill orphans. Without sweeps, an emergency restore would leave
   our bar floating over the recovered explorer desktop. Sweeps (shared
   helper `wr_core::process::kill_all_named`):
   - watchdog `recover()` (hotkey, crash-loop, IPC restore),
   - shell startup (stray taskbar from a crashed previous shell), before it
     spawns its own,
   - shell clean-shutdown paths (IPC `Shutdown`, last-resort
     `restore_windows_and_exit`).

   The watchdog's *startup* sweep deliberately covers only `wr-shell.exe`,
   not the taskbar: extra work between sweeping the stray shell and spawning
   the fresh one widens the single-process window that T7 caught on
   2026-07-19 (see the ADR 0002 amendment). The fresh shell's own sweep
   converges stray taskbars moments later.

4. **Config flows by file + nudge, not by pipe.** The taskbar loads
   `config.toml` itself (`wr-core::config`, same never-fail rules). On
   `ReloadConfig` the shell re-reads its store and posts a registered window
   message (`WinRestyleConfigChanged`) to the taskbar's window class; the
   taskbar then re-reads the file. No second IPC channel until a surface
   needs richer commands. `[taskbar] enabled` transitions are handled by the
   shell's supervisor (spawn/kill), not by the message.

5. **Unswapped sessions: never topmost.** At startup the taskbar checks
   `desktop_shell_running()` (explorer's `Shell_TrayWnd` present) and stays
   non-topmost if so, so dev/VM runs beside a live explorer don't cover the
   real taskbar. Swapped sessions get a topmost bar.

## Known hazard, recorded for the tray milestone

Recovery idempotency currently *detects a live desktop shell* via the
`Shell_TrayWnd` window class (`wr_core::shell::desktop_shell_running`). Tray
hosting (later in Phase 2) requires us to create a `Shell_TrayWnd` window
ourselves — which would make our own taskbar look like "explorer is running"
to every recovery path, breaking the launch-explorer-only-if-needed check
and the idempotence of `recover()`. **Before any tray work, that check must
distinguish explorer's tray window from ours** (e.g. by window process image
name), and this ADR must be amended with the chosen mechanism.

> **Resolved by the 2026-07-19 amendment below.**

## Rendering (context, not a decision)

First slice renders via D3D11 → premultiplied-alpha composition swapchain →
Direct2D, composed by DirectComposition (`WS_EX_NOREDIRECTIONBITMAP`), WARP
fallback so GPU-less VMs still render. True acrylic/blur is a later
refinement; translucency + rounded corners come from the alpha channel.

## Validation

Automated T13 (spawn/paint, relaunch, crash-loop give-up, config opt-out)
**green 2026-07-19** (full suite 24/24), after one finding: the first run's
T7 failure that led to the sweep→spawn window amendment in ADR 0002. Visual
half — bar on screen, and **no bar left after `Win+Ctrl+F1`** — at the next
manual T3.

## Amendment (2026-07-19) — tray hosting and the reworked shell check

Tray hosting shipped, with the hazard above resolved as follows.

1. **`desktop_shell_running()` now identifies each tray window's owner.**
   It walks *every* top-level `Shell_TrayWnd` with `FindWindowExW` and
   resolves each owner process (`GetWindowThreadProcessId` →
   `OpenProcess` → `QueryFullProcessImageNameW`). Per window:
   - owned by `wr-taskbar.exe` (basename, case-insensitive; unit-tested)
     → **not** a desktop shell — it is our own tray host;
   - owned by any *other* identifiable process → a desktop shell. This
     keeps the old "any `Shell_TrayWnd` counts" semantics for explorer
     *and* third-party shells (a LiteStep-style session must not get a
     stray explorer launched over it);
   - owner unresolvable → **not** a desktop shell. This direction was
     chosen deliberately: the recovery sweep (`kill_all_named`) terminates
     `wr-taskbar.exe` *without waiting*, so our dying tray host can be
     enumerable-but-unopenable at the exact moment `recover()` checks.
     Counting it would make recovery — which runs at most once — skip
     launching explorer and strand the user shell-less; not counting a
     real-but-unidentifiable shell merely opens one stray file-manager
     window. The prime invariant ("we must always be able to put explorer
     back") decides the tie: when in doubt, launch it.

2. **The tray host is a separate, hidden `Shell_TrayWnd` window** created
   by the taskbar **only in swapped sessions** (the same
   `desktop_shell_running()` probe that decides topmost). Unswapped, the
   host is never created: two live tray windows would fight for every
   app's `Shell_NotifyIcon` registration and could hijack icons from the
   dev machine's real taskbar. Consequence: the automated (unswapped)
   suite cannot exercise the host end-to-end; the wire-format parser and
   icon registry are pure and unit-tested instead, and the live half rides
   the manual T3 checklist.

3. **Protocol scope of this first slice.** `WM_COPYDATA` with `dwData == 1`
   (`NIM_ADD`/`MODIFY`/`DELETE`/`SETVERSION`, both the modern 956-byte and
   the ancient 152-byte `NOTIFYICONDATA` layouts, 32-bit wire handles
   sign-extended); `TaskbarCreated` broadcast at host start so running
   apps re-register; icons drawn right of the window buttons; L/R
   mouse down/up forwarded in the owner's negotiated encoding (legacy or
   `NOTIFYICON_VERSION_4`); owners polled for liveness on the clock tick
   so a crashed app's icon disappears. Not hosted yet: the appbar channel
   (`dwData == 0`, work-area negotiation), balloon notifications
   (`NIF_INFO` is parsed and ignored), `NIS_SHAREDICON`, and keyboard
   focus (`NIM_SETFOCUS` is a deliberate no-op).

4. **Failure posture.** The host window failing to create logs a warning
   and disables hosting; nothing else changes. A malformed or hostile
   registration buffer parses to `None` and is dropped — the parser is
   length-checked everywhere and fuzz-shaped unit tests pin that.

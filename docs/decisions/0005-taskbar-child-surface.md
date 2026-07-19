# ADR 0005 — The taskbar is a supervised child process

Date: 2026-07-19. Status: accepted.

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

## Rendering (context, not a decision)

First slice renders via D3D11 → premultiplied-alpha composition swapchain →
Direct2D, composed by DirectComposition (`WS_EX_NOREDIRECTIONBITMAP`), WARP
fallback so GPU-less VMs still render. True acrylic/blur is a later
refinement; translucency + rounded corners come from the alpha channel.

## Validation

Automated T13 (spawn/paint, relaunch, crash-loop give-up, config opt-out).
Visual half — bar on screen, and **no bar left after `Win+Ctrl+F1`** — at the
next manual T3.

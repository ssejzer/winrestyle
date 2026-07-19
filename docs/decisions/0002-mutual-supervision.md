# ADR 0002 — Watchdog crash recovery via mutual supervision

- **Status:** Accepted (2026-07-18)
- **Supersedes:** the `AutoRestartShell` reliance in [ADR 0001](0001-watchdog-liveness.md)
  (its stray-shell sweep and Phase 1 heartbeat plan stand).

## Context

ADR 0001 bet watchdog-crash recovery on Winlogon's `AutoRestartShell`
mechanism, flagged it as *must be confirmed in the VM*, and defined T5 as the
confirming test. **T5 failed** (2026-07-18, Win11 22H2 build 22621, Hyper-V):
with the swap applied, a killed `wr-watchdog.exe` was never relaunched —
`AutoRestartShell = 1` notwithstanding — leaving an orphaned `wr-shell`, no
emergency hotkey, and no supervision until a manual
`wr-installer restore` + `start explorer.exe`.

This matches long-standing reports from the kiosk/embedded community: the
mechanism effectively restarts only `explorer.exe`, not a custom shell (and
especially not one configured per-user in HKCU). The GINA/logon revamp after
Windows 2000 gutted the original behavior, and the standard workaround has
always been a monitor process. Sources:

- [Auto Restart a Custom Shell on termination](https://groups.google.com/g/microsoft.public.windowsxp.embedded/c/IZ1mBrlAruY)
  (microsoft.public.windowsxp.embedded)
- [Automatically restart shell](https://www.pcreview.co.uk/threads/automatically-restart-shell.2814802/)
- [Auto Restart Shell](https://www.visualautomation.com/comprod/secure6/auto_res.htm)

## Decision

**The watchdog and shell supervise each other.** No new process (rejecting
ADR 0001's option B a second time, now with the evidence updated):

- The watchdog supervises `wr-shell` exactly as before (relaunch on exit,
  crash-loop fallback to explorer).
- `wr-shell` watches the watchdog *process* and relaunches it if it dies. The
  relaunched watchdog's stray sweep (ADR 0001) then kills the old shell and
  spawns a fresh one — the pair always converges to exactly one of each.
- The `AutoRestartShell` startup check from ADR 0001 is removed: T5 proved the
  value is irrelevant to a custom per-user shell, and logging "Winlogon
  relaunches us" would be false confidence.

### Protocol (Phase 0: environment variables; Phase 1: `wr-ipc`)

Defined in `wr-core::guardian`:

- `WR_WATCHDOG_PID` — set by the watchdog on each shell it spawns; the shell
  opens that process handle and waits on it.
- `WR_WD_RELAUNCH_STATE` (`"<count>:<first-unix-secs>"`) — watchdog-relaunch
  accounting, threaded through the spawn chain. Needed because each hop in the
  relaunch cycle (shell relaunches watchdog → watchdog spawns fresh shell) is a
  *new* process whose in-memory counters reset; without carried state, a
  watchdog that crashes on startup would flicker forever.

**Runaway cap:** more than 3 watchdog relaunches within 60 s and the shell
stops relaunching, restores the registry itself (`wr-core`), and starts
`explorer.exe` — mirroring the watchdog's own shell-crash-loop policy.

## Amendment: the shell's monitor guards across generations (found by automated T7, 2026-07-18)

The first implementation's monitor was one-shot: relaunch the watchdog once,
then return, *assuming* the new watchdog's stray sweep would kill this shell.
Automated T7 (rapid kills) broke that assumption — a relaunched watchdog killed
during startup, before its sweep ran, left the shell alive with **no watchdog,
no hotkey, no supervision, and no detector for any of it**. The same happens
outside tests if the watchdog crashes early in startup.

Fix: the monitor loops. Each relaunched watchdog is spawned as the shell's
*child* and the monitor blocks in `child.wait()` — the normal outcome is still
that the sweep kills the shell mid-wait (one iteration), but if the watchdog
dies first, the monitor bumps the relaunch state and spawns the next one, up to
the existing runaway cap. Waiting on the child handle also eliminates PID-reuse
risk for every generation after the first.

## Amendment: the sweep→spawn single-process window (found by automated T7, 2026-07-19)

Between the startup stray-sweep killing the old shell and the supervisor
spawning the fresh one, **the relaunched watchdog is the only WinRestyle
process alive**. A kill landing in that window kills the whole family: no
shell exists to relaunch anything, and T7's rapid-kill loop hit exactly that
(watchdog logged its sweep and hotkey registration, died before
`shell launched`; everything gone after 2 kills, no cap trip). The window had
existed since Phase 0 — the Phase 2 change that also swept `wr-taskbar.exe`
at startup added a second full process-snapshot inside it, widening it enough
for the harness to land in it.

Mitigations:

1. The sweep moved from `run()` (main thread, before pipe/hotkey setup) into
   `supervise_shell` immediately before the spawn loop — sweep→spawn are now
   back-to-back on one thread, and the hotkey/pipe registration no longer sit
   inside the window. Side effect: a stray shell may briefly connect to the
   new pipe before being swept; the pipe server already survives client churn.
2. The watchdog does **not** sweep stray taskbars at startup. The fresh shell
   sweeps them itself before spawning its own (ADR 0005), and `recover()`
   sweeps them on every restore path. Nothing that can run later may run
   inside the window.
3. T7 now waits for the pair to converge (fresh shell pid) between kills: the
   test validates the runaway cap, and a kill inside the (few-ms) residual
   window is a distinct, humanly unreproducible failure mode.

The residual window — the tail of one process snapshot plus one
`CreateProcess` — is accepted, like the simultaneous-death gap above: the
recovery for both is the emergency knowledge that `Ctrl+Shift+Esc` → run
`explorer.exe` always works, and the next logon is unaffected (the registry
is untouched on this path).

Re-validated after the mitigations: full automated suite 24/24, including
T7, on 2026-07-19.

## Verification

Revised T5–T7 **all pass** (2026-07-18, Win11 22H2 build 22621, Hyper-V;
re-validated by the automated suite after the cross-generation amendment —
`scripts\vm-test.ps1`, 11/11):
killing either process brings the pair back; repeated kills trip the runaway
cap and the desktop self-restores to explorer. Note the cap's restore is
*permanent by design* — it rewrites `HKCU Shell`, so the next logon is stock
Windows and re-enabling WinRestyle requires an explicit `wr-installer apply`.
A broken install can never survive a re-logon.

## Consequences

- Watchdog *crash* recovery is now our own code on both sides, empirically
  testable (revised T5–T7 in `docs/TESTING.md`), instead of an OS mechanism
  that turned out not to apply.
- Known, accepted gaps for Phase 0:
  - **Both processes dying simultaneously** — nothing recovers; the user falls
    back to `Ctrl+Shift+Esc` → run `explorer.exe` manually. Unchanged from
    before.
  - **PID reuse race** — if the watchdog dies in the milliseconds between
    spawning the shell and the shell opening the handle, the shell could watch
    a recycled PID. Phase 1's pipe heartbeat removes this entirely.
  - **A hung (not dead) watchdog** — still deferred, as in ADR 0001; the
    Phase 1 heartbeat is the building block for closing it.
- The env-var protocol is deliberately temporary; Phase 1 replaces the
  process-handle wait with `ShellHeartbeat` over `wr-ipc`, which detects hangs
  as well as deaths in both directions.

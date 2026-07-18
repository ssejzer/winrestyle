# ADR 0001 — Keeping the watchdog itself alive

- **Status:** Superseded in part by [ADR 0002](0002-mutual-supervision.md)
  (2026-07-18) — **T5 failed**: `AutoRestartShell` does not restart a custom
  per-user shell, so the option-A reliance on it is abandoned. The stray-shell
  sweep and the Phase 1 heartbeat plan stand.
- **Phase:** closes the last Phase 0 open item; part of it lands in Phase 1.

## Context

Phase 0 locked in that **`wr-watchdog` is the registered shell** (`HKCU\…\Shell`
→ `wr-watchdog.exe`), and it owns two safety-critical jobs: the
`Win + Ctrl + F1` emergency hotkey and supervision/relaunch of `wr-shell`.

That raises the obvious question: *who watches the watchdog?* If the watchdog
dies or wedges, the hotkey and supervision vanish silently and the user can be
stranded. This was the one loose thread left after the Phase 0 tests passed.

## Analysis

The problem is smaller than it first looks, because being the shell buys us an
OS-level safety net.

1. **Windows already restarts the shell on crash.** Winlogon's
   `AutoRestartShell` value
   (`HKLM\…\Winlogon\AutoRestartShell`, **default `1`**) relaunches the
   registered shell process whenever it terminates — this is exactly why
   `explorer.exe` reappears after it crashes. Since the watchdog *is* the shell,
   **a watchdog crash is recovered by Windows for free.** ⚠️ *Must be confirmed
   in the VM* (see Verification) — including whether Winlogon throttles or gives
   up under a rapid restart loop.

2. **A crash is not a hang.** `AutoRestartShell` only fires on process
   *termination*. A watchdog that is alive but wedged (e.g. the mutex deadlock we
   hit and fixed during Phase 0 — a live process pumping no messages) will **not**
   be restarted by Winlogon. Hang detection needs an external observer.

3. **Orphaned children.** When the watchdog crashes, its `wr-shell` child is not
   killed automatically (Windows has no auto process-tree teardown). After
   Winlogon relaunches the watchdog it would spawn a *second* `wr-shell` — a
   duplicate blank desktop — unless the watchdog cleans up strays on startup.

4. **We never write `HKLM`.** `AutoRestartShell` lives under `HKLM` and defaults
   on, so we rely on it but do not set it — consistent with the architecture's
   "per-user only" rule. We may *read* it to warn.

## Options considered

- **A — Rely on `AutoRestartShell`, accept the hang gap (for now).** Zero new
  moving parts. Covers the common failure (crash). Leaves hangs uncovered.
- **B — A second guardian process ("watch the watchdog").** Robust against both
  crash and hang, but adds a permanent extra process and just moves the question
  up a level (who watches *it*?). Overkill at this stage.
- **C — Mutual IPC heartbeat.** `wr-shell` and `wr-watchdog` exchange periodic
  liveness pings over the Phase 1 named pipe; a missed heartbeat is treated as a
  hang. Detects a hung *shell* naturally, and is the building block for detecting
  a hung *watchdog* later. Cheap because Phase 1 builds `wr-ipc` anyway.

## Decision

- **Phase 0/1: rely on `AutoRestartShell` for watchdog crash recovery. Do not add
  a second guardian (reject B).** Lean on the OS mechanism; keep the process
  count minimal.
- **Two cheap hardening changes, do now:**
  1. On startup, the watchdog kills any stray `wr-shell.exe` before spawning its
     own — prevents duplicate desktops after a Winlogon-driven restart (fixes #3).
  2. The watchdog *reads* `AutoRestartShell` and logs a loud warning if it is not
     `1`. It never writes `HKLM` (respects #4).
- **Phase 1: add `ShellHeartbeat` over `wr-ipc`** so a *hung* `wr-shell` (alive
  but not pumping) is detected and recovered — closing part of #2 via option C.
- **Defer watchdog *hang* detection.** The only failure now uncovered is the
  watchdog itself hanging. The most likely cause (an internal deadlock) was
  already found and fixed in Phase 0. Revisit a second observer only if hangs
  prove real in practice — don't pay for it speculatively.

## Verification (add to the VM test protocol)

- **T5 — Winlogon restarts the shell:** with the swap applied and logged in, kill
  `wr-watchdog.exe` from Task Manager. ✅ Winlogon relaunches it (new pid),
  hotkey works again. (Confirms the `AutoRestartShell` assumption in #1.)
- **T6 — no duplicate desktop:** after T5, exactly one `wr-shell.exe` is running
  (confirms the orphan-cleanup change).
- **T7 — rapid restart:** force the watchdog to exit repeatedly; observe whether
  Winlogon throttles/stops restarting, and after how many attempts.

## Consequences

Cheap, no new long-lived process, and it leans on a battle-tested OS mechanism
rather than reinventing it. The residual, knowingly-accepted gap is a hung
watchdog — narrow, and mitigated by the deadlock fix and the Phase 1 shell
heartbeat.

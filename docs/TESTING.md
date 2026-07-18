# WinRestyle — Manual Test Protocol (Phase 0)

> **Run everything below inside a disposable Windows 11 VM with snapshots.**
> A bug here can leave a blank desktop. Take a snapshot named `clean` before
> you start, and revert to it between runs.

## Prerequisites

- Windows 11 VM, snapshot `clean` taken.
- Rust (MSVC toolchain) installed in the VM, or copy a release build in.
- Build: `cargo build --release` → binaries land next to each other in
  `target\release\` (`wr-watchdog.exe`, `wr-shell.exe`, `wr-installer.exe`).

## T0 — Registry backup/restore is reversible (no shell swap yet)

This tests `wr-core` logic without ever leaving you shell-less.

1. `wr-installer status` → note `HKCU Shell (current)` and `backup present: false`.
2. `wr-installer apply` → `backup present` becomes true; `HKCU Shell` now points
   at `wr-shell.exe`. **Do not log out yet.**
3. `wr-installer restore` → `HKCU Shell` returns to the original value (or is
   removed if there was none); `backup present: false`.
4. ✅ Pass if the value is byte-for-byte what it was at step 1.

## T1 — Watchdog relaunches a crashed shell

Run the watchdog directly (it spawns `wr-shell` itself); no registry swap needed.

1. Build a shell that crashes: the watchdog launches `wr-shell.exe`; to force a
   crash for this test, temporarily run the shell via the watchdog with a short
   crash timer (see `wr-shell --crash-after=`), or hard-kill `wr-shell.exe` from
   Task Manager.
2. ✅ Pass if the watchdog logs the exit and relaunches a new `wr-shell` (new pid).

## T2 — Crash-loop falls back to explorer

1. Arrange for `wr-shell` to crash repeatedly faster than the relaunch window
   (`--crash-after=1`).
2. ✅ Pass if, after `CRASH_LIMIT` exits within `CRASH_WINDOW`, the watchdog
   stops relaunching, logs "crash-loop", restores the registry, and launches
   `explorer.exe`.

## T3 — Emergency hotkey restores the desktop  ⭐ the critical test

This is the real shell-swap test. **Snapshot first.**

1. `wr-installer apply`, then arrange for the watchdog to start at logon
   (Phase 0: launch it manually as the "shell" stand-in, or wire it as the
   Shell value — see open question below).
2. Log out and back in. Expect a blank/minimal desktop (the dummy shell).
3. Press **`Win + Ctrl + F1`**.
4. ✅ Pass if the original Windows desktop (taskbar, Start) returns and
   `HKCU Shell` is restored. ❌ If explorer only opens a file window, the
   mid-session restore mechanism needs work (this is the known Phase 0 risk).

## T4 — Uninstall leaves no trace

1. `wr-installer restore`.
2. ✅ Pass if `HKCU Shell` matches the `clean` snapshot and the `HKCU\Software\
   WinRestyle` backup key is gone.

## T5 — Shell relaunches a killed watchdog  ⭐ validates ADR 0002

> The original T5 (Winlogon's `AutoRestartShell` relaunches the watchdog)
> **failed** on 2026-07-18 — the mechanism ignores custom per-user shells. See
> `docs/decisions/0002-mutual-supervision.md`; the shell now relaunches the
> watchdog itself.

**Snapshot first.**

1. `wr-installer apply`, log out and back in (the watchdog is now the running
   shell, as in T3). Note the `wr-watchdog.exe` and `wr-shell.exe` pids in Task
   Manager (`Ctrl+Shift+Esc`).
2. Kill `wr-watchdog.exe` from Task Manager (simulates a watchdog crash).
3. ✅ Pass if, within a couple of seconds: the shell logs
   "watchdog … died; relaunching"; a new `wr-watchdog.exe` appears (new pid);
   **and** `Win + Ctrl + F1` still restores the desktop afterwards.

## T6 — No duplicate desktop after a watchdog relaunch

Run immediately after T5 step 2 (before pressing the hotkey).

1. In Task Manager, count `wr-shell.exe` and `wr-watchdog.exe` processes.
2. ✅ Pass if exactly **one of each** is running. The relaunched watchdog must
   have killed the old shell (log: "killing stray wr-shell.exe") and spawned a
   fresh one (new `wr-shell.exe` pid vs. step 1 of T5).

## T7 — Watchdog crash-loop ends in a restored desktop

Validates the runaway cap (`wr-core::guardian`: >3 relaunches within 60 s).

1. From T5's end state, kill `wr-watchdog.exe` again as soon as it reappears;
   repeat quickly.
2. ✅ Pass if after ~4 kills within a minute the cycle stops: the shell logs
   "watchdog crash-loop … restoring Windows", `HKCU Shell` is restored, and the
   normal explorer desktop comes back on its own.
3. Revert the snapshot when done.

## T8 — Hung shell is killed and relaunched  (ADR 0003)

No registry swap needed — run the watchdog directly from a terminal.

1. In a terminal: `set WR_SHELL_TEST_ARGS=--hang-heartbeat-after=10`, then run
   `wr-watchdog.exe` (the spawned shell inherits the env var and hangs its
   heartbeat after 10 s while staying *alive*).
2. ✅ Pass if ~5–6 s after the hang the watchdog logs
   "shell heartbeat silent … killing hung shell" and relaunches it (new pid).
   (The env var is inherited, so every relaunched shell hangs again — expect
   the cycle to repeat about every 15 s.)
3. Crash-loop interaction: rerun with `--hang-heartbeat-after=1`. Now each
   hang-kill-relaunch cycle is ~6 s, fast enough to accumulate >3 exits inside
   the 20 s crash-loop window. ✅ Pass if after ~4 cycles the watchdog logs
   "crash-loop", restores the registry, and falls back to explorer.

## T9 — Hung watchdog is killed and relaunched  (ADR 0003)

**Snapshot first** if running swapped; also works unswapped from a terminal.

1. Start the watchdog with `--ack-hang-after=20`. After 20 s its pipe server
   freezes (simulating a wedged watchdog with a dead hotkey).
2. ✅ Pass if ~5–6 s later the shell logs "watchdog silent … killing hung
   watchdog", the monitor relaunches a fresh watchdog (new pid, no test flag),
   the pair converges to one of each, and `Win + Ctrl + F1` works again.

## Resolved Phase 0 question

How the watchdog is launched as the shell, and whether `explorer.exe` re-adopts
the shell role mid-session: **resolved 2026-07-18** — the watchdog *is* the
registered shell, and mid-session restore works (T0–T4 pass). Findings recorded
in `docs/ARCHITECTURE.md`; liveness follow-up in
`docs/decisions/0001-watchdog-liveness.md`.

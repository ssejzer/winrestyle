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

## T5 — Winlogon relaunches a killed watchdog  ⭐ validates ADR 0001

The watchdog's own crash recovery is Winlogon's `AutoRestartShell` (HKLM,
default `1`) — see `docs/decisions/0001-watchdog-liveness.md`. This test
confirms that assumption. **Snapshot first.**

1. `wr-installer apply`, log out and back in (the watchdog is now the running
   shell, as in T3). Note the `wr-watchdog.exe` pid in Task Manager.
2. Kill `wr-watchdog.exe` from Task Manager (simulates a watchdog crash).
3. ✅ Pass if Winlogon relaunches the watchdog (new pid appears within a few
   seconds, and its log shows the startup banner again) **and** the
   `Win + Ctrl + F1` hotkey still restores the desktop afterwards.
4. ❌ If nothing relaunches, ADR 0001's "no second guardian" decision must be
   revisited before Phase 1.

## T6 — No duplicate desktop after a watchdog restart

Run immediately after T5 step 3 (before pressing the hotkey).

1. In Task Manager, count `wr-shell.exe` processes.
2. ✅ Pass if exactly **one** `wr-shell.exe` is running — the relaunched
   watchdog must have killed the orphaned child from the old instance (its log
   shows a "killing stray wr-shell.exe" line) before spawning its own.

## T7 — Rapid watchdog restart (Winlogon throttling)

Determines whether Winlogon throttles or gives up when the shell keeps dying.

1. From T5's end state, kill the relaunched `wr-watchdog.exe` again as soon as
   it appears; repeat ~5–10 times in quick succession.
2. Record: does Winlogon keep relaunching indefinitely, or stop after N
   attempts / start delaying? There is no fixed pass criterion — the goal is
   data. Write the observed behavior into ADR 0001.
3. Finish with `Win + Ctrl + F1` (if a watchdog is alive) or a manual
   `wr-installer restore` + re-logon, then revert the snapshot.

## Resolved Phase 0 question

How the watchdog is launched as the shell, and whether `explorer.exe` re-adopts
the shell role mid-session: **resolved 2026-07-18** — the watchdog *is* the
registered shell, and mid-session restore works (T0–T4 pass). Findings recorded
in `docs/ARCHITECTURE.md`; liveness follow-up in
`docs/decisions/0001-watchdog-liveness.md`.

# ADR 0004 — Logon autostart policy

Date: 2026-07-18. Status: accepted.

## Context

When WinRestyle is the registered shell, explorer never runs, so nothing
launches the user's startup programs — OneDrive, IMEs, clipboard helpers. A
swapped session would be silently degraded. The shell must run what explorer
would at logon (Phase 1 roadmap item).

Explorer's logon work: `Run`/`RunOnce` registry keys (HKLM + HKCU), the
per-user and common Startup folders, and session helpers such as
`rdpclip.exe`. Some of this collides with our invariants.

## Decisions

1. **HKLM `RunOnce` entries are skipped, loudly.** Honoring them requires
   deleting HKLM values — forbidden by the never-write-HKLM invariant — and
   running them *without* deleting would re-run installers every logon, which
   is worse than not running them. Each skipped entry is logged as a warning.
   (In practice these are rare on end-user machines and are usually consumed
   by the next explorer logon anyway.) HKLM `Run` is read-only to honor, so
   it launches normally.

2. **HKCU `RunOnce` follows the Windows contract**: delete the value, then
   run the command (reversed for `!`-prefixed names, per the documented
   semantics — those delete only after a successful launch). The deletion is
   deliberately **not** backed up: restoring it would violate the "once"
   contract. This is a scoped exception to the "every registry change is
   backed up" invariant, and it is the OS's contract, not our restyling.

3. **Once per logon session.** The watchdog relaunches a crashed shell, and
   the shell relaunches a crashed watchdog; autostart must not re-run on
   every relaunch. Guard: a marker file (`%LOCALAPPDATA%\WinRestyle\
   autostart-session`) holding the logon-session LUID (`TOKEN_STATISTICS.
   AuthenticationId` — unique per logon, even across reboots). Same LUID →
   skip. Survives arbitrary process churn, needs no registry writes, and
   self-invalidates at the next logon. If the LUID cannot be read (never
   observed), we *fail open* and run: a rare double-launch beats silently
   degrading every session.

4. **Never in an unswapped session.** If another desktop shell
   (`Shell_TrayWnd`) is on screen, autostart is a no-op. This keeps every
   dev/test run (T1–T11 launch the pair dozens of times) from spraying the
   host session with startup apps, and mirrors the recovery-idempotence
   invariant's "check the screen before acting" approach.

5. **Per-entry opt-out via config.** Every entry gets a stable, logged id —
   `hkcu-run:<name>`, `hklm-run:<name>`, `hkcu-runonce:<name>`,
   `startup-user:<file>`, `startup-common:<file>`, `session:rdpclip` — and
   `[autostart].disabled` (case-insensitive) skips it; `enabled = false` is
   the master off switch. Defaults mirror Windows: everything runs. The
   Phase 3 manager UI will surface these ids as a checklist.

6. **Test hook**: `--autostart-test-filter=<substr>` (via `WR_SHELL_TEST_ARGS`)
   bypasses guards 3 and 4 but launches only entries whose id contains the
   substring. T12 uses it to exercise the real enumerate → filter → launch
   path against disposable `WinRestyleT12*` entries without touching the VM
   session's real startup apps.

## Known gaps (accepted for Phase 1)

- **WOW64 registry views** (`Wow6432Node` `Run` keys) are not enumerated yet.
- **No waiting/ordering guarantees**: explorer serializes some RunOnce
  processing; we fire-and-forget everything. No observed consequence yet.
- Scheduled-task "at logon" items are Task Scheduler's job, not the shell's —
  they fire regardless of us (already noted in the roadmap).

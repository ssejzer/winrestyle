# ADR 0008 — Live activate / deactivate (no re-logon)

Date: 2026-07-19. Status: accepted.

## Context

Since Phase 3, applying a restyle swapped `HKCU Shell` and told the user to log
out and back in; only *restoring* was live (the `Win+Ctrl+F1` hotkey, validated
at T3, kills our family, restores the registry, and relaunches explorer
mid-session — no re-logon). The asymmetry was an accident of history, not a
design need: the restore direction proved in Phase 0 that a session's shell can
be replaced while it runs. Activation is the same transition with the roles
reversed.

The constraint from ADR 0006 §4 stands: nothing new may ride the single-client
IPC pipe. Fortunately live activation needs no IPC at all — it is pure process
lifecycle, built from primitives every prior phase validated.

## Decisions

1. **`manager::activate_now()` performs the exact transition the next logon
   would**, in this order: preflight → converge to zero WinRestyle processes →
   stop `explorer.exe` → launch `wr-watchdog.exe` (which spawns the shell,
   which spawns the taskbar; the taskbar sees no foreign desktop shell and
   comes up swapped — topmost, tray host on). The emergency hotkey is armed as
   soon as the watchdog starts. Killing explorer is the one deliberate
   exception to `process`'s "own executable names only" rule, documented
   there; it closes open File Explorer windows, which the confirm prompts say
   out loud.

2. **Best-effort with a safe fallback.** The registry swap has already
   happened before activation is attempted, so the worst case is always the
   proven "active at next logon" state. Concretely: ADR 0001/T5 showed
   winlogon's `AutoRestartShell` manages *explorer* (never custom shells), so
   on some setups Windows may relaunch explorer right after we stop it. After
   a short settle, `activate_now` checks `desktop_shell_running()` (foreign
   `Shell_TrayWnd`s only — ADR 0005 amendment); if explorer's desktop is
   back, it sweeps our family out and reports that activation deferred to the
   next sign-in. Two desktops never intentionally coexist.

3. **Sweeps must out-stubborn mutual supervision.** A single kill pass loses
   by design: the watchdog and shell resurrect each other (ADR 0002 — the
   feature), so killing one lets the survivor respawn it. `sweep_wr_processes`
   kills all three names repeatedly until a full pass finds nothing (round
   cap, best-effort) — the same converge-by-repetition the harness's
   `Stop-WrProcesses` has always used. `manager::uninstall` now uses it too
   and includes the watchdog, which it previously spared; before this, an
   Undo from *inside* a live session left the watchdog alive to respawn the
   shell it had just killed, and the desktop came back. Undo is now a true
   live deactivation.

4. **CLI verbs `activate` and `deactivate`** expose both directions
   scriptably (that is what makes T18 automatable). `apply` and `restore`
   are byte-for-byte unchanged (T0/T4 depend on them); `restore` remains the
   bare registry restore, `deactivate` = `uninstall()` (restore + sweep +
   conditional explorer). The manager asks "Activate now?" (Yes/No) after a
   successful apply — after the recovery-instructions dialog, so the user
   holds the hotkey before the desktop churns.

5. **What this is not:** live *config re-apply* to a running session (theme
   edits taking effect without restarting anything) still has no
   manager→shell channel — that remains deferred behind the multi-client
   pipe rework (ADR 0006 §4). Live activation sidesteps it by restarting the
   family, which rereads config from disk anyway.

## Validation

- Unit tests: recovery/instruction text (updated), preflight unchanged. The
  sweep/activation sequence is process-lifecycle code, validated in the VM.
- Automated **T18** (runs last): `apply` + `activate` must leave exactly one
  watchdog/shell/taskbar, no explorer, and the swapped-mode log signature
  (`taskbar up: … topmost, tray host active`); `deactivate` must bring
  explorer back, sweep the family to zero, and remove the backup key. The
  harness's `finally` relaunches explorer if the test dies mid-swap.
- **Manual, next T3:** the manager's Yes/No flow end to end, and the
  backout path on any machine where explorer auto-relaunches.

## Consequences

Activating live closes the user's open File Explorer windows (explorer is one
process); both the manager prompt and the CLI warn first. The
explorer-relaunch race outside the settle window is accepted: if explorer
comes back *later* than the check, the hotkey or Undo resolves it, and the
next logon is correct regardless. `recovery_instructions()` now names
`wr-installer deactivate`; the headline no longer claims a logout is required.
Nothing in the watchdog, shell, taskbar, or IPC layer changed — the whole
feature lives in `wr-core::{manager,process}` and the installer.

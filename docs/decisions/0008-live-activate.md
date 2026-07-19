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
   stop the outgoing desktop **and the session tree it spawned** → launch
   `wr-watchdog.exe` (which spawns the shell, which spawns the taskbar; the
   taskbar sees no foreign desktop shell and comes up swapped — topmost, tray
   host on). The emergency hotkey is armed as soon as the watchdog starts.
   Stopping the desktop tree is the one deliberate exception to `process`'s
   "own executable names only" rule, documented there (see the amendment).

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
- **First VM run (2026-07-19): activation itself passed live** — the session
  swapped onto the WinRestyle desktop mid-suite and `Win+Ctrl+F1` restored
  explorer without a logout — but T18 recorded nothing because the harness
  hung on `Start-Process -Wait`, which waits for *descendants* (the watchdog
  family activate leaves running; the relaunched explorer after the hotkey
  kept it waiting even post-restore). Harness fixed to wait on the installer
  process alone; full T18 pass pending.
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

## Amendment (2026-07-19) — activation ends the old session's app tree

**Context.** The first cut of `activate_now` stopped only `explorer.exe`,
leaving the apps the user had launched from the old shell still running under
the new one. That looked safer, but it is inconsistent with what activation
*replaces*: the prior way to start WinRestyle mid-day was to log out and back
in, and **a logout terminates the whole session** — every app the user had
open. Leaving them running was the surprise, not stopping them.

**Decision.** Activation now stops the outgoing desktop **and the descendant
process tree it spawned**, via `process::kill_tree_named("explorer.exe")` —
the transitive children of every `explorer.exe`, which is where classic apps
launched from the shell sit (Windows parents them to explorer and, by design,
does *not* cascade-kill them when explorer dies, so they must be ended
explicitly). Two guards keep it safe:

- **The invoking branch is spared.** The kill excludes this process and its
  entire ancestor chain, so the terminal running `wr-installer activate` (or
  the manager window that a click drove) — and, in a console run, the parent
  process waiting on us — survive even though they are themselves children of
  the dying desktop. Without this, the CLI would kill its own terminal and the
  automated **T18** would kill the harness PowerShell driving it.
- **It is forceful, not graceful.** `TerminateProcess` gives no
  `WM_QUERYENDSESSION` save prompt the way a real logout does, so the manager's
  Yes/No dialog and the CLI warning both say "save your work first."

The pid-tree math (`descendant_pids`, `ancestor_pids`) is pure and
unit-tested on the dev host; only the process snapshot and the kill are
Windows. Shell-UI hosts parented elsewhere (e.g. `StartMenuExperienceHost`,
`RuntimeBroker` under `svchost`/`sihost`) are *not* explorer descendants and
are left alone — they idle out or relaunch on demand, and several back the
user's UWP apps, so sweeping them would over-reach.

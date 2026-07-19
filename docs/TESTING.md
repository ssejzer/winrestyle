# WinRestyle — VM Test Protocol

> **Run everything below inside a disposable Windows 11 VM with snapshots.**
> A bug here can leave a blank desktop. Take a snapshot named `clean` before
> you start, and revert to it between runs.

## Automated harness — run this first

Almost everything below is automated. In the VM:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\vm-test.ps1
```

It pulls, builds release, runs the unit tests, then executes **T0, T1, T2,
T5–T18** against the real binaries and prints a PASS/FAIL summary. Per-test
logs land in `target\vm-test-logs\`. Everything runs unswapped except **T18**,
which (deliberately, last) live-swaps the session's desktop and puts it back.
Flags: `-SkipPull` (test local changes), `-SkipBuild`, `-SkipUnit`, and
`-Tests` to run a subset instead of the full regression — comma/space
separated, wildcards allowed:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\vm-test.ps1 `
    -SkipPull -SkipBuild -SkipUnit -Tests 'T17,T18'
```

**Still manual, once per release:** **T3** — the real swap + logon + blank
desktop + `Win + Ctrl + F1` — because it needs a human at the logon screen,
and the registry-hygiene halves of **T4**. The sections below remain the
reference for what each test means and for running one by hand when debugging.

## Prerequisites

- Windows 11 VM, snapshot `clean` taken.
- Rust (MSVC toolchain) installed in the VM, or copy a release build in.
- Build: `cargo build --release` → binaries land next to each other in
  `target\release\` (`wr-watchdog.exe`, `wr-shell.exe`, `wr-taskbar.exe`,
  `wr-installer.exe`).

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

This is the real shell-swap test. **Snapshot first.** Last passed
2026-07-19 (Phase 2 complete: bar + live tray registration on screen in the
swapped session; hotkey restored the Windows desktop).

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
   freezes (simulating a partially wedged watchdog).
2. ✅ Pass if ~5–6 s later the frozen watchdog is *replaced*: a fresh watchdog
   (new pid, no test flag) takes over and the pair converges to one of each
   with `Win + Ctrl + F1` working. Either mechanism counts (see ADR 0003
   amendment): the watchdog self-exits ("own pipe thread is wedged", usually
   first) or the shell kills it ("killing hung watchdog").

## T10 — Config loads at startup and hot-reloads over IPC  (Phase 1)

No registry swap needed — run the watchdog directly from a terminal.

1. Write `%APPDATA%\WinRestyle\config.toml` with a recognizable value, e.g.
   `[wallpaper]` / `color = "#112233"`. **The harness backs up and restores
   your real config byte-identically; do the same by hand.**
2. Start the watchdog with `--send-reload-every=3` (test flag: sends the shell
   `ReloadConfig` every 3 s — nothing sends it for real until the Phase 3
   installer).
3. ✅ Pass if the shell logs `config: wallpaper color #112233` at startup.
4. Change the file's color to `#445566`. ✅ Pass if within a few seconds the
   shell logs `ReloadConfig received` and `config now: wallpaper color #445566`.
5. Resilience (covered by unit tests, worth eyeballing once): a *broken* file
   at startup logs an error and the shell still starts with defaults; a broken
   file at reload keeps the previous good config.

## T11 — Wallpaper paints and hot-repaints  (Phase 1)

Same setup as T10 (the harness runs them as one scenario).

1. ✅ Pass if the shell logs `wallpaper window up (…)` and
   `wallpaper painted: color #112233` shortly after startup.
2. After changing the config color (T10 step 4), ✅ pass if it logs
   `wallpaper painted: color #445566`.
3. **Automated caveat:** unswapped (explorer still running), the wallpaper
   window sits *behind* explorer's desktop, so the harness verifies paint
   events via logs, not pixels. The visual check — wallpaper actually visible,
   image file rendering, fallback color on a broken image path — happens
   during the manual T3 release pass.

## T12 — Logon autostart with per-entry opt-out  (Phase 1, ADR 0004)

No registry swap needed. The `--autostart-test-filter=<substr>` test flag (via
`WR_SHELL_TEST_ARGS`) makes the shell run autostart even though explorer is on
screen, but only for entries whose id contains the substring — the session's
real startup apps are never launched by a test.

1. Create disposable HKCU `Run` + `RunOnce` values named `WinRestyleT12*`
   whose commands write marker files. (A fresh Win11 image may lack the HKCU
   `RunOnce` key entirely — Windows creates it on demand; the harness creates
   and later removes it. The shell tolerates the missing key by design.)
2. Start the watchdog with `WR_SHELL_TEST_ARGS=--autostart-test-filter=WinRestyleT12`.
3. ✅ Pass if both markers appear, and the `RunOnce` value is *deleted* from
   the registry (the Windows RunOnce contract).
4. Write `[autostart]` / `disabled = ["hkcu-run:WinRestyleT12"]` to the config
   and start a fresh pair. ✅ Pass if the shell logs
   `autostart: skipped hkcu-run:WinRestyleT12 (disabled in config)` and the
   entry does not run.
5. What the harness can't cover (verify at the manual T3 release pass): in a
   *real* swapped logon, the user's actual startup apps come up, and a crash
   relaunch of the shell does **not** re-run them (the session-marker guard;
   look for "autostart already ran this logon session" in the logs).

## T13 — Taskbar surface supervision  (Phase 2, ADR 0005)

No registry swap needed. Unswapped, the taskbar detects explorer's live
desktop (`Shell_TrayWnd`) and stays **non-topmost**, so it never covers the
real taskbar during testing; in a swapped session it is topmost.

1. Start the watchdog plain (default config). ✅ Pass if `wr-taskbar.exe` is
   running, and the logs show `taskbar launched (pid …)`, `taskbar window up
   (…)`, and `taskbar painted: color #10101a alpha 224`. (A GPU-less VM logs
   `using WARP (software) rendering` — that is fine.)
2. Kill `wr-taskbar.exe` from Task Manager. ✅ Pass if the shell logs
   `taskbar exited unexpectedly` / `relaunching taskbar` and a new
   `wr-taskbar.exe` pid appears within a second or two.
3. Crash-loop: set `WR_TASKBAR_TEST_ARGS=--crash-after=1` and start a fresh
   pair. ✅ Pass if after 4 exits within 20 s the shell logs
   `taskbar crash-loop … giving up on the taskbar` and — the point of the
   policy — `wr-shell.exe` and `wr-watchdog.exe` keep running. A broken
   taskbar degrades the desktop; it must never take it down.
4. Config opt-out: write `[taskbar]` / `enabled = false` and start a fresh
   pair. ✅ Pass if the shell logs `taskbar disabled in config; not spawning
   it` and no `wr-taskbar.exe` appears.
5. What the harness can't cover (verify at the manual T3 release pass): the
   bar is actually *visible* (bottom of the primary monitor, rounded,
   translucent, clock on the right); the Start button (square chip with a
   four-pane glyph) sits leftmost and lightens on hover — clicking it
   unswapped opens the system Start menu, swapped it does nothing visible
   but logs `start button clicked` (stub; real menu is a later phase); and
   after `Win + Ctrl + F1` the restored explorer desktop has **no WinRestyle
   bar left on screen** (recovery paths sweep `wr-taskbar.exe`).

## T14 — Taskbar buttons track running windows  (Phase 2)

No registry swap needed; works unswapped (the bar enumerates the session's
real windows either way — unswapped it simply sits non-topmost).

1. Start the watchdog plain. ✅ Pass if shortly after `taskbar window up` the
   log shows `taskbar windows: N` with one `window added: "…"` per
   taskbar-worthy window on screen (alt-tab rules: visible, unowned,
   non-tool, non-cloaked, titled).
2. Open a new window with a distinctive title (the harness uses a
   `WScript.Shell` popup titled `WinRestyleT14` — locale-independent).
   ✅ Pass if `window added: "WinRestyleT14"` appears within a second or two
   (event-driven — WinEvent hooks, no polling).
3. Close it. ✅ Pass if `window removed: "WinRestyleT14"` appears.
4. What the harness can't cover (verify at the manual T3 release pass):
   buttons are *visible* on the bar with app icons and ellipsized titles
   (windows without icons get text-only chips); the foreground window's chip
   is brighter and follows focus changes; hovering a chip lightens it and
   the highlight clears when the mouse leaves the bar; clicking a button
   focuses that window, clicking the focused window's button minimizes it,
   clicking a minimized window's button restores it.

## T15 — Taskbar extras: pinned, backdrop, date, bars, tray gating  (Phase 2)

No registry swap needed. Automated smoke of the Phase 2 completion slices,
unswapped: write an extras config (`backdrop = "acrylic"`, `show_date`,
one pinned app) and start a fresh pair.

1. ✅ Pass if the bar paints (`taskbar painted`), reports **exactly one
   bar** on the single-monitor VM (`taskbar up: 1 bar(s)`), and logs
   `pinned apps: 1`.
2. ✅ Pass if the backdrop path settles either way — `backdrop applied:
   Acrylic`, or `backdrop: system backdrop unavailable` on builds without
   `DWMWA_SYSTEMBACKDROP_TYPE` — and never crashes the bar.
3. **Safety assertion:** ✅ Pass if the log shows `tray host off` — the
   `Shell_TrayWnd` host must never be created while explorer's desktop is
   live (ADR 0005 amendment; two tray windows would fight for icon
   registrations).
4. The harness posts a real `WM_LBUTTONDOWN` at the center of the pinned
   square, taking the coordinates from the bar's own startup log line
   (`pinned[0] chip at x,y WxH (bar-local)`) so it never re-derives layout
   or DPI math. ✅ Pass if `pinned launch:` is logged (the app itself is
   reaped afterwards).
5. What the harness can't cover (verify at the manual T3 release pass, in a
   *swapped* session):
   - **Tray:** `tray host active` + `broadcast TaskbarCreated` in the log;
     icons of running tray apps appear right of the window buttons within
     seconds; left/right-clicking a tray icon opens that app's usual
     popup/menu; killing the app makes its icon vanish within ~2 s.
   - **Backdrop:** with `backdrop = "acrylic"` and a lower `alpha`
     (e.g. 140), the area behind the bar blurs; `"mica"` gives the flatter
     desktop-tinted look; `"none"` is today's translucent fill.
   - **Date + theming:** the date line renders under the clock;
     `text_color` recolors clock/titles/glyph.
   - **Overflow:** open enough windows to overflow the bar — a `»` chip
     appears; clicking it lists the dropped windows and picking one
     focuses it.
   - **Multi-monitor** (needs real hardware, not the VM): one bar per
     monitor, correct position/DPI per monitor; unplugging/plugging a
     monitor rebuilds the bars (`rebuilding bars (display change)`);
     windows keep working (buttons on every bar list all windows).

## T16 — Installer / manager UI  (Phase 3, ADR 0006)

> Last passed 2026-07-19 (suite 30/30 + the manual window pass below): the
> manager rendered, unchecking OneDrive + **Restyle Now** trial-ran and swapped,
> the swapped shell logged `autostart done: 2 launched, 1 disabled` (the opt-out
> round-tripped on the shared `entry_id`), and `Win + Ctrl + F1` restored the
> desktop.

The manager's safety-critical logic is unit-tested cross-platform
(`wr-core::{components,autostart,manager,config}`, `wr-installer::view`); the
window itself is Direct2D and can only be *seen* in the VM. So T16 splits:

**Automated (in the harness):**

1. `wr-shell.exe --selftest` — the trial run the installer performs before it
   ever touches the registry. ✅ Pass if it logs `selftest ok: config parsed
   (…)` and exits `0`, without spawning the wallpaper/taskbar or the safety
   harness. A non-zero exit is the installer's signal to abort the swap.

**Manual, once per release (the window):** run `wr-installer` with no arguments.

2. ✅ Pass if the manager window opens: a **Components** checklist (Taskbar,
   Wallpaper, Startup programs) with the currently-enabled ones checked, and a
   **Startup programs** list with one row per real logon entry (your HKCU/HKLM
   `Run`, `RunOnce`, and Startup-folder items), each with a checkbox reflecting
   whether it is on the config's `disabled` list. Toggling a checkbox
   lightens/repaints its row; the list scrolls (mouse wheel) when it overflows;
   the two footer buttons hover-highlight.
3. **Restyle Now** (⚠️ **snapshot first** — this swaps the shell): ✅ pass if it
   runs the trial, writes `%APPDATA%\WinRestyle\config.toml` reflecting the
   checklist + startup opt-outs, backs up + sets `HKCU Shell`, and shows the
   recovery-instructions dialog naming `Win + Ctrl + F1`. Then this collapses
   into a normal **T3**: log out/in → WinRestyle desktop → hotkey restores it.
4. **Undo / Restore**: ✅ pass if `HKCU Shell` returns to its original value
   (or is removed), our surfaces are swept, and — if no desktop shell was on
   screen — explorer comes back. Equivalent to `wr-installer restore` (T4) with
   the process sweep added.
5. Component semantics to eyeball: unchecking **Wallpaper** and re-applying
   paints the neutral default background (no custom color/image); unchecking a
   **Startup program** and re-applying makes the shell log
   `autostart: skipped <id> (disabled in config)` at the next swapped logon
   (ties back to T12).

## T17 — Start menu  (Phase 4, ADR 0007)

> Last passed 2026-07-19 (suite 32/32), and the manual half was checked live
> the same day in a **swapped session**: the menu opened over the bar
> (`start menu opened: 69 apps`), hover + scrollbar rendered, launching an
> entry logged `start menu launch: …\Computer Management.lnk`, the launched
> window got a taskbar button, and the menu closed on dismissal.

The menu is a window inside `wr-taskbar` (no new process, no IPC — ADR 0007);
clicking the Start chip opens it in every session, swapped or not. Its logic is
unit-tested cross-platform (`wr-taskbar::{apps,startmenu}`: folder merge with
user-shadows-machine, filtering, placement, scroll/selection math); the window
itself splits like the manager's:

**Automated (in the harness):**

1. The bar logs `start chip at x,y WxH (bar-local)`; the harness posts
   `WM_LBUTTONDOWN` at the chip's center. ✅ Pass if the log shows
   `start menu opened: N apps, M actions` (N > 0 on any real Windows install).
   Both the pinned-click (T15) and this one re-acquire the bar window + latest
   geometry right before posting and retry, so a mid-test display-change
   rebuild (e.g. resizing a Hyper-V enhanced session) doesn't flake them.
2. ✅ Pass if `M actions` > 0 — the built-in actions (ADR 0007 amendment). The
   release binaries run from a `target\` tree, so `dev_mode` is on and all four
   (Restore, settings, terminal, run-tests) show.
3. Esc posted to the `WinRestyleStartMenu` window. ✅ Pass if the log shows
   `start menu closed`.

**Manual (rides T3):** running an action — **Restore Windows desktop** brings
the standard desktop back mid-session (spawns `wr-installer deactivate`);
**WinRestyle settings** opens the manager; the dev actions open a terminal /
launch the suite. Actions filter and launch by keyboard like apps.

**Manual, once per release (rides T3):**

3. ✅ Pass if a real click on the Start chip shows the menu above the bar:
   a "Type to search" box and the alphabetical shortcut list (letter rows —
   icons are a later slice), with hover highlight, wheel scrolling, and a
   thumb when the list overflows.
4. Typing filters the list live; Backspace un-filters; a non-matching filter
   shows "No matches". Up/Down move the selection (scrolling to keep it
   visible); **Enter** launches the selected entry and closes the menu
   (`start menu launch: <path>` in the log). Clicking a row does the same.
5. Dismissal: Esc closes; clicking anywhere outside (another window, the
   desktop) closes; clicking the Start chip again closes without reopening.
6. In a **swapped session** (during the T3 pass): the menu opens, lists the
   same apps, and launches them — the Start experience explorer used to
   provide.

## T18 — Live activate / deactivate  (ADR 0008)

> First run (2026-07-19) half-passed: **activation worked live** — swapped
> desktop up, `WinRestyle is now your desktop`, and `Win+Ctrl+F1` restored
> explorer mid-session — but the harness hung on its own `Start-Process
> -Wait`, which waits for **descendants** too, and both verbs deliberately
> leave descendants running (activate → the watchdog family; deactivate →
> explorer; even the hotkey-relaunched explorer counts, so the wait outlived
> the restore). Fixed: the harness waits on the installer process alone.
> Full pass pending the next run.

The only automated test that swaps the running session's desktop for real —
which is why it runs **last**. It proves the no-logon path both ways:

1. `wr-installer apply` then `wr-installer activate`. ✅ Pass if explorer is
   gone, exactly one watchdog + shell + taskbar are running, and the log shows
   the swapped-mode signature `taskbar up: … topmost, tray host active` (never
   seen in the unswapped tests). Activation stops the outgoing desktop *and
   its descendant app tree* (ADR 0008 amendment — activation replaces a
   logout, which ends the session), but spares the branch that launched it —
   which is exactly why the harness PowerShell driving `activate` keeps
   running to record the result and drive step 2.
2. `wr-installer deactivate`. ✅ Pass if explorer is back, all three WinRestyle
   processes are swept (repeated sweep — mutual supervision resurrects
   single-pass survivors), and the registry backup key is gone.

If the harness dies between the two halves, its `finally` block restores the
registry and relaunches explorer. Manual notes: on a machine where winlogon
relaunches explorer (`AutoRestartShell` — ADR 0001/T5), `activate` detects the
returned desktop, backs our processes out, and reports that the restyle will
activate at the next sign-in instead — verify the message, not a hang.

## Resolved Phase 0 question

How the watchdog is launched as the shell, and whether `explorer.exe` re-adopts
the shell role mid-session: **resolved 2026-07-18** — the watchdog *is* the
registered shell, and mid-session restore works (T0–T4 pass). Findings recorded
in `docs/ARCHITECTURE.md`; liveness follow-up in
`docs/decisions/0001-watchdog-liveness.md`.

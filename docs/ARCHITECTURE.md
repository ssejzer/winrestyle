# WinRestyle Architecture

This document describes *how* WinRestyle works and *why* it is built the way it
is. It is the reference for contributors.

## Goal

Replace parts of the Windows 11 shell with modern, fast, themeable components —
without bricking the user's desktop when something goes wrong.

## Core decision: full shell replacement

Windows starts a single "shell" process at logon, defined by the registry value:

```
HKEY_CURRENT_USER\Software\Microsoft\Windows NT\CurrentVersion\Winlogon\Shell
HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon\Shell  (default: "explorer.exe")
```

By default this is `explorer.exe`, which paints the desktop/wallpaper and hosts
the taskbar, Start menu, and **system notification area (tray)**. WinRestyle
replaces this with `wr-shell.exe`.

> We set the **per-user** value (`HKCU`) only. The machine-wide `HKLM` value is
> left untouched as a fallback, and per-user changes are reversible from another
> account. We never write `HKLM\…\Shell`.

### Consequences of being the shell

When `explorer.exe` is no longer the shell, **we lose everything it provided**
and must reimplement or re-host it:

- Desktop wallpaper + desktop icons → painted by `wr-shell`.
- Taskbar + window buttons → `wr-taskbar`.
- Start menu → `wr-startmenu` (later phase).
- **System tray** — apps register icons via `Shell_NotifyIcon`, which messages a
  window of class `Shell_TrayWnd`. To receive tray icons we must create that
  window and speak the `TB_*` / `APPBAR` / copydata protocol ourselves. This is
  the hardest single piece and is scheduled explicitly (Phase 2).
- **Logon startup programs** — explorer runs the user's autostart *at shell
  start*: the `Run` / `RunOnce` keys (HKCU + HKLM) and the per-user + common
  Startup folders, plus session helpers such as `rdpclip.exe`. As the shell we
  must run these ourselves (Phase 1), with a per-entry opt-in/out. Scheduled-task
  "at logon" items are launched by Task Scheduler, not the shell, so they still
  fire on their own.

`explorer.exe` can still be launched on demand as a *file manager* without being
the shell.

## Safety architecture (the foundation)

Three independent layers, designed so no single failure leaves a blank desktop.

### 1. Watchdog process (`wr-watchdog`)

A **separate** process — not part of `wr-shell` — so it survives a shell hang or
crash. Responsibilities:

- Register the global **`Win + Ctrl + F1`** hotkey (`RegisterHotKey`). On press:
  restore the original shell value, terminate `wr-shell`, and launch
  `explorer.exe` to recover the session.
- Launch and monitor `wr-shell`. On unexpected exit, relaunch it.
- **Crash-loop fallback:** if `wr-shell` exits ≥ `N` times within `T` seconds,
  stop relaunching, restore `explorer.exe`, and surface an error.

**Decided in Phase 0:** the watchdog *is* the registered shell. `wr-installer
apply` points `HKCU\…\Shell` at `wr-watchdog.exe`, and the watchdog spawns
`wr-shell` as its child. This guarantees the emergency hotkey and supervisor are
running the moment the session starts — pointing `Shell` at `wr-shell` directly
would log the user into a blank desktop with no hotkey and no supervisor.

**The watchdog itself is supervised by the shell** (mutual supervision,
ADR 0002): the shell relaunches a dead watchdog — across generations, with a
runaway cap — and the relaunched watchdog's stray sweep converges the pair back
to exactly one of each. Hangs are detected by the pipe heartbeat (ADR 0003) and
*converted into deaths*; inside the watchdog, per-thread liveness stamps ensure
a partially hung watchdog blames itself, not the shell. Recovery is idempotent:
`explorer.exe` is only launched if no live desktop shell (`Shell_TrayWnd`) is
already on screen.

> **Concurrency invariant:** the watchdog must never hold the child-handle lock
> across a blocking `wait()`. The recovery path (hotkey / crash-loop) needs that
> same lock to terminate a *running* shell; holding it across a blocking wait
> deadlocks recovery and strands the desktop. The supervisor therefore polls
> `try_wait` and releases the lock between polls. (A violation of exactly this
> deadlocked the emergency hotkey during the Phase 0 spike before it was fixed.)

### Process tree & child surfaces (ADR 0005)

```
wr-watchdog            (registered shell: hotkey, pipe server, supervisor)
└─ wr-shell            (wallpaper, autostart, guardian threads)
   └─ wr-taskbar       (Phase 2 UI surface)
```

UI surfaces are children of the **shell**, not the watchdog: crash isolation
without entangling the safety harness. Surface supervision is deliberately
weaker — the shell relaunches a dead taskbar, and a crash-looping taskbar
makes the shell *give up on the taskbar* (logged error, nothing more). A
missing surface degrades the desktop; it is never a recovery trigger.

Because surfaces are grandchildren that outlive a killed parent, **every
recovery path sweeps them by name** (`wr_core::process::kill_all_named`):
watchdog startup and `recover()`, shell startup and its clean-shutdown paths.
An emergency restore must never leave a WinRestyle bar over the recovered
explorer desktop.

### 2. Registry backup + rollback (`wr-core`)

- On install/apply: read the current `HKCU` (or effective) `Shell` value and
  copy it to `HKCU\Software\WinRestyle\OriginalShell` before overwriting.
- On uninstall / emergency restore: write the backed-up value back (defaulting
  to `explorer.exe` if none), and remove our override.

### 3. No blind commit

The installer trial-runs `wr-shell` in the current session (as a normal window)
and only writes the registry `Shell` value after a clean run. All development
happens in a **Windows 11 VM with snapshots**.

> ✅ **Resolved in Phase 0 (2026-07-18, Win11 22H2, build 22621):** mid-session
> full-shell restore **works**. With `wr-watchdog` set as the registered shell,
> the recovery sequence — terminate the custom shell → restore the `HKCU\…\Shell`
> value → `spawn("explorer.exe")` — causes explorer to **re-adopt the shell
> role**: the taskbar and Start menu return without a re-logon. Confirmed by
> pressing the `Win + Ctrl + F1` emergency hotkey on a live swapped session. No
> controlled re-logon fallback is required for this path.

## Rendering

- **Taskbar and other always-on-screen surfaces:** native **Direct2D +
  DirectComposition**. GPU-accelerated, supports acrylic/blur/rounded corners,
  and keeps idle cost near zero — essential for something always visible.
- **Installer and (later) Start menu:** native by default; **WebView2** is an
  allowed option where development speed matters more than footprint.

## Component model

Every restyle feature is a crate implementing a shared `Component` trait
(`install` / `uninstall` / `apply` / `status`). The installer enumerates a
registry of these components to render its checklist. This is what makes the
"tick components, hit Restyle Now" UX a thin layer over real modules.

## IPC

`wr-watchdog`, `wr-shell`, and `wr-installer` coordinate over a Windows **named
pipe** (`\\.\pipe\winrestyle`) with a small serde-encoded (newline-delimited
JSON) message protocol defined in `wr-ipc` (e.g. `RequestRestore`,
`ReloadConfig`, `ShellHeartbeat`). The **watchdog hosts the pipe server**; the
shell and installer connect as clients.

The pipe also carries **hang detection** (ADR 0003): the shell heartbeats every
second, the watchdog acks, and 5 s of silence on a live channel means the peer
is hung — the observer kills it, converting the hang into a death that the
ADR 0002 mutual-supervision paths already recover from. The heartbeat layer
only detects; it never recovers.

## Configuration

User config lives in TOML under `%APPDATA%\WinRestyle\config.toml`, deserialized
via serde into `wr-core` types (`wr_core::config`). Loading can never take the
shell down: a missing file means defaults, a broken file at startup means
defaults plus a logged error, and a broken file at *reload* keeps the previous
good config. Hot reload is driven by the `ReloadConfig` IPC message (sent by the
installer once it exists; the watchdog's `--send-reload-every` test flag until
then). File-watching for live preview may come later.

Surface processes (the taskbar) load the same file themselves; the shell
forwards a reload by posting the registered `WinRestyleConfigChanged` window
message to the surface's window class (ADR 0005) — no second pipe.

## Open questions (tracked, not yet decided)

- ~~Exact mid-session shell-restore mechanism~~ — **resolved** (see above:
  restore registry → kill custom shell → launch `explorer.exe`, which re-adopts
  the shell role).
- Watchdog launch/ownership: **decided** — the watchdog is the registered shell.
- Watchdog liveness: **decided** — mutual supervision (ADR 0002; Winlogon's
  `AutoRestartShell` proved not to apply to custom per-user shells) plus
  heartbeat-based hang detection over the pipe (ADR 0003).
- Tray hosting completeness vs. effort (full `Shell_TrayWnd` protocol
  coverage). Hard prereq recorded in ADR 0005: `desktop_shell_running()`
  detects a live desktop via the `Shell_TrayWnd` class, so once *we* create
  one, recovery must be able to tell ours from explorer's.
- Multi-monitor + DPI strategy for the taskbar.
- Code signing / Defender + SmartScreen mitigation before public release.

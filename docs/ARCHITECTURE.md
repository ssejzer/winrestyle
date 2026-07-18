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

> **Concurrency invariant:** the watchdog must never hold the child-handle lock
> across a blocking `wait()`. The recovery path (hotkey / crash-loop) needs that
> same lock to terminate a *running* shell; holding it across a blocking wait
> deadlocks recovery and strands the desktop. The supervisor therefore polls
> `try_wait` and releases the lock between polls. (A violation of exactly this
> deadlocked the emergency hotkey during the Phase 0 spike before it was fixed.)

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
via serde into `wr-core` types. File-watching enables live preview / hot reload.

## Open questions (tracked, not yet decided)

- ~~Exact mid-session shell-restore mechanism~~ — **resolved** (see above:
  restore registry → kill custom shell → launch `explorer.exe`, which re-adopts
  the shell role).
- Watchdog launch/ownership: **decided** — the watchdog is the registered shell.
- Watchdog liveness: **decided** — mutual supervision (ADR 0002; Winlogon's
  `AutoRestartShell` proved not to apply to custom per-user shells) plus
  heartbeat-based hang detection over the pipe (ADR 0003).
- Tray hosting completeness vs. effort (full `Shell_TrayWnd` protocol coverage).
- Multi-monitor + DPI strategy for the taskbar.
- Code signing / Defender + SmartScreen mitigation before public release.

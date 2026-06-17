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

The watchdog is itself launched by the shell entry, but ordering/ownership is a
Phase 0 design question (see Open Questions).

### 2. Registry backup + rollback (`wr-core`)

- On install/apply: read the current `HKCU` (or effective) `Shell` value and
  copy it to `HKCU\Software\WinRestyle\OriginalShell` before overwriting.
- On uninstall / emergency restore: write the backed-up value back (defaulting
  to `explorer.exe` if none), and remove our override.

### 3. No blind commit

The installer trial-runs `wr-shell` in the current session (as a normal window)
and only writes the registry `Shell` value after a clean run. All development
happens in a **Windows 11 VM with snapshots**.

> ⚠️ **Open question validated in Phase 0:** restoring the *full* explorer shell
> mid-session (vs. at logon) behaves differently — launching `explorer.exe` when
> a custom shell is set may run it as a file browser rather than re-adopting the
> shell role. The Phase 0 spike's entire job is to nail down the reliable
> restore mechanism (likely: restore registry → terminate custom shell → let
> Winlogon/explorer re-take the shell, or force a controlled re-logon).

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
pipe** (`\\.\pipe\winrestyle`) with a small serde-encoded message protocol
defined in `wr-ipc` (e.g. `RestoreShell`, `ReloadConfig`, `ShellHeartbeat`).

## Configuration

User config lives in TOML under `%APPDATA%\WinRestyle\config.toml`, deserialized
via serde into `wr-core` types. File-watching enables live preview / hot reload.

## Open questions (tracked, not yet decided)

- Exact mid-session shell-restore mechanism (Phase 0 spike).
- Watchdog launch/ownership model and how it is itself kept alive.
- Tray hosting completeness vs. effort (full `Shell_TrayWnd` protocol coverage).
- Multi-monitor + DPI strategy for the taskbar.
- Code signing / Defender + SmartScreen mitigation before public release.

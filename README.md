# WinRestyle

> Make Windows 11 beautiful, customizable, and fast — open source.

WinRestyle replaces parts of the Windows 11 shell (starting with the taskbar)
with modern, GPU-accelerated, fully themeable components. Install it, tick the
components you want, hit **Restyle Now** — that's it.

> [!WARNING]
> **Pre-alpha / Phase 0.** WinRestyle performs a **full shell replacement**: it
> swaps the Windows shell for its own. This is powerful but invasive — a bug can
> leave you with a blank desktop. **Only run it inside a disposable Windows 11
> VM with snapshots** until we say otherwise. See [Safety](#safety) below.

## What it does

- 🎨 **Restyle** the taskbar (Start menu, theming engine, and more to come).
- ⚡ **Native & fast** — Rust + Direct2D/DirectComposition, no Electron, no bloat.
- 🧩 **Modular** — pick components to install/uninstall from one screen.
- 🛟 **Recoverable** — emergency restore hotkey and automatic rollback built in.

## Safety

Because WinRestyle becomes *the shell*, safety is the foundation, not an
afterthought. Three independent guarantees:

1. **Watchdog process** — a separate guardian (`wr-watchdog`) owns the
   **`Win + Ctrl + F1`** emergency-restore hotkey, monitors the shell, and
   auto-restores `explorer.exe` if the shell crash-loops.
2. **Registry backup + rollback** — we set the **per-user** shell
   (`HKCU\…\Winlogon\Shell`) only, never machine-wide, and stash the original
   value so uninstall/restore is always possible.
3. **No blind commit** — the installer trial-runs the new shell before writing
   the registry, and dev happens in a snapshotted VM.

If something goes wrong: press **`Win + Ctrl + F1`**. If that fails, log out and
back in (the per-user shell change can be reverted from another admin account),
or restore your VM snapshot.

## Architecture

A Cargo workspace of focused crates:

| Crate          | Role                                                            |
| -------------- | -------------------------------------------------------------- |
| `wr-core`      | Shared types, config schema, shell-registry backup/restore.    |
| `wr-ipc`       | Named-pipe protocol between watchdog ⇄ shell ⇄ installer.       |
| `wr-watchdog`  | Guardian process: emergency hotkey, crash-loop fallback.       |
| `wr-shell`     | Shell host: paints desktop, hosts the system tray, lifecycle.  |
| `wr-taskbar`   | The flagship taskbar UI (Direct2D/DirectComposition).          |
| `wr-installer` | Manager UI: component checklist + **Restyle Now** / uninstall. |

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) and
[`docs/ROADMAP.md`](docs/ROADMAP.md) for the full design and phased plan.

## Building

WinRestyle targets **Windows 11 (x86_64)** and builds with the **MSVC**
toolchain. You cannot meaningfully run it on Linux/macOS (it links Windows shell
APIs), though you can edit and check it anywhere.

```powershell
# On Windows 11, with Rust (https://rustup.rs) and the MSVC build tools:
rustup target add x86_64-pc-windows-msvc
cargo build --workspace
```

## Status

🚧 **Phase 0 — safety harness & shell-swap spike.** No UI yet; the goal is to
prove the swap → crash → restore loop is bulletproof. Follow the
[roadmap](docs/ROADMAP.md).

## License

Licensed under the [Apache License, Version 2.0](LICENSE).

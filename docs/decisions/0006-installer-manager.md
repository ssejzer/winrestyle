# ADR 0006 — The installer / manager UI (Phase 3)

Date: 2026-07-19. Status: accepted.

## Context

Phase 3 ships the target UX: a one-screen manager to choose which components to
restyle, opt individual logon-startup programs in or out, and apply/undo the
shell swap safely. Before this, the only front door was the `wr-installer` CLI
(`status`/`apply`/`restore`) that Phases 0–2 used to drive the safety harness by
hand.

Two forces shape the design. First, the safety model: applying the restyle
means swapping the per-user shell, the single most dangerous thing this project
does, so the apply path must stay as auditable as the CLI it replaces. Second,
the dev-machine constraint: the binaries only run in the VM, so any logic that
lives in the UI process can only be *seen*, never *tested*, until a manual T3
pass. Phases 1–2 already answered that tension — put the real logic in
cross-platform, unit-tested `wr-core`, keep the Windows surface thin — and
Phase 3 follows the same shape.

## Decisions

1. **UI toolkit: custom Direct2D, consistent with the taskbar.** Not native
   Win32 controls, not a GUI framework (egui/…). The project renders its
   surfaces itself; the manager does too, so there is one rendering idiom to
   maintain and the Phase 4 start menu / theming UI inherit it. Concretely:

   - **`view.rs` is pure geometry + hit-testing** (rows, section headers,
     footer buttons, scroll math), no Win32 — so it unit-tests on the Linux dev
     host exactly like the taskbar's `layout.rs`. This is where the UI's real
     logic-under-test lives.
   - **`render.rs` uses a Direct2D `ID2D1HwndRenderTarget`, not the taskbar's
     DirectComposition swapchain.** The manager is an ordinary *opaque* window;
     composition only buys per-pixel translucency, which it does not need. Same
     Direct2D + DirectWrite primitives, far less plumbing (no D3D/DXGI/DComp).
     The target is pinned to 96 DPI so one unit is one pixel and all DPI scaling
     stays in `view.rs`.
   - **`app.rs` is the window + message pump**, following the taskbar's one
     concurrency rule: never hold the `STATE` `RefCell` borrow across anything
     that pumps messages (`MessageBoxW`'s modal loop, the apply's spawned trial
     process). Snapshot under a short borrow, drop it, act, re-borrow.

2. **All safety-critical logic lives in `wr-core`, cross-platform and
   unit-tested.**

   - `components` — the `Component` trait (`install`/`uninstall`, an
     `is_installed` query) and a `Registry` whose `apply(base, selected)`
     produces the config a checklist selection describes. Pure config
     transformation; the manager renders one row per component and never
     special-cases them.
   - `autostart` — the *entry model*: stable id constructors (`entry_id`,
     `Source::prefix`) and enumeration. The shell's launch filter and the
     manager's per-entry checkboxes now build ids through the **same**
     function, so a `[autostart].disabled` id can never mean one thing to the
     writer and another to the reader. The shell's `autostart::enumerate` was
     switched onto these constructors; a unit test pins the exact historical id
     strings so that refactor changed no behavior.
   - `manager` — the safe-apply sequence and the recovery-instructions text
     (one source of truth for the emergency hotkey + restore command).
   - `config` — gained TOML *write* support (atomic temp-file + rename) and a
     `Wallpaper.enabled` switch with `effective_color`/`effective_image`, so
     "wallpaper" is a real toggleable component. Off paints the neutral default
     rather than leaving the (explorer-less) desktop black.

3. **Safe apply, in this order, to preserve "we can always put explorer back":**
   preflight (all sibling binaries present) → **write config** → **trial run**
   (`wr-shell --selftest`: loads/validates config, exits 0, spawns no surfaces —
   proves the binary runs on this machine while explorer is still the shell and
   a failure costs nothing) → back up + swap (only now touch the registry, and
   only after the byte-for-byte backup `wr-core::shell` already provides) → show
   recovery instructions, every time. A failure before the swap step leaves the
   registry untouched.

4. **The manager does NOT talk to the running shell/watchdog over IPC.** The
   `wr-ipc` pipe server is `MAX_INSTANCES = 1` and the shell holds it
   permanently for its heartbeat; a second client cannot connect without
   reworking the server into a multi-client one — and that server carries the
   delicate T8/T9 hang-detection logic we've already lost races to twice. So the
   manager edits `config.toml` and does the registry swap/restore **directly**,
   with changes taking effect at the next logon. This is the honest installer
   model (you log out/in to change your shell) and it reuses only proven
   primitives: `shell::{backup_and_set_shell,restore_shell,desktop_shell_running}`
   and `process::kill_all_named`. Undo restores the registry, sweeps our
   surfaces, and relaunches explorer only if no desktop shell is already on
   screen — the same idempotence rule as the watchdog's `recover()`.

   **Live apply-over-IPC is deferred** until the pipe server is made
   multi-client (a Phase 4+ item). Until then the emergency hotkey remains the
   live safety path in a swapped session.

5. **The CLI stays.** `status`/`apply`/`restore` are unchanged (T0/T4 depend on
   their exact behavior); no-args now opens the manager window. The CLI is the
   scriptable back door, the GUI the front door.

## Validation

- Unit tests: `wr-core::{components,autostart,manager,config}` (registry apply,
  id-format pinning, preflight/recovery text, config write round-trip,
  wallpaper effective values) and `wr-installer::view` (layout, scroll
  clamping, hit-testing incl. scrolled content and footer exclusion) — green on
  the host and type-checked on `x86_64-pc-windows-msvc`.
- Automated **T16** (VM harness): `wr-shell --selftest` validates config and
  exits 0 — the trial-run primitive the swap depends on. Suite **30/30**,
  2026-07-19.
- **Manual T3 — passed 2026-07-19.** The manager window rendered correctly
  (`manager window up (544x641, dpi 96)`): component checklist, and the real
  logon entries listed with their source labels (SecurityHealth `Run (machine)`,
  OneDrive `Run (user)`, MicrosoftEdgeAutoLaunch `Run (user)`). Unchecking
  OneDrive and clicking **Restyle Now** ran the full sequence — trial
  `--selftest` (pid logged), config written, `HKCU Shell` backed up + set, the
  recovery dialog naming `Win + Ctrl + F1`. At the swapped logon the shell logged
  `autostart: skipped hkcu-run:OneDrive (disabled in config)` →
  `autostart done: 2 launched, 1 disabled, 0 failed` — the manager checkbox
  round-tripped through config into the shell's launch filter on the *same*
  `entry_id`, confirming the no-drift contract end-to-end. `Win + Ctrl + F1`
  restored the standard Windows desktop.

## Consequences

The `Wallpaper.enabled` field is additive and defaults to `true`, so existing
configs and the validated Phase 1 wallpaper behavior are unchanged. The shell's
autostart id refactor is behavior-preserving (pinned by test). Nothing in the
watchdog or the IPC layer changed, so the Phase 0–2 safety validation still
stands unmodified.

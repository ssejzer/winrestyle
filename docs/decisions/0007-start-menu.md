# ADR 0007 — The start menu (Phase 4, first slice)

Date: 2026-07-19. Status: accepted.

## Context

Phase 4 opens with the start menu. The taskbar's Start button has been a stub
since Phase 2 (it tapped the Win key, which opens the system Start menu
unswapped and lands on *nothing* in a swapped session) — the most visible hole
in the daily-driver experience. The roadmap sketched the menu as a separate
`wr-startmenu` binary; this ADR revisits that under the constraints the project
has since accumulated.

Two constraints dominate. First, ADR 0006 §4: the `wr-ipc` pipe is
single-client, permanently held by the shell's heartbeat, and carries the
delicate T8/T9 hang-detection logic — nothing new may ride it until it is
reworked to multi-client. A separate menu process would need exactly such a
channel just to hear "the Start button was clicked". Second, the dev-machine
split: logic must live in pure, cross-platform modules to be tested at all;
Windows code can only be *seen* at a manual T3.

## Decisions

1. **The start menu is a module of `wr-taskbar`, not a separate process.** The
   Start click just shows a window owned by the same thread that owns the bars:
   no IPC, no new supervision surface, no new binary to sweep. The menu
   inherits the taskbar's safety posture — it is cosmetic, a crash takes down
   `wr-taskbar` and the shell's proven relaunch/crash-loop-give-up policy (ADR
   0005) covers it. The roadmap's `wr-startmenu` name survives as the feature
   name, not a process name. Revisit only if the menu ever needs a different
   process lifetime than the bar (nothing planned needs that).

2. **Clicking Start opens *our* menu in every session, swapped or not.** The
   Win-key-tap stub is gone. Unswapped, this shadows the system menu for our
   bar's users — which is the product working as intended, and it is what makes
   the menu automatable: the unswapped VM suite can post a click at the Start
   chip and drive the menu end to end (T17), where the old stub was only
   manually observable.

3. **App discovery is pure `std::fs` (`apps.rs`), unit-tested on the dev
   host.** The menu lists the union of the user and machine Start Menu
   `Programs` folders (`%APPDATA%\…` and `%ProgramData%\…`), walked recursively
   for `.lnk`/`.url`, user entries shadowing machine entries with the same
   relative path (explorer's merge rule), sorted case-insensitively. Reading
   `%ProgramData%` is read-only and touches no registry — the HKLM invariant is
   about *writes*. The list is re-scanned on every open (a local walk of a few
   hundred shortcuts; freshness beats caching), and launching goes through the
   same `ShellExecuteW` path as pinned apps.

4. **Menu geometry and interaction state are pure (`startmenu.rs`),
   unit-tested** — the `layout.rs`/`view.rs` shape for the third time: menu
   placement above the bar, row layout under a scroll offset, scrollbar thumb
   math, hit-testing, and a `MenuState` (type-to-filter buffer, selection,
   scroll) whose keyboard/wheel transitions are all plain functions.

5. **The window is the taskbar's rendering idiom, activatable.** Same
   D2D-on-DirectComposition `Renderer` as the bars (translucent, WARP
   fallback); the window class is `WinRestyleStartMenu` (the `WinRestyle`
   prefix keeps it out of `winlist`'s button rules, and it is not
   `Shell_TrayWnd`, so ADR 0005's recovery logic is untouched). Unlike the
   bars it is **not** `WS_EX_NOACTIVATE`: it takes focus so typing filters and
   Esc/Enter work, and losing activation is what dismisses it. Because the
   bars are no-activate, clicking the Start chip while the menu is open does
   *not* deactivate it — the click handler sees it visible and toggles it
   closed (plus a short re-open debounce for paths that do deactivate first).
   Known limit, accepted for this slice: if Windows refuses us foreground
   (posted/synthetic clicks, focus lock), the menu opens without focus —
   mouse still works, keyboard and click-away dismissal don't. The T3
   checklist covers the real-input behavior.

6. **Deliberately out of this slice:** per-entry icons (`SHGetFileInfoW` on
   every shortcut can block on network-target `.lnk`s; rows draw letter chips
   like pinned fallbacks until an async icon loader exists), opening via the
   Win key (needs a keyboard hook), pinned/recent sections, power controls
   (Ctrl+Alt+Del remains the shutdown path in a swapped session), and any new
   config section (the menu derives its theme from `[taskbar]`, with a raised
   opacity floor for readability).

## Validation

- Unit tests: `wr-taskbar::apps` (merge/shadowing, extension and depth rules,
  sorting, filtering) and `wr-taskbar::startmenu` (placement incl. clamping,
  row/scroll/scrollbar math, hit-testing, selection/filter state machine) —
  green on the host and type-checked on `x86_64-pc-windows-msvc`.
- Automated **T17** (VM harness, unswapped): the bar logs the Start chip's
  geometry; the harness posts `WM_LBUTTONDOWN` at it and asserts
  `start menu opened: N apps`, then posts Esc to the menu window and asserts
  `start menu closed`. **Suite 32/32, 2026-07-19.**
- **Manual — passed 2026-07-19, in a live swapped session:** the menu rendered
  above the bar (search hint, hover highlight, scrollbar), listed 69 apps from
  both Programs roots, filtered, and launching an entry worked end to end
  (`start menu launch: …\Computer Management.lnk`, the window got a taskbar
  button, the menu dismissed). Open/close logging matched the T17 signature.

## Amendment (2026-07-19) — built-in actions

The menu gained a small **actions** section above the scanned apps (`actions.rs`,
pure + unit-tested like `apps.rs`): WinRestyle commands the user would otherwise
need a terminal or the emergency hotkey for. `Win+Ctrl+F1` stays the *emergency*
restore; these are the calm, discoverable equivalents.

- **Restore Windows desktop** and **WinRestyle settings** — always shown.
  Restore spawns `wr-installer deactivate` **detached** (`ShellExecuteW`), so it
  runs outside the WinRestyle family and survives the taskbar's own teardown
  when the sweep kills us (ADR 0008); settings spawns the manager window.
- **Open terminal here** and **Run VM test suite** — **dev-gated**: shown only
  when the running exe sits under a `target\` tree (`winlist::dev_mode`), never
  in a shipped install. "Run tests" passes `-SkipBuild` (the running `.exe`s are
  locked, so a swapped-session rebuild can't relink) and `-NoExit`; a literal
  "Rebuild" action was dropped for the same lock reason — rebuild from the
  terminal after a Restore.

Actions and apps share one filtered/selected/scrolled list (actions first, then
apps), so type-to-filter, arrows, and Enter treat them uniformly; the renderer
marks action rows with a resting chip and a divider. Dispatch and process
spawning live in `bar`/`winlist`; `actions.rs` stays pure. No config section
was added (the dev gate is a path check, not a setting).

## Amendment 2 (2026-07-19) — grouping + icons

The flat "actions then apps" list became **grouped under headers** — `Admin`
(Restore, settings) and `Dev` (terminal, tests) for the actions, `Apps` for the
scanned shortcuts — because the whole point of the actions is that they are a
*different kind* of entry, and unlabelled dividers didn't say so. Headers are
dim uppercase labels with a hairline rule, non-interactive: the menu content is
now a single `Vec<MenuEntry>` (`Header | Action | App`) with a parallel
selectable mask, so arrows skip headers and a filter that empties a group drops
its header too. The header-skipping selection and group-building are pure and
unit-tested (`startmenu::move_selection_skipping`/`first_selectable`, `bar`'s
`build_content` via the `actions`/`apps` filters).

Each row gained an **icon column**: actions draw a Unicode symbol glyph
(`↺ ⚙ ❯ ✓`) via DirectWrite — font fallback resolves them, so no fragile
hand-drawn paths or private-use icon-font codepoints — and apps draw a
first-letter chip there for now. That column is reserved on every item row so
real app icons (a background `SHGetFileInfoW` loader — a `.lnk` target can block
on a network path, so it must stay off the UI thread) slot in next with no
layout change. Consistent with the north star: grouped, glanceable, and a step
past a plain Windows list rather than a copy of it.

## Consequences

The Start button's behavior changes (our menu instead of the Win-key tap) —
no test depended on the stub. Nothing in the watchdog, shell, IPC layer, or
recovery paths changed; Phase 0–3 validation stands. The menu adds the first
keyboard-focused surface to the taskbar process, bringing `TranslateMessage`
into its pump (harmless for the existing bars, required for `WM_CHAR`).

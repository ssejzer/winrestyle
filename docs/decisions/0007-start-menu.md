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
  `start menu closed`.
- **Manual, next T3:** the menu's look, type-to-filter, Enter/click launching,
  click-away dismissal, and behavior in a swapped session.

## Consequences

The Start button's behavior changes (our menu instead of the Win-key tap) —
no test depended on the stub. Nothing in the watchdog, shell, IPC layer, or
recovery paths changed; Phase 0–3 validation stands. The menu adds the first
keyboard-focused surface to the taskbar process, bringing `TranslateMessage`
into its pump (harmless for the existing bars, required for `WM_CHAR`).

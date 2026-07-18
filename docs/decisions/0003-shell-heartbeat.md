# ADR 0003 — ShellHeartbeat: hang detection over the `wr-ipc` pipe

- **Status:** Accepted (2026-07-18)
- **Builds on:** [ADR 0002](0002-mutual-supervision.md) (mutual supervision),
  closing its two accepted gaps: hung-process detection and the PID-reuse race.

## Context

After ADR 0002, either process *dying* is detected (process-handle waits) and
recovered (mutual relaunch + crash-loop caps) — all VM-verified. Two gaps
remained, both acknowledged in ADR 0002:

1. **Hangs.** A process that is alive but wedged signals nothing. A hung
   watchdog is the worst case: the emergency hotkey and shell supervision are
   silently dead. (Exactly this happened during Phase 0 — the supervisor
   deadlock — and was only caught by manual testing.)
2. **PID reuse.** The shell watches the watchdog via a PID passed at spawn
   time; if the watchdog dies in the instant before the shell opens the
   handle, the PID could be recycled to an unrelated process.

## Decision

A heartbeat over the named pipe (`\\.\pipe\winrestyle`), with the policy:
**the heartbeat layer only detects; recovery stays with the proven death
paths.** A detected hang is *converted into a death* by killing the hung
process — then supervision, mutual relaunch, and the crash-loop caps from
Phase 0 / ADR 0002 take over unchanged. No second recovery mechanism exists.

- **Roles:** the watchdog hosts the pipe server (session root, most stable
  lifetime); the shell — later the installer — are clients.
- **Wire format:** newline-delimited JSON (`serde_json`), one message per
  line; unknown messages are logged and skipped so version skew can't take
  the channel down.
- **Cadence:** shell sends `ShellHeartbeat{seq, pid}` every **1 s**; watchdog
  answers each with `HeartbeatAck{seq, pid}`. Timeout is **5 s** (five missed
  beats) — tolerant of scheduling stalls, and recovery still lands within a
  few seconds.
- **Watchdog side:** no heartbeat for 5 s *from a shell that has heartbeated
  before* → kill the shell; the supervisor reaps the exit, relaunches, and
  crash-loop-accounts it as usual. A shell that never connects is degraded
  but alive and is never killed for silence.
- **Shell side:** no ack for 5 s *on a live pipe* → the watchdog is hung with
  a dead hotkey → kill it, using the PID from its acks (authoritative, which
  is what removes the PID-reuse race); the ADR 0002 monitor thread then
  relaunches it under the existing runaway cap. A *broken* pipe means a dead
  watchdog — that's the monitor thread's job, the heartbeat loop just
  reconnects to the successor.
- **Transport:** blocking pipe with non-blocking reads via `PeekNamedPipe`
  polling (200 ms) — the same idiom as the supervisor's `try_wait` loop; no
  overlapped I/O. One pipe instance in Phase 1; the installer later means
  bumping `MAX_INSTANCES` and a thread per connection.
- Command messages (`RequestRestore`, `ReloadConfig`, `Shutdown`) ride the
  same pipe; `RequestRestore` is wired to the emergency-restore path now, the
  rest are protocol placeholders for Phase 1 config work.

## Verification (VM)

- **T8 — hung shell:** `wr-shell --hang-heartbeat-after=N` keeps the process
  alive but silent; the watchdog must kill and relaunch it within ~6 s.
- **T9 — hung watchdog:** `wr-watchdog --ack-hang-after=N` freezes the pipe
  server; the shell must kill the watchdog, the monitor relaunches it, and
  the pair converges (sweep) with a working hotkey.

## Consequences

- Every liveness failure mode of either process — crash, kill, hang — now has
  a detection signal and a single, already-tested recovery path.
- Residual accepted risk: both processes failing at once (unchanged), and a
  hung shell that never connected to the pipe is not detected until Phase 1
  makes the shell's first pipe connection mandatory.
- The 200 ms polling threads are a deliberate simplicity trade-off; revisit
  only if idle-cost measurements ever say so.

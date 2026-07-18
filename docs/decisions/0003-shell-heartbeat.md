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

## Amendment: partial hangs inside the watchdog (found by T9, 2026-07-18)

The first automated T9 run exposed a flaw in the design above: the watchdog is
multi-threaded, and a hang can wedge *some* of its threads. Two failure modes:

1. **Pipe thread hung, supervisor alive.** The supervisor's "shell went
   silent" signal is produced by the pipe thread — so when that thread is the
   hung one, the supervisor blamed the shell and killed the innocent party
   (one second before the shell's own detection would have fired), and the
   frozen server then blocked the successor shell with `pipe busy`. Observed
   live in T9.
2. **Supervisor hung, pipe thread alive** (the shape of the actual Phase 0
   deadlock). The pipe thread would keep acking, so the shell would see a
   "healthy" watchdog whose hotkey recovery is dead. Nothing would detect it.

Fix, same principle extended *inside* the watchdog — a thread may only vouch
for what it can verify:

- The supervisor and pipe threads each maintain a liveness stamp.
- The supervisor treats stale shell heartbeats as evidence against the shell
  **only if the pipe thread's stamp is fresh**; if both are stale, the
  watchdog itself is compromised and **exits** (convert own hang to death —
  the shell's monitor relaunches a fresh watchdog).
- The pipe thread **withholds acks while the supervisor's stamp is stale** —
  an ack vouches for the whole watchdog, so a wedged supervisor must make the
  watchdog *look* hung to the shell, which then kills and replaces it.

Remaining accepted gap: a hang confined to the main message loop alone (hotkey
dead, supervision and heartbeats fine). No cheap observer exists for a thread
whose job is to block in `GetMessageW`; revisit if it ever occurs in practice.

## Verification (VM)

Automated in `scripts\vm-test.ps1`. **Both pass** (2026-07-18, Win11 22H2,
suite green 11/11). Getting there took three runs: automated T9 found the
partial-hang flaw (amendment above), then the timing race in its first fix —
the staleness-vs-staleness comparison loses by up to a beat interval, hence
the 2 s *freshness* bound (`PIPE_OBSERVING_BOUND`); automated T7 found the
one-shot monitor gap (ADR 0002 amendment).

- **T8 — hung shell:** `wr-shell --hang-heartbeat-after=N` keeps the process
  alive but silent; the watchdog must kill and relaunch it within ~6 s.
- **T9 — hung watchdog:** `wr-watchdog --ack-hang-after=N` freezes the pipe
  server; the watchdog must be replaced by a fresh one and the pair must
  converge with a working hotkey. Either resolution is a pass: the watchdog
  self-exits (pipe-wedged check, usually wins the race) or the shell kills it
  (ack timeout).

## Consequences

- Every liveness failure mode of either process — crash, kill, hang — now has
  a detection signal and a single, already-tested recovery path.
- Residual accepted risk: both processes failing at once (unchanged), and a
  hung shell that never connected to the pipe is not detected until Phase 1
  makes the shell's first pipe connection mandatory.
- The 200 ms polling threads are a deliberate simplicity trade-off; revisit
  only if idle-cost measurements ever say so.

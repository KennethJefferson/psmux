# Spec: Deterministic agent orchestration in psmux (cmux-style events, hooks, and signaling)

Status: awaiting user approval
Date: 2026-07-11
Review provenance: architecture derived from behavioral study of manaflow-ai/cmux (AGPL-3.0;
no code ported). Round-1 adversarial review by GPT Sol Ultra (gpt-5.6-sol, ultra effort)
produced 20 blocking findings; all are incorporated below except two explicitly scoped-out
items recorded in "Deferred by decision". Round-2 re-grade was skipped by user decision
("continue; worry about the other features later if they present a problem").

## 1. Goal

Let an orchestrator agent OUTSIDE psmux (Claude Code, Codex, etc.) and lead agents INSIDE
panes drive multi-agent workflows deterministically through the application itself:

```powershell
$cur = psmux cursor                                   # commitment point
psmux send-keys -t %1 'claude --dangerously-skip-permissions "task"' Enter
psmux wait-event --pane %1 --name agent-done --after $cur --timeout 600000   # push, no polling
psmux capture-pane -t %1 -p --settle 400              # read the completed screen
```

Workers' turn-completion fires automatically via installed agent hooks. No PowerShell
runspaces, no capture-pane polling loops, no sentinel-string scraping, no skill required at
runtime. Every link is an in-app Rust feature.

## 2. Threat model (explicit)

v1 trusts all processes of the same Windows user at the same integrity level (tmux posture).
Identity tokens exist for DETERMINISM (stale-signal rejection), not authentication.
- Env vars are attribution hints; the server validates in-pane calls against server-minted
  instance state and rejects mismatches (publishing a `stale-notify` event).
- The server refuses to start elevated unless `PSMUX_ALLOW_ELEVATED=1`, closing the
  medium-to-elevated escalation path for all commands including the new stream.
- New code paths get bounded line framing and a pre-auth deadline on connections.
- Known pre-existing transport issues (TCP+keyfile readable across integrity boundaries;
  `Local\` mutex vs profile-global registry files across console/RDP logon sessions) are
  documented as a separate hardening workstream (named pipe + DACL +
  GetNamedPipeClientProcessId), not part of this feature.

## 3. Identity model

Server-minted, never trusted from clients:

| Token | Minted when | Properties |
|---|---|---|
| `session_uid` | real session created / warm server claimed | UUID, immutable across rename |
| `bus_id` | same moment | UUID; event `seq` is scoped to it |
| `pane` | pane creation | slot id `%N` (tmux-compatible) |
| `pane_instance` | every spawn AND respawn-pane into a slot | monotonic generation; exported as `PSMUX_PANE_INSTANCE` |
| `turn_id` | optional | passthrough of the agent's native session/turn id from hook payloads |

- Every event carries `{session_uid, bus_id, seq, pane, pane_instance?, name, category,
  source, occurred_at, payload}`.
- Cursors are the triple `{session_uid, bus_id, seq}`; any component mismatch is an explicit
  gap error with a distinct exit code — never silent resumption.
- A delayed hook from a pane's previous process carries the old `pane_instance` and is
  rejected as stale.
- Warm servers: the bus is dormant until claim; no events from `__warm__` construction; no
  state keyed by mutable session names. Hook-driven agent panes require cold spawn (warm
  panes lack instance env; their notifies fail validation — documented).
- Popup panes are excluded from the event/identity system in v1 (documented).
- Session rename does not reset the bus.

## 4. Event bus (new `src/events.rs`)

In-memory only in v1; disk JSONL log is increment 2 with its own recovery/rotation/flush/
ACL/retention spec.

- Single sequencer: all publishes route through the main state loop and are emitted only
  AFTER the corresponding state mutation commits. Connection threads never mint `seq`.
- Producers (v1): pane lifecycle (spawned/exited — transition records capture
  `pane_instance`, exit status, reason BEFORE identity teardown), window/session lifecycle,
  bell, `agent-done` / `agent-notify` / `stale-notify` (from hooks and `psmux notify`),
  `bus-closed`. Send/paste producers are explicitly NOT in v1.
- Subscriber registration under the bus lock: capture cutoff = current seq, register live
  queue, release. Writer emits ack frame → replay (filtered, seq ≤ cutoff, streamed from
  the 4096-event ring) → live events (seq > cutoff) from a 1024-slot bounded queue. Filters
  apply before enqueue. Replay never passes through the live queue.
- Limits enforced before fanout: 16 KiB serialized event cap (oversize → payload replaced
  with `{truncated:true}` + counters), per-pane and global publish rate limits, max
  subscriber count, bounded per-subscriber socket-write deadline with cancellation. Slow
  consumers are closed with a terminal `slow_consumer` error frame; others unaffected.
- Ack frame: `{session_uid, bus_id, oldest_seq, latest_seq, replay_count, gap, gap_reason}`.
- Heartbeat frame every 15 s (client-suppressible).
- Shutdown: one idempotent server-shutdown path (used by kill-server and last-session exit;
  existing direct `std::process::exit` sites in server code are routed through it):
  publish terminal `bus-closed`, stop accepts, close subscriber queues with terminal
  reason, join writers with deadline, then exit. Event subscribers do not keep exit-empty
  sessions alive. Hard kill/panic excluded (documented).

## 5. CLI verbs and the synchronization contract

- `psmux events [--name X]... [--category Y]... [--after '<session_uid>:<bus_id>:<seq>']
  [--no-heartbeat] [--json]` — long-lived subscribe stream (ack/replay/live/heartbeat/
  terminal frames as JSON lines). Terminal frames (`bus-closed`, `slow_consumer`) are
  distinguishable from transport loss by exit code. `--cursor-file`/`--reconnect` are
  increment 2 (alongside the disk log).
- `psmux cursor [--json]` — prints the current `{session_uid, bus_id, seq}` cursor.
- `psmux wait-event --pane %N [--instance G] [--name agent-done] --after <cursor>
  --timeout <ms> [--json]` — ONE-SHOT: subscribes with the atomic cutoff contract, scans
  the replay window after `<cursor>` (catching completion-before-wait), then blocks.
  Exit 0 + event JSON on match; distinct nonzero codes for timeout / cursor gap /
  bus mismatch / session-ended.
- The race-free dispatch recipe (cursor → dispatch → wait-event) is THE documented
  contract; there is no subscribe-before-dispatch ordering requirement because the cursor
  is the commitment point.
- `agent-done` is SEMANTIC completion only, never a screen barrier. New:
  `psmux capture-pane --settle <quiet-ms> [--settle-timeout <ms>]` blocks until the pane's
  parser watermark has been idle for `quiet-ms`, then captures; on settle-timeout it
  captures anyway and exits nonzero.
- `psmux notify [--done | --name <event>] [--title <t>] [--include-content]` — in-pane,
  self-identifies via `PSMUX_PANE_INSTANCE`. Reliability contract: BOUNDED SYNCHRONOUS —
  waits for bus-commit acknowledgement with a 2000 ms deadline, then exits 0 regardless
  (never blocks the caller beyond the bound; orchestrator `--timeout` is the documented
  backstop for the lossy tail).

## 6. Turn-complete signaling and hook installers

- Hook command is the psmux binary itself — no `.cmd`/`.ps1` wrappers, no quoting or
  execution-policy surface: `psmux.exe hook-notify <agent> <event>` reads the agent's JSON
  payload on stdin, gates on `PSMUX_PANE_INSTANCE` presence (outside psmux: print `{}`,
  exit 0), honors `PSMUX_HOOKS_DISABLED=1`, applies the bounded-sync contract, always
  satisfies the agent's hook protocol.
- Routing: explicit flags > validated env identity > reject. Never falls back to the
  focused/active pane; unresolvable → no-op success.
- v1 ships ONE verified integration: Claude Code. `psmux hooks install claude
  [--user|--project-local]` writes `Stop` (+ `SessionStart`/`SessionEnd`) hook entries into
  Claude settings; default `--user`; `--project-local` writes `.claude/settings.local.json`
  and never silently commits executable config into a repo.
- Installer safety: file lock + reread-under-lock, semantic JSON merge touching only
  entries whose command matches the psmux marker, same-directory atomic replace,
  timestamped backup, ownership manifest at `~/.psmux/hooks-manifest.json` (version, file
  hashes), `psmux hooks status|doctor|uninstall`.
- codex and gemini installers are increment 2, each specified against its then-current
  contract first (codex: user-level `notify` receiving one JSON argv with
  `agent-turn-complete`, or project Stop hooks with a visible trust step; gemini:
  settings.json with PowerShell-policy caveats). A per-agent version/scope/trust matrix is
  a deliverable of that increment.

## 7. OSC fallback (increment 3, downgraded to untrusted)

- OSC 9 / OSC 777;notify produce UNTRUSTED `pane-notify` events — never `agent-done`,
  never authoritative, and NO suppression logic in either direction. Dedup only when both
  sources carry the same native `turn_id`.
- Staging: bounded ordered per-pane-instance FIFO (cap 32) with overflow/gap counters and
  rate limits — not the existing single-slot staging. Parsing spec covers BEL and ST
  terminators, malformed UTF-8 (lossy, bounded), semicolons in bodies, and non-collision
  with existing OSC 9;4 progress and OSC 52 handling.
- Reap race: an exited pane enters a draining state until the reader/parser pipeline is
  flushed and staged events published; the lifecycle exit event carries the same
  `pane_instance`.

## 8. Environment work

- Prerequisite fix (v1): protected env (`TMUX_PANE`, `PSMUX_*` minted values) is installed
  LAST and matched CASE-INSENSITIVELY (Windows env keys are case-insensitive), fixing the
  current ordering bug where user env can overwrite them (`src/pane.rs` ~199). Explicit
  exception: `PSMUX_HOOKS_DISABLED` (and per-agent variants) are user-settable.
- New pane env (identity only): `PSMUX_PANE_INSTANCE`, `PSMUX_SESSION_UID`, `PSMUX_BUS_ID`.
  No new address var — in-pane CLI discovery reuses the existing TMUX-var/registry
  resolution unchanged; cursor/bus validation catches stale-port reuse after restart.
- `--env-file` (increment 3): UTF-8 with BOM tolerance, CRLF, quoting, comments,
  duplicates-last-wins, NUL rejection, per-file/value/count limits, Win32 env-block size
  check before CreateProcessW, and cold-spawn enforcement (env-file on a warm target forces
  cold spawn).
- `wait-for` is NOT load-bearing anywhere in this design; its known defects (request has no
  response channel; waiter not registered) are logged as a separate pre-existing bug.
- JSON output (increment 3): audit the existing `-J` family for one-valid-document output
  under `-a` and add `-J` where missing (no duplicate `--json` flags on tmux verbs; the new
  agent verbs use `--json` since they have no tmux counterpart).

## 9. Redaction (v1, not deferred)

Event payloads carry BOUNDED METADATA ONLY by default: name, category, identity fields,
lengths (`title_len`/`body_len`), `turn_id`. Content requires BOTH `psmux notify
--include-content` AND server option `event-content on`, capped at 4 KiB, escaped on
human-readable output. Content travels via stdin, never command lines. Disk-persistence
ACL/retention lands with the increment-2 log spec.

## 10. Adversarial test plan (v1 gate)

Rust integration tests + PowerShell e2e; all must pass before v1 ships:
fast completion before wait-event (replay catch); stale replay ignored via `--after`;
respawn-pane with delayed old-instance hook rejected as stale; atomic cutoff — no loss or
duplication across the replay/live boundary under concurrent publish; cursor gap / bus_id
mismatch / session-ended distinct exit codes; blocked subscriber → `slow_consumer` close
with others unaffected; oversize event truncation; warm-server claim (no pre-claim events,
correct `session_uid`) and two concurrent warm servers; session rename does not reset the
bus; hook fires mid-frame → `capture-pane --settle` returns the complete screen; spoofed
`PSMUX_PANE_INSTANCE` rejected; elevated-server refusal; case-folded env protection
(`tmux_pane=x` cannot override); installer: concurrent install, reinstall/upgrade,
uninstall restores, foreign hooks preserved; server shutdown with active subscribers →
terminal frames + flush; pre-auth slow-loris dropped at deadline; bounded line framing.

## 11. Increments

1. Identity model + protected-env fix + in-memory bus + `events`/`cursor`/`wait-event`/
   `notify`/`hook-notify` + `capture-pane --settle` + Claude installer + unified shutdown
   path + pre-auth hardening + elevated refusal + full test matrix (§10).
2. Disk JSONL log (recovery/rotation/flush/ACL/retention spec) + `--cursor-file`/
   `--reconnect` + codex & gemini installers with per-agent contract matrix.
3. OSC untrusted `pane-notify` (FIFO staging, drain-before-reap) + `--env-file` + `-J`
   audit.

## 12. Deferred by decision (user call, 2026-07-11)

- Full transport-security rework (named pipe + DACL + client-PID verification; logon-
  session-scoped registry) — revisit if it presents a problem in practice.
- GSU round-2 formal re-grade of the revision — skipped; round-1 findings incorporated.

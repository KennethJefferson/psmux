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
- New code paths get bounded line framing and a pre-auth deadline on connections. As shipped,
  `read_line_bounded` (`src/server/connection.rs`) is a single reader (one byte at a time,
  no internal retry loop across calls) used once per connection to read the `AUTH` line,
  under one cumulative deadline (5 s) that also bounds the byte cap (1024 B) check — not a
  per-read or per-command deadline. `Ok(None)` from it means either the cap was exceeded or
  the deadline expired; callers treat both identically (reject the connection), so the
  ambiguity is intentional and not a gap.
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
  state keyed by mutable session names. As shipped, this only applies to the pre-claim
  `__warm__` pool server's own initial pane: it was spawned before any real session existed
  to mint an identity, so `set_tmux_env` had nothing to export and that one pane's `notify`/
  hook calls stay a silent no-op forever (`WarmPane.minted_instance == None`, see
  `src/types.rs`). Warm PANE spares replenished by an already-live, already-identified
  session (a normal `session_uid` already minted) DO carry identity — it was baked in at
  spawn time and reused verbatim at transplant (`minted_instance == Some(n)`), never
  reallocated post-spawn. "Hook-driven agent panes require cold spawn" was an overstatement;
  the accurate rule is: only a session's very first pane, if it came from the pre-claim
  `__warm__` pool, lacks identity — every pane after that (including replenished warm
  spares) has it. See docs/agent-events.md "Warm-pane caveat" for the user-facing version
  and `PSMUX_NO_WARM=1` as the blunt opt-out.
- Popup panes are excluded from the event/identity system in v1 (documented).
- Session rename does not reset the bus.

## 4. Event bus (new `src/events.rs`)

In-memory only in v1; disk JSONL log is increment 2 with its own recovery/rotation/flush/
ACL/retention spec.

- Single sequencer: all publishes route through the main state loop and are emitted only
  AFTER the corresponding state mutation commits. Connection threads never mint `seq`.
- Producers (v1, as shipped): `pane-exited` (transition records capture `pane_instance`,
  reason, BEFORE identity teardown), `pane-bell`, `window-created`, `session-renamed`,
  `agent-done`, `agent-notify`, `stale-notify` (from hooks and `psmux notify`), `bus-closed`.
  Send/paste producers are explicitly NOT in v1.
- Subscriber registration under the bus lock: capture cutoff = current seq, register live
  queue, release. Writer emits ack frame → replay (filtered, seq ≤ cutoff, streamed from
  the 4096-event ring) → live events (seq > cutoff) from a bounded queue. Filters
  apply before enqueue. Replay never passes through the live queue.
- Limits enforced before fanout, as shipped: 16 KiB serialized event cap (oversize →
  payload replaced with `{truncated:true}`), an 8192-slot bounded per-subscriber channel
  with `try_send`-and-drop on a full queue (the "slow_consumer" path below), and a 64
  concurrent-subscriber cap per bus (`MAX_SUBSCRIBERS`, `src/events.rs`) — the 65th
  `subscribe()` call is refused registration (ack carries `"refused":"max_subscribers"`,
  no replay, connection closes) rather than growing `subs` unbounded. Slow consumers (full
  channel on `try_send`) are dropped and sent a terminal `slow_consumer` frame; other
  subscribers are unaffected. Per-pane/global publish RATE limits and a bounded
  per-subscriber socket-write deadline with cancellation are explicitly DEFERRED to
  increment 2 — not part of the v1 surface described here.
- Ack frame (as shipped): `{type, session_uid, bus_id, oldest_seq, latest_seq,
  replay_count, gap, gap_reason, mismatch, refused}` — `refused` is `null` unless the
  64-subscriber cap rejected this registration, in which case it's the string
  `"max_subscribers"`.
- Heartbeat frame every 15 s (client-suppressible client-side, not server-gated).
- Shutdown: one idempotent server-shutdown path, `shutdown_server(app, reason)` in
  `src/server/mod.rs`, called from every exit site with a reason specific to that site —
  `"kill-server"`, `"detach-exit"`, `"session-teardown"`, `"exit-empty"` — not a single
  generic reason. Each call: publish terminal `bus-closed` (payload carries that same
  `reason`), close subscriber queues with the terminal frame, remove the port/key files,
  then exit. Event subscribers do not keep exit-empty sessions alive. Hard kill/panic
  excluded (documented).

## 5. CLI verbs and the synchronization contract

- `psmux events [--name X]... [--category Y]... [--after '<session_uid>:<bus_id>:<seq>']
  [--no-heartbeat] [--json]` — long-lived subscribe stream (ack/replay/live/heartbeat/
  terminal frames as JSON lines). The ack frame carries a `mismatch` field: when
  `--after`'s `session_uid`/`bus_id` doesn't match this bus, `mismatch:true` is written
  and the stream closes immediately (no replay, no live events) — the CLI's non-clean-close
  detection then exits 1. A malformed (unparseable) `--after` value gets a dedicated
  `{"type":"error","error":"bad cursor"}` frame instead of silently falling back to a
  live-only stream, then the connection closes. Terminal frames (`bus-closed`,
  `slow_consumer`, and now `closed` with `reason:"max_subscribers"` when the 64-subscriber
  cap refuses registration) are distinguishable from transport loss by exit code.
  `--cursor-file`/`--reconnect` are increment 2 (alongside the disk log).
- `psmux cursor [--json]` — prints the current `{session_uid, bus_id, seq}` cursor.
- `psmux wait-event --pane %N [--instance G] [--name agent-done] [--after <cursor>]
  --timeout <ms> [--json]` — ONE-SHOT: `--after` is OPTIONAL as shipped (omitting it
  subscribes live-only, matching `events-subscribe`'s own `None` semantics — there is no
  requirement to always pass a cursor). When given, subscribes with the atomic cutoff
  contract, scans the replay window after `<cursor>` (catching completion-before-wait),
  then blocks. Exit 0 + event JSON on match; distinct nonzero codes for timeout (2) /
  cursor gap (3) / bus mismatch (4) / session-ended (5); a refused (max-subscribers)
  registration surfaces as a generic error exit (1), same bucket as any other `ERR:` reply.
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
correct `session_uid`) and two concurrent warm servers [DEFERRED — no automated coverage
shipped in v1; single warm-server claim is covered by the stale/e2e suites, but the
two-concurrent-warm-servers interleaving is not exercised by any test in this repo as of
this writing]; session rename does not reset the bus; hook fires mid-frame →
`capture-pane --settle` returns the complete screen (as shipped, this also required fixing
`capture-pane -t %N --settle` to resolve and probe the actual `-t` target instead of
whatever pane happens to be active — see the `PaneDataVersion(Option<usize>, ...)` fix);
spoofed `PSMUX_PANE_INSTANCE` rejected; elevated-server refusal; case-folded env protection
(`tmux_pane=x` cannot override); installer: concurrent install, reinstall/upgrade,
uninstall restores, foreign hooks preserved; server shutdown with active subscribers →
terminal frames + flush; pre-auth slow-loris dropped at deadline; bounded line framing;
malformed `--after` cursor → dedicated `bad cursor` error frame (not silent live-only
fallback); 64-subscriber cap refuses the 65th registration. Cutoff-under-concurrent-publish
stress specifically [DEFERRED — the atomic-cutoff *logic* is covered by
`subscribe_replays_filtered_after_cursor`/`live_events_reach_subscriber_after_subscribe` in
`tests-rs/test_agent_events_bus.rs`, but there is no dedicated stress test that publishes
concurrently from multiple threads while subscribing, to catch a race the single-threaded
unit tests can't surface].

## 11. Increments

1. Identity model + protected-env fix + in-memory bus + `events`/`cursor`/`wait-event`/
   `notify`/`hook-notify` + `capture-pane --settle` + Claude installer + unified shutdown
   path + pre-auth hardening + elevated refusal + full test matrix (§10).
2. Disk JSONL log (recovery/rotation/flush/ACL/retention spec) + `--cursor-file`/
   `--reconnect` + codex & gemini installers with per-agent contract matrix.
3. OSC untrusted `pane-notify` (FIFO staging, drain-before-reap) + `--env-file` + `-J`
   audit.

Follow-ups identified during the final-review fix-up pass (not yet scheduled to a specific
increment above):
- PID-scoped `kill_remaining_server_processes` (`src/session.rs`): today it is a broad
  nuclear fallback used by `kill-server`, not scoped to the specific server this client is
  talking to. Scope it to the target session's own process (and its known child PIDs) so a
  `kill-server` on one session can't have any chance of reaping an unrelated psmux server
  process on the same machine.
- Warm-claim identity injection: `silent_rehome` (`src/pane.rs`) rewrites a transplanted
  pane's cwd/env at claim time but does not mint or inject identity env for the one case
  that truly lacks it — a session's very first pane, claimed from the pre-claim `__warm__`
  pool (see §3). A `silent_rehome`-style targeted env injection at warm-claim time, scoped
  to just that one gap, would close it without touching the (correctly identity-bearing)
  warm PANE-spare replenishment path.
- `gap_reason` constants: `EventBus::subscribe` (`src/events.rs`) currently builds a
  free-form `String` ad hoc at each call site ("cursor older than retained ring", "cursor
  newer than latest"). Promote these to named constants (or a small enum with
  `Display`/`as_str`) so callers and tests don't rely on exact prose matching, and so new
  gap reasons can't accidentally collide with or subtly restate an existing one.

## 12. Deferred by decision (user call, 2026-07-11)

- Full transport-security rework (named pipe + DACL + client-PID verification; logon-
  session-scoped registry) — revisit if it presents a problem in practice.
- GSU round-2 formal re-grade of the revision — skipped; round-1 findings incorporated.
- Concurrent warm servers and cutoff-under-concurrent-publish stress test coverage (§10):
  the interleaving/race scenarios themselves are architecturally handled (dormant bus
  until claim; subscribe-under-lock cutoff capture), but no automated test in this repo
  drives either scenario under actual concurrency as of this fix-up pass — see the §10
  annotations for exactly what is and isn't covered today.

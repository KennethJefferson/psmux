# Agent Events

Deterministic orchestration for multi-agent workflows: an orchestrator (Claude Code,
Codex, or any script) running *outside* psmux can drive lead agents running *inside*
panes without polling, without capture-pane scraping loops, and without a race between
"did the task finish yet?" and "start listening for completion."

Every piece of this is a native psmux feature — no PowerShell runspaces, no skill or
plugin required at runtime.

## The core recipe: cursor → dispatch → wait-event

```powershell
$cur = psmux cursor                                                    # commitment point
psmux send-keys -t %1 'claude --dangerously-skip-permissions "task"' Enter
psmux wait-event --pane %1 --name agent-done --after $cur --timeout 600000   # push, no polling
psmux capture-pane -t %1 -p --settle 400                               # read the completed screen
```

This ordering is the whole contract, and it is race-free by construction:

1. **`psmux cursor`** prints the current event-bus position as `session_uid:bus_id:seq`.
   Capture it *before* you dispatch anything — this is your commitment point.
2. **Dispatch** the work (`send-keys`, launching an agent, etc.).
3. **`psmux wait-event --after <cursor>`** subscribes, replays everything published since
   your cursor (catching the task if it finished before you even called `wait-event`),
   and then blocks for live events. There is no "subscribe before dispatch" ordering
   requirement — the cursor is what makes this safe, not timing.
4. Once you have proof of completion, **`capture-pane --settle`** reads the finished
   screen instead of a mid-render partial frame.

`agent-done` is a **semantic completion signal**, never a screen barrier — the agent's
hook fires the instant its turn ends, which can be before the terminal has finished
repainting. That's what `--settle` is for (see below).

## Exit codes

`wait-event` always exits with one of these codes, so scripts can branch without parsing
error text:

| Exit code | Meaning |
|---|---|
| `0` | Matched — event JSON printed to stdout |
| `2` | Timeout — no matching event before `--timeout` elapsed |
| `3` | Gap — your cursor is older than the retained replay window; some events may have been missed |
| `4` | Mismatch — your cursor's `session_uid`/`bus_id` doesn't belong to this session's bus (e.g. stale cursor from a killed/replaced server) |
| `5` | Ended — the session's event bus closed while you were waiting |
| `1` | Other/unexpected error |

`capture-pane --settle` also has a distinct nonzero exit: if the pane doesn't go quiet
within `--settle-timeout`, it captures anyway and exits nonzero so the caller can tell
"I got a screen, but it may still be mid-render" apart from a clean settle.

## Event names and categories

| Category | Event names | Fired when |
|---|---|---|
| `pane` | `pane-exited` | A pane's process terminates (carries pane id + instance so a later pane reusing the same id can't be confused with this one) |
| `pane` | `pane-bell` | A bell (`\x07`) is detected in a pane's output (carries pane id + instance) |
| `window` | `window-created` | A new window is created |
| `session` | `session-renamed` | The session is renamed |
| `agent` | `agent-done` | A validated in-pane `notify --done` or an agent hook's turn-complete signal |
| `agent` | `agent-notify` | A validated in-pane `notify` (custom event name) or an agent hook's non-completion signal |
| `agent` | `stale-notify` | A `notify`/hook call whose claimed identity did **not** match the server's live record for that pane (see Threat model) |
| `bus` | `bus-closed` | Terminal frame published once, right before the server shuts down |

Filter `psmux events`/`wait-event` by `--name` and/or `--category` to scope what you
receive.

If your `--after` cursor's `session_uid`/`bus_id` doesn't match this session's bus, the
subscribe ack shows `"mismatch":true` and the stream immediately closes — the CLI exits 1
(non-clean close) rather than silently falling back to a live-only stream. A malformed
`--after` value (fails to parse as `session_uid:bus_id:seq`) gets its own one-line
`{"type":"error","error":"bad cursor"}` frame before the connection closes, again for the
same reason: never silently proceed as if `--after` had been omitted. A session bus also
caps concurrent subscribers at 64 — the 65th `events`/`wait-event` caller gets an ack with
`"refused":"max_subscribers"` followed by a `closed` frame (`wait-event` reports this as an
error, non-zero exit) instead of being queued or silently dropped.

## `notify` and `hook-notify`

```powershell
psmux notify --done                                  # fires agent-done
psmux notify --name my-event --title "..."            # fires a custom agent-notify
psmux notify --done --include-content                 # attach a content blob (see redaction, below)
```

- `notify` **self-identifies** using env vars psmux exports into every pane it spawns
  (`PSMUX_PANE_INSTANCE`, `PSMUX_SESSION_UID`) plus the pane id from `TMUX_PANE`. You
  never pass identity flags yourself.
- Reliability contract: **bounded synchronous**. `notify` waits up to 2000 ms for the
  server to acknowledge the publish, then exits 0 regardless — it will never hang the
  calling agent beyond that bound. If you need a hard guarantee the event was seen,
  your `wait-event --timeout` on the orchestrating side is the documented backstop.
- Run outside any psmux pane (no identity env present)? `notify` is a **silent no-op**
  that exits 0 — safe to leave in a script that sometimes runs under psmux and
  sometimes doesn't.

`hook-notify` is the machine-facing entrypoint agent hook systems call directly — you
normally never invoke it by hand; `psmux hooks install` wires it up for you. It reads
the agent's own JSON payload on stdin, always prints `{}` and exits 0 (so it never
breaks the agent's hook protocol, even if psmux itself is unreachable), and is gated by:

- **`PSMUX_PANE_INSTANCE` must be present** — outside psmux, it's a silent no-op.
- **`PSMUX_HOOKS_DISABLED=1`** — user kill-switch. Set it (in the pane's environment,
  a `set-environment`, or your agent launch command) to suppress all hook-driven
  notifications from that pane without uninstalling anything. Unlike the other
  identity env vars, this one is deliberately **not** protected — you're meant to be
  able to set it yourself.

## Installing hooks: `psmux hooks install <agent>`

```powershell
psmux hooks install claude                # writes to ~/.claude/settings.json
psmux hooks install claude --project-local  # writes to .claude/settings.local.json instead
psmux hooks status claude                 # or: doctor
psmux hooks uninstall claude

psmux hooks install codex                 # writes to ~/.codex/hooks.json (global only)
psmux hooks status codex                  # reports installed + trust state
psmux hooks uninstall codex
```

Two verified integrations: **Claude Code** and **Codex**. `install` writes `Stop` (and,
for claude, `SessionStart`/`SessionEnd`) hook entries whose command is the psmux binary
itself — no `.cmd`/`.ps1` wrapper, no PowerShell quoting or execution-policy surface to
fight. Codex wires only the `Stop` → `agent-done` done-signal (that is the whole point:
a relay does `wait-event --name agent-done` instead of polling `capture-pane --settle`).

The installer is safe to run repeatedly and safe next to hand-edited config:

- File-locked read-modify-write with a timestamped backup on every change.
- Semantic JSON merge that only touches entries whose command matches the psmux
  marker — your own hooks (and other tools' hooks) are left completely alone.
- An ownership manifest at `~/.psmux/hooks-manifest.json` tracks what psmux installed,
  so `uninstall` removes exactly that and nothing else.
- `--project-local` writes to `.claude/settings.local.json`, never `.claude/settings.json`
  — psmux will not silently commit executable hook config into your repo.
- Idempotency is **exact-match**: a re-install with an unchanged binary path is a no-op
  (no write), so it never needlessly disturbs a trust-gated config. A *stale* entry
  (e.g. after the psmux binary moved) is detected as not-installed and repaired.

### Codex: global-only + trust-hash

Codex hooks live at `~/.codex/hooks.json` and are **global to every codex session** —
there is no `--project-local` scope (the flag is rejected). Codex also **content-hash
gates its hooks**: any edit to `hooks.json` invalidates the recorded `trusted_hash` in
`~/.codex/config.toml`, and codex will prompt to **re-trust hooks on next launch**.
`psmux hooks install codex` prints a warning to that effect, and `psmux hooks status
codex` reports `trust-pending` until a matching trust entry exists.

**This has a real consequence: until you accept the re-trust prompt, the Stop hook does
not fire — so `agent-done` never publishes.** A relay that blocks on `wait-event` with no
timeout would hang. Therefore, **always pass `--timeout` to `wait-event` in a relay** and
fall back to `capture-pane --settle` on timeout. The bus is a latency optimization over
polling, never a single point of hang:

```powershell
psmux wait-event --name agent-done --pane %1 --timeout 60   # exit 2 = timeout → fall back to settle
```

The installer's file safety (locked read-modify-write, backup-on-change-only, atomic
rename-over-target, marker-scoped merge, exact-match idempotency) applies identically to
codex's file — it never overwrites a config it could not fully read, and `uninstall`
removes only psmux's own command, leaving any sibling hooks intact.

## `capture-pane --settle`

```powershell
psmux capture-pane -t %1 -p --settle 400 --settle-timeout 5000
```

Blocks until the pane's output has been idle for `--settle` milliseconds, then
captures — so you read the finished screen instead of a frame mid-repaint. If nothing
settles within `--settle-timeout` (default 10000 ms), it captures anyway and exits
nonzero so the caller can distinguish "definitely done" from "captured under a timeout."

Pair this with `wait-event --name agent-done`: the hook fires the moment the agent's
turn completes, which can still be a beat before the terminal finishes drawing —
`--settle` is what closes that gap.

## Threat model, in short

psmux's agent-events trust boundary matches the rest of psmux: it trusts all processes
belonging to the same Windows user at the same integrity level. Identity tokens exist
for **determinism**, not authentication:

- `session_uid`, `bus_id`, and each pane's `pane_instance` are **minted by the server**
  and never trusted from a client's claim. When a pane calls `notify`, the server
  checks the caller's claimed identity against its own live record for that pane.
- A mismatch — e.g. a delayed hook firing from a process that has since been killed
  and replaced by `respawn-pane` — is rejected and republished as `stale-notify`
  instead of `agent-done`/`agent-notify`. It never silently succeeds, and it never
  satisfies a waiter listening for the real thing.
- The server **refuses to start elevated** unless you explicitly opt in (see
  `PSMUX_ALLOW_ELEVATED`, below) — this closes off the medium-to-elevated escalation
  path for the entire command surface, including the event stream.
- Event payloads are **bounded metadata by default**: name, category, identity fields,
  and *lengths* (`title_len`/`body_len`) — never the actual text. Seeing real content
  requires opting in on both sides: the caller passes `notify --include-content` *and*
  the server has `event-content on` set; content is capped at 4 KiB and always travels
  over stdin, never a command line.
- New connections (including the event stream) get bounded line framing and a
  pre-auth deadline, so a slow or hung peer can't tie up a connection slot forever.

This is a determinism/attribution boundary, not a sandbox — if you need to isolate
mutually-untrusted agents from each other, that's a different (OS-level) problem this
feature does not attempt to solve.

### `PSMUX_ALLOW_ELEVATED`

If your shell is running elevated, the psmux server refuses to start **by design** —
this is the same posture as tmux's own historical guidance against running a
multiplexer server as root/admin unnecessarily. If you have a specific, understood
reason to run an elevated server anyway (e.g. a locked-down CI box that only has an
elevated shell available), set:

```powershell
$env:PSMUX_ALLOW_ELEVATED = "1"
```

before starting the server. There is no equivalent override for the reverse direction
(an elevated *client* talking to a non-elevated server) — that path isn't blocked,
since it doesn't cross the same trust boundary.

## Warm-pane caveat — read this before wiring up hooks

psmux keeps a pre-spawned "warm" shell ready so `new-session`/`new-window`/`split-window`
feel instant. **A pane that gets its shell by claiming a warm standby from a
`__warm__` pool server does not have psmux's identity env at all** — that pool server
spawned the shell before any real session existed to mint an identity for it, so there
was nothing to export yet. Any `notify`/hook-notify call from such a pane is a silent
no-op (no identity env → the same "outside psmux" behavior described above), not a
`stale-notify` — it simply never reaches the bus.

In practice this means: **if you plan to run an agent in a pane and rely on its hooks
or `notify` calls to signal completion, launch it in a pane created *after* the
session already exists** (a `split-window`/`new-window` you create yourself, then
dispatch into) rather than assuming the session's very first pane is always
notify-capable — and if you want to rule out the warm-pane path entirely for a given
run, set:

```powershell
$env:PSMUX_NO_WARM = "1"
```

before creating the session. This forces every pane (including the session's first
one) to cold-spawn, at the cost of the instant-open feel warm panes normally give you.

## Popup panes

Popup panes (`display-popup`) are excluded from the event/identity system entirely in
v1 — they never get identity env, and lifecycle events for them are not published.
Don't route agent orchestration through a popup; use regular panes/windows instead.

## What's not here yet

The disk-persisted JSONL event log (with recovery/rotation/retention), `--cursor-file`
reconnect support, the codex and gemini hook installers, the untrusted OSC 9/777
`pane-notify` fallback, and `--env-file` are later-increment work and not part of this
release. `psmux events`/`wait-event`/`notify`/`hook-notify` and the Claude Code
installer described above are the complete, shipped v1 surface.

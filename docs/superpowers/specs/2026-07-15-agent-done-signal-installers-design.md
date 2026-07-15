# Event-driven done-signal: codex & gemini hook installers — design

Date: 2026-07-15
Branch: `feature/agent-events-done-signal`
Status: **IMPLEMENTED (codex-only), 2026-07-15** — pending final whole-branch review
Predecessor: 2026-07-11-agent-events-design.md (increment 1, merged: PR #1 + #2)

> **SCOPE REVISION (2026-07-15, before implementation):** Gate-0 inspection found that
> **antigravity (`agy`, the gemini-family CLI actually used for relays) has NO hook mechanism**
> — no hook file, no hook/notify/event flags, only `plugin` management. The event-driven
> done-signal is impossible on antigravity's side; agy relays keep settle-polling (which works).
> The Google **gemini CLI** (`~/.gemini/settings.json`) *is* hook-capable but is not the CLI in
> use. Decision: **this branch ships the CODEX installer only.** All "gemini" installer content
> below is DEFERRED/not-built. Retained: shared-spine hardening (§5), exact-match idempotency
> (§6), codex trust-hash defense (§7), real-tmux warning fix (§8) — all justified for codex alone
> (its hooks.json is trust-gated) and they harden claude's shipped path too.

## 1. Purpose

Give codex and gemini panes the same event-driven "done" signal Claude panes already have.
A relay/orchestrator today infers turn completion by polling `capture-pane --settle` (works, but
it is inference and can false-fire on a mid-turn quiet window). With this feature, the agent's own
completion hook publishes an `agent-done` event onto the psmux event bus, and a relay does one
blocking `wait-event --name agent-done --pane %N` instead of polling.

Scope is **done-signal only** — each installer wires exactly ONE hook event to `agent-done`. The
full per-event matrix (tool-use, prompt-submit, etc.) is explicitly out of scope (YAGNI); it can be
added later behind the same installer if a concrete consumer appears.

## 2. Background / ground truth

The event bus (in-memory, one per server lifetime) and the `hook-notify` entrypoint shipped in
increment 1. Key facts this design builds on:

- `hook-notify <agent> <event>` (`src/main.rs`) is **agent-agnostic**: pane identity comes from
  `PSMUX_*` process env (`NotifyEnv::from_process_env`), NOT from parsing the agent's stdin payload.
  It always prints `{}` and exits 0 so it can never break the host agent.
- Event-name mapping is `parse_hook_event_name` (`src/main.rs`): `stop | agent-turn-complete =>
  agent-done`, else `agent-notify`. The `_agent` arg is currently unused.
- The Claude installer (`src/hooks_install.rs`, ~169 lines) merges a hook group into
  `.claude/settings.json`: sibling-lockfile locking, timestamped backup, marker-scoped
  idempotent strip-and-re-add (`OWNED_MARKER`, `is_owned`, `strip_owned`), atomic write,
  per-agent manifest.

Observed agent hook contracts (this machine, treated as authoritative per user decision):

| Agent  | Hook file | Schema | Done event | Trust gate | Project scope |
|--------|-----------|--------|-----------|-----------|---------------|
| claude | `.claude/settings.json` (or global) | hooks object | `Stop` | none | yes (`--project-local`) |
| codex  | `~/.codex/hooks.json` | **identical to claude** | `Stop` | **content-hash in `config.toml [hooks.state]`** | **no (global only)** |
| gemini | `~/.gemini/settings.json` (hooks nested inside) | same list-of-groups shape | `AfterAgent` | none | **no (global only)** |

All three use the same nested shape: `hooks.<Event>` is an **array of groups**, each group has a
`hooks` **array of commands**. Multiple entries per event are supported → installers are ADDITIVE
(push a group), never clobber.

gemini's `settings.json` also holds unrelated **user-owned** config (`security.auth` oauth creds,
`general.sessionRetention`). It is NOT a psmux-owned file. This raises the stakes on every write.

## 3. Decision record

- **Persistent installer (approach B), not ephemeral session-scoped injection (approach A).** Chosen
  for a simpler mental model (`install` / `uninstall`). The user has ACCEPTED the global-footprint
  trade: while installed, codex/gemini fire `hook-notify` on every session machine-wide, and codex
  will show a one-time re-trust prompt. Reversible on demand via `uninstall` (marker-scoped, byte-clean).
- **Harden the shared spine** (not a gemini-only side path). The file primitives are fixed once for
  all three agents; claude benefits too. One correct implementation, no per-agent divergence.
- **Codex trust-hash defense = warn + status-state + timeout discipline** (no self-test verb, no
  ephemeral fallback for codex).

## 4. Architecture

Extend existing `src/hooks_install.rs` + the `hooks` CLI verb. No new subsystem.

```
acquire_lock / load / atomic_write / backup   ← shared primitives, HARDENED (§5)
owned_marker(agent) -> " hook-notify <agent> " ← was a const, now per-agent
hook_entry(agent, exe, event)                  ← already parameterized
install_json_hooks(path, exe, agent, events, opts) ← generalized claude/codex merge (same schema)
  install_claude = install_json_hooks(.claude/settings.json, "claude",
                     [(file-key "Stop", arg "stop"), ("SessionStart","session-start"), ("SessionEnd","session-end")])
  install_codex  = install_json_hooks(~/.codex/hooks.json,   "codex",  [(file-key "Stop", arg "stop")]) + trust-warn (§7)
install_gemini(~/.gemini/settings.json, exe)   ← settings.json-nested variant, same hardened primitives;
                     one event: (file-key "AfterAgent", arg "agent-turn-complete")
```

Each event is a pair: the **file-key** (the JSON `hooks.<Event>` key each agent expects — codex
`Stop`, gemini `AfterAgent`) and the **hook-notify arg** the command passes (`stop` /
`agent-turn-complete`), which `parse_hook_event_name` maps to `agent-done`. These are distinct: the
file-key is what the agent fires on; the arg is what psmux receives.

The `hook-notify` entrypoint needs no change for identity. Event mapping needs no change: the
**gemini installer emits the already-supported `agent-turn-complete` token** (not gemini's literal
`AfterAgent`), which `parse_hook_event_name` already maps to `agent-done`. codex emits `stop`
(already mapped). This keeps the installer→entrypoint round-trip exact with zero mapping edits.

## 5. Shared-spine hardening (benefits claude too)

GSU (gpt-5.6-sol, xhigh) design review flagged these as FIX-FIRST because gemini's file is
credential-bearing and codex is trust-gated. The Claude installer's original shortcuts were
acceptable for a psmux-adjacent low-stakes file; they become data-loss/silent-hang bugs on
user-owned or trust-gated files.

- **`load()`: only `ErrorKind::NotFound` means empty `{}`.** Every other read error (permission,
  sharing violation, etc.) ABORTS — never write back a file we could not fully read. Incompatible
  shape (`hooks` present but not an object) also ABORTS rather than coercing/overwriting.
- **`atomic_write()`: true atomic replace.** Write temp in the same directory, then rename-over the
  original (Windows replace semantics). NO delete-then-rename — a crash must never leave zero file.
- **`backup()`: must-succeed-on-change, skip-on-no-op.** Compute ownership FIRST; back up only when
  actually changing an existing file, and abort if the backup copy fails. Never create a
  credential-bearing backup during a no-op uninstall.

## 6. Exact-match idempotency (replaces the loose marker check)

- "Installed" = the target event contains our EXACT desired owned group: command path (current
  `psmux.exe`) + event arg + timeout. A stale command (e.g. after the exe moved) is detected as
  NOT correctly installed and repaired.
- If the file is ALREADY exactly right, perform NO write — this avoids needlessly re-invalidating
  codex's trust hash on a redundant install.
- `uninstall` removes only the **owned command** and prunes its group only when the group becomes
  empty; it removes psmux-CREATED empty containers but never pre-existing ones (full reversibility).
- `status` validates every configured event for the agent (not just one), and reports the embedded
  exe path and whether that path still exists.

## 7. Codex trust-hash defense (no silent hang)

Editing `~/.codex/hooks.json` invalidates codex's per-file `trusted_hash`; codex prompts to
re-trust on next launch. If missed/declined, the hook silently never fires — and `hook-notify`'s
delivery failure is swallowed — so a naive `wait-event` would hang with no diagnostic.

- Installer prints a LOUD explicit warning after any change to codex's file: codex will prompt to
  re-trust; until accepted, the done-signal will not fire.
- `status codex` distinguishes **configured** from **trust-pending**: read `config.toml
  [hooks.state.'…hooks.json:stop:0:0'].trusted_hash`; if present and matches the current file, report
  trusted; else trust-pending.
- **Relay discipline (documented, load-bearing):** `wait-event` in a relay MUST carry `--timeout`.
  On timeout the relay falls back to `capture-pane --settle` (which still works). The bus is an
  optimization, never a single point of hang.

## 8. Real-tmux warning fix

`warn_if_warm_claimed_pane` currently warns for ANY `TMUX_PANE` lacking `PSMUX_PANE_INSTANCE` —
which also describes ordinary tmux and every non-psmux codex/gemini turn. With a GLOBAL install
that means nagging after unrelated turns machine-wide. Fix: gate the warning on a psmux-specific
marker (e.g. `PSMUX_SESSION_UID` present), so a globally-installed hook is SILENT outside psmux.

## 9. CLI surface

```
psmux hooks install   claude [--project-local]     (unchanged)
psmux hooks install   codex                          (global only)
psmux hooks install   gemini                          (global only)
psmux hooks uninstall codex|gemini
psmux hooks status    codex|gemini
```

- codex/gemini REJECT `--project-local` explicitly (no project-scoped hook file exists for them).
- Global path resolution FAILS if no home dir is available (no relative-dir fallback).
- Unknown flags rejected consistently across install/uninstall/status (and the `doctor` alias).

## 10. Manifest

`write_manifest` is generalized to write the acting agent's entry (`m["codex"]` / `m["gemini"]`)
alongside `m["claude"]`, with its own atomic write and merge-preserving existing entries; a
manifest failure must not leave a written hook unreported (define ordering so status/uninstall can
always find an installed hook). `uninstall` clears the agent's manifest entry.

## 11. Verification gates (plan-level, before trusting either installer)

- **Env-propagation smoke test (blocking):** launch codex and gemini INSIDE a psmux pane, fire one
  turn each, confirm the hook subprocess sees `PSMUX_PANE_INSTANCE` + `PSMUX_SESSION_UID` and the
  `agent-done` event lands on the bus. If an agent sanitizes env, the approach is a silent no-op for
  that agent and must be discovered here, not in production.
- **End-to-end done-signal test:** codex `Stop` and gemini `AfterAgent`→`agent-turn-complete` each
  wake a `wait-event --name agent-done --pane %N` with correct pane attribution.

## 12. Test matrix

Unit / integration:
- `load()` returns empty only on NotFound; aborts on other errors and on non-object `hooks`.
- `atomic_write()` never leaves a missing file across a simulated mid-write failure.
- backup made only on real change; no backup on no-op uninstall.
- exact-match idempotency: stale-exe command repaired; already-exact file → zero write (hash stable).
- uninstall removes only the owned command; sibling user command in the same group survives; empty
  psmux-created containers removed, pre-existing ones preserved; manifest entry cleared.
- gemini merge preserves `security.auth` + `general` across install AND uninstall.
- codex `status` reports trusted vs trust-pending from `config.toml`.
- warning suppressed in plain-tmux / non-psmux env; emitted only with the psmux marker.
- CLI: `--project-local` rejected for codex/gemini; unknown flags rejected; no-home fails.

Live (gated by §11): the two real-agent verification tests above.

## 13. Out of scope (this increment)

- Full per-event matrix for codex/gemini (tool-use/prompt/compact/permission events).
- Disk JSONL event log, `--cursor-file`/`--reconnect`, rate-limiter (the rest of the original
  increment-2 bundle — deferred; not needed for the done-signal and each carried global/scale cost
  the user did not want now).
- Ephemeral session-scoped hook injection (approach A) — rejected in favor of persistent install.
- Pre-existing shared-code cleanups GSU noted but that this branch does not regress (crash-stale
  lock recovery, no-op mkdir side effects): tracked as follow-ups, not gating this work.

# Event-Driven Done-Signal Installer (codex) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **SCOPE REVISED 2026-07-15 → CODEX-ONLY.** Gate inspection found antigravity (`agy`) has no
> hook mechanism, so the gemini/agy installer cannot be built. **Task 7 (gemini installer) is
> DROPPED. Tasks 0/9/10 are codex-only.** The shared-spine hardening (Tasks 1-3), per-agent
> marker/idempotency (4-6), and warning fix (8) are retained — justified for codex alone and they
> harden claude's shipped path. See spec's scope-revision banner and `.superpowers/sdd/progress.md`.

**Goal:** Add `psmux hooks install codex` so codex panes publish `agent-done` onto the event bus, replacing settle-polling with `wait-event`.

**Architecture:** Extend the existing `src/hooks_install.rs` Claude installer and the `hooks` CLI verb in `src/main.rs`. First harden the shared file primitives, then add the codex installer (same hooks.json schema as claude) with trust-hash defenses. An early live gate proves codex preserves `PSMUX_*` env into the hook subprocess before we build on that assumption.

**Tech Stack:** Rust, `serde_json`, existing psmux test conventions (`tests-rs/` modules, `#[cfg(test)]` include, `PSMUX_HOOKS_MANIFEST_DIR` sandbox).

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-15-agent-done-signal-installers-design.md`. Every task's requirements implicitly include it.
- Branch: `feature/agent-events-done-signal`.
- Elevated dev shell: any live psmux run needs `PSMUX_ALLOW_ELEVATED=1`.
- Build/test single pass only: `cargo test --bin psmux` (NEVER background cargo on this machine; NEVER `cargo test` full — runs 3× ~3min).
- codex hooks are **global-only** (no `--project-local`), at `~/.codex/hooks.json`, and **trust-hash gated** (`config.toml [hooks.state]`) — any edit triggers a re-trust prompt; exact-match idempotency avoids needless re-writes.
- Scope is **done-signal only, CODEX ONLY**: one hook event (`Stop` → `agent-done`). Gemini/agy dropped (agy has no hooks).
- Marker convention: owned command substring is `" hook-notify <agent> "` (leading+trailing space), matching the existing `OWNED_MARKER = " hook-notify claude "`.
- Event round-trip: installer command passes a **hook-notify arg** (`stop` / `agent-turn-complete`) that `parse_hook_event_name` maps to `agent-done`. The **file-key** (JSON `hooks.<Event>` key) is the agent's own event name (codex `Stop`, gemini `AfterAgent`).

---

## Task 0: Live env-propagation gate (BLOCKING — run before writing any installer code) — CODEX ONLY

**Purpose:** Prove **codex** preserves `PSMUX_PANE_INSTANCE` + `PSMUX_SESSION_UID` into a hook subprocess. If codex sanitizes env, `hook-notify` is a silent no-op and the whole feature is void — discover it now (spec §11). (Gemini/agy dropped: agy has no hooks; not gated.)

**Files:**
- Create: `tests/gate_env_propagation.ps1` (manual live gate, not a cargo test)

**Interfaces:**
- Consumes: nothing.
- Produces: a documented PASS/FAIL per agent. FAIL for an agent removes its installer tasks from scope.

- [ ] **Step 1: Build the current release once**

Run (foreground, Git Bash or PS): `cd psmux && cargo build --release --bin psmux`
Expected: compiles clean. Note the exe path: `psmux/target/release/psmux.exe`.

- [ ] **Step 2: Write the gate script**

Create `tests/gate_env_propagation.ps1`:

```powershell
# Live gate: does <agent> preserve PSMUX_* into a hook subprocess?
# Manual — requires codex/gemini CLIs installed. Sets PSMUX_ALLOW_ELEVATED for the elevated dev shell.
param([string]$Psmux = "$PSScriptRoot\..\target\release\psmux.exe")
$env:PSMUX_ALLOW_ELEVATED = "1"
$probe = Join-Path $env:TEMP "psmux-envgate-$PID.txt"
if (Test-Path $probe) { Remove-Item $probe -Force }

# A fake hook command that records whether the two identity vars survived.
$hookCmd = "cmd /c `"echo INSTANCE=%PSMUX_PANE_INSTANCE% SESSION=%PSMUX_SESSION_UID% > `"$probe`"`""

Write-Host "Probe file: $probe"
Write-Host "Manually: in a psmux pane running <agent>, install a hook whose command is:"
Write-Host "  $hookCmd"
Write-Host "Then fire a turn. After it completes, this script reads the probe."
Write-Host "PASS if INSTANCE and SESSION are BOTH non-empty."

if (Test-Path $probe) {
  $line = Get-Content $probe -Raw
  Write-Host "RESULT: $line"
  if ($line -match "INSTANCE=\d+" -and $line -match "SESSION=\w+") { Write-Host "PASS" } else { Write-Host "FAIL" }
} else {
  Write-Host "No probe yet — run the manual steps above first."
}
```

- [ ] **Step 3: Run the gate live for BOTH agents**

For codex and gemini each: `gridup.ps1 -Spec '<agent>' -Visible -FreshStart`, install the probe hook into that agent's real hook file inside the pane, fire a turn, run the gate script, record PASS/FAIL. Tear down with `griddown.ps1`.

Expected: PASS (INSTANCE + SESSION both populated) for each agent you intend to ship. **If an agent FAILs, stop and report — its installer tasks are out of scope until resolved.**

- [ ] **Step 4: Commit the gate script + result note**

```bash
git add tests/gate_env_propagation.ps1
git commit -m "test(gate): live env-propagation gate for codex/gemini hooks (spec §11)"
```

---

## Task 1: Harden `load()` — NotFound-only-empty, abort on other errors

**Files:**
- Modify: `src/hooks_install.rs:26-32` (`load`)
- Test: `tests-rs/test_hooks_install.rs` (append)

**Interfaces:**
- Consumes: nothing new.
- Produces: `load(path) -> Result<serde_json::Value, String>` unchanged signature; new behavior: returns `Ok({})` ONLY when the file does not exist; every other read error is `Err`.

- [ ] **Step 1: Write the failing test**

Append to `tests-rs/test_hooks_install.rs`:

```rust
#[test]
fn load_missing_file_is_empty_object() {
    let p = tmp("load-missing");
    // tmp() creates the dir but not the file
    let v = load(&p).unwrap();
    assert!(v.is_object() && v.as_object().unwrap().is_empty());
}

#[test]
fn load_invalid_json_is_err_not_empty() {
    let p = tmp("load-bad");
    std::fs::write(&p, "{ not json").unwrap();
    assert!(load(&p).is_err(), "invalid JSON must abort, not coerce to empty");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --bin psmux load_invalid_json_is_err_not_empty -- --nocapture`
Expected: FAIL — current `load` returns `Ok({})` on the parse error path only for read errors, but the invalid-JSON case already errors; the NEW guard is the read-error path. Confirm `load_missing_file_is_empty_object` passes and the invalid case behavior.

- [ ] **Step 3: Rewrite `load`**

Replace `src/hooks_install.rs:26-32`:

```rust
fn load(path: &Path) -> Result<serde_json::Value, String> {
    match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(serde_json::json!({})),
        Ok(s) => serde_json::from_str(&s).map_err(|e| format!("{}: invalid JSON: {}", path.display(), e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(format!("{}: cannot read (refusing to overwrite): {}", path.display(), e)),
    }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --bin psmux load_ -- --nocapture`
Expected: both tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/hooks_install.rs tests-rs/test_hooks_install.rs
git commit -m "fix(hooks): load() aborts on read errors, NotFound-only means empty"
```

---

## Task 2: Harden `atomic_write()` — true rename-over, never delete-then-rename

**Files:**
- Modify: `src/hooks_install.rs:57-64` (`atomic_write`)
- Test: `tests-rs/test_hooks_install.rs` (append)

**Interfaces:**
- Produces: `atomic_write(path, contents) -> Result<(), String>` — never leaves the target missing; replaces in place.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn atomic_write_replaces_without_transient_delete() {
    let p = tmp("atomic");
    std::fs::write(&p, "OLD").unwrap();
    atomic_write(&p, "NEW").unwrap();
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "NEW");
    // No stray tmp/backup left in the dir besides the file itself.
    let dir = p.parent().unwrap();
    let leftovers: Vec<_> = std::fs::read_dir(dir).unwrap()
        .filter_map(|e| e.ok()).map(|e| e.file_name().into_string().unwrap())
        .filter(|n| n.contains("psmux-tmp")).collect();
    assert!(leftovers.is_empty(), "temp file leaked: {:?}", leftovers);
}
```

- [ ] **Step 2: Run to verify it fails or passes on current code**

Run: `cargo test --bin psmux atomic_write_replaces -- --nocapture`
Expected: current code does `remove_file` then `rename` — the leak check may pass but the transient-delete window exists. This test locks the post-fix invariant (file always present, tmp cleaned).

- [ ] **Step 3: Rewrite `atomic_write`**

Replace `src/hooks_install.rs:57-64`:

```rust
fn atomic_write(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
    }
    let tmp = path.with_extension("json.psmux-tmp");
    std::fs::write(&tmp, contents).map_err(|e| e.to_string())?;
    // Rename directly over the target (atomic replace on Windows & Unix when
    // same directory). Do NOT remove the original first — a crash between a
    // delete and a rename would leave the file missing.
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Best-effort cleanup so a failed replace doesn't leak the tmp.
            let _ = std::fs::remove_file(&tmp);
            Err(format!("{}: atomic replace failed: {}", path.display(), e))
        }
    }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --bin psmux atomic_write_replaces -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/hooks_install.rs tests-rs/test_hooks_install.rs
git commit -m "fix(hooks): atomic_write uses rename-over, never transient delete"
```

---

## Task 3: `backup()` — must-succeed-on-change, skip-on-no-op

**Files:**
- Modify: `src/hooks_install.rs:66-72` (`backup`), and call sites `install_claude` (:96) / `uninstall_claude` (:115-116)
- Test: `tests-rs/test_hooks_install.rs` (append)

**Interfaces:**
- Produces: `backup(path) -> Result<Option<PathBuf>, String>` (signature CHANGED from `-> Option<PathBuf>`): `Ok(None)` if file absent (nothing to back up), `Ok(Some(bak))` on success, `Err` if the copy fails. Callers back up ONLY after deciding a change will happen.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn no_backup_on_noop_uninstall() {
    let p = tmp("noop-uninstall");
    std::fs::write(&p, r#"{"hooks":{}}"#).unwrap(); // nothing owned
    let r = uninstall_claude(&p).unwrap();
    assert!(!r.changed);
    assert!(r.backup.is_none(), "no-op uninstall must not create a backup");
    let dir = p.parent().unwrap();
    let baks: Vec<_> = std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok())
        .map(|e| e.file_name().into_string().unwrap())
        .filter(|n| n.contains(".bak-")).collect();
    assert!(baks.is_empty(), "backup leaked on no-op: {:?}", baks);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --bin psmux no_backup_on_noop_uninstall -- --nocapture`
Expected: FAIL — current `uninstall_claude` backs up BEFORE the `strip_owned` no-op check (:115-116).

- [ ] **Step 3: Change `backup` signature + reorder callers**

Replace `src/hooks_install.rs:66-72`:

```rust
fn backup(path: &Path) -> Result<Option<PathBuf>, String> {
    if !path.exists() { return Ok(None); }
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis()).unwrap_or(0);
    let bak = path.with_extension(format!("json.bak-{}", ms));
    std::fs::copy(path, &bak).map_err(|e| format!("{}: backup failed: {}", path.display(), e))?;
    Ok(Some(bak))
}
```

In `install_claude` (around :96), move the backup to AFTER the `already`-installed early return and BEFORE `strip_owned`, and propagate the error:

```rust
        if already { return Ok(InstallReport { changed: false, backup: None }); }
        let bak = backup(settings_path)?;
```

In `uninstall_claude` (:113-119), compute ownership FIRST, back up only if changing:

```rust
        let mut v = load(settings_path)?;
        if !strip_owned(&mut v) { return Ok(InstallReport { changed: false, backup: None }); }
        let bak = backup(settings_path)?;
        atomic_write(settings_path, &serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?)?;
        Ok(InstallReport { changed: true, backup: bak })
```

- [ ] **Step 4: Run to verify pass (and the full hooks suite still green)**

Run: `cargo test --bin psmux install_ uninstall_ no_backup_ -- --nocapture`
Expected: PASS, no regressions.

- [ ] **Step 5: Commit**

```bash
git add src/hooks_install.rs tests-rs/test_hooks_install.rs
git commit -m "fix(hooks): backup must-succeed-on-change, skip on no-op"
```

---

## Task 4: Per-agent marker + exact-match idempotency helpers

**Files:**
- Modify: `src/hooks_install.rs` (`OWNED_MARKER` const → `owned_marker(agent)`, add `desired_group`, `event_has_exact`)
- Test: `tests-rs/test_hooks_install.rs` (append)

**Interfaces:**
- Produces:
  - `fn owned_marker(agent: &str) -> String` → `format!(" hook-notify {} ", agent)`
  - `fn desired_group(psmux_exe: &Path, agent: &str, arg: &str) -> serde_json::Value` → the exact `{hooks:[{type,command,timeout}]}` group.
  - `fn event_has_exact(arr: &serde_json::Value, want: &serde_json::Value) -> bool` → true iff some group in the event array equals `want`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn exact_match_detects_stale_command() {
    let exe = std::path::Path::new("C:\\bin\\psmux.exe");
    let want = desired_group(exe, "codex", "stop");
    // A stale group with a DIFFERENT exe path must NOT count as exact.
    let stale = serde_json::json!({"hooks":[{"type":"command",
        "command":"\"C:\\\\old\\\\psmux.exe\" hook-notify codex stop","timeout":10}]});
    let arr = serde_json::json!([stale]);
    assert!(!event_has_exact(&arr, &want), "stale exe must not match exact");
    let arr2 = serde_json::json!([want.clone()]);
    assert!(event_has_exact(&arr2, &want), "identical group must match");
}

#[test]
fn owned_marker_is_per_agent() {
    assert_eq!(owned_marker("codex"), " hook-notify codex ");
    assert_eq!(owned_marker("gemini"), " hook-notify gemini ");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --bin psmux exact_match_detects_stale_command owned_marker_is_per_agent -- --nocapture`
Expected: FAIL — functions don't exist yet.

- [ ] **Step 3: Add the helpers, replace the const**

Remove `const OWNED_MARKER` (:8). Add:

```rust
fn owned_marker(agent: &str) -> String { format!(" hook-notify {} ", agent) }

fn desired_group(psmux_exe: &Path, agent: &str, arg: &str) -> serde_json::Value {
    serde_json::json!({
        "hooks": [{
            "type": "command",
            "command": format!("\"{}\" hook-notify {} {}", psmux_exe.display(), agent, arg),
            "timeout": 10
        }]
    })
}

fn event_has_exact(arr: &serde_json::Value, want: &serde_json::Value) -> bool {
    arr.as_array().map(|gs| gs.iter().any(|g| g == want)).unwrap_or(false)
}
```

Update `is_owned` AND `strip_owned` to take the agent marker (thread it now so Task 5 can call them):

```rust
fn is_owned(group: &serde_json::Value, marker: &str) -> bool {
    group["hooks"].as_array().map(|hs| {
        hs.iter().any(|h| h["command"].as_str().map(|c| c.contains(marker)).unwrap_or(false))
    }).unwrap_or(false)
}

fn strip_owned(v: &mut serde_json::Value, marker: &str) -> bool {
    let mut changed = false;
    if let Some(events) = v.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for (_ev, arr) in events.iter_mut() {
            if let Some(groups) = arr.as_array_mut() {
                let before = groups.len();
                groups.retain(|g| !is_owned(g, marker));
                changed |= groups.len() != before;
            }
        }
    }
    changed
}
```

(Every existing `is_owned(g)` call becomes `is_owned(g, &marker)`; `strip_owned(&mut v)` becomes `strip_owned(&mut v, &marker)`; `install_claude`, `status_claude` pass `&owned_marker("claude")`. `hook_entry` is superseded by `desired_group` — replace its uses.)

- [ ] **Step 4: Run to verify pass + full suite green**

Run: `cargo test --bin psmux hooks -- --nocapture` then `cargo test --bin psmux`
Expected: new tests PASS, no regressions (all prior hooks tests updated to the marker-arg signature).

- [ ] **Step 5: Commit**

```bash
git add src/hooks_install.rs tests-rs/test_hooks_install.rs
git commit -m "feat(hooks): per-agent marker + exact-match idempotency helpers"
```

---

## Task 5: Generalize the merge into `install_json_hooks` (claude + codex)

**Files:**
- Modify: `src/hooks_install.rs` (`install_claude` → thin wrapper over new `install_json_hooks`; add `install_codex`, `uninstall_agent`, `status_agent`)
- Test: `tests-rs/test_hooks_install.rs` (append)

**Interfaces:**
- Consumes: Task 4 helpers.
- Produces:
  - `fn install_json_hooks(path: &Path, exe: &Path, agent: &str, events: &[(&str,&str)]) -> Result<InstallReport, String>` — `events` = list of `(file_key, arg)`.
  - `pub fn install_codex(path: &Path, exe: &Path) -> Result<InstallReport, String>` = `install_json_hooks(path, exe, "codex", &[("Stop","stop")])`.
  - `pub fn install_claude(...)` unchanged public signature, now delegates with `&[("Stop","stop"),("SessionStart","session-start"),("SessionEnd","session-end")]`.
  - `pub fn uninstall_agent(path, agent)` / `pub fn status_agent(path, agent, events)` generic over agent.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn install_codex_writes_stop_only_exact() {
    let p = tmp("codex-fresh");
    let exe = std::path::Path::new("C:\\bin\\psmux.exe");
    let r = install_codex(&p, exe).unwrap();
    assert!(r.changed);
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    let cmd = v["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
    assert!(cmd.contains("hook-notify codex stop"));
    assert!(v["hooks"]["SessionStart"].is_null(), "codex is done-signal only");
    // Re-install is a no-op (exact match) — no second write.
    let r2 = install_codex(&p, exe).unwrap();
    assert!(!r2.changed, "exact re-install must be a no-op");
}

#[test]
fn install_codex_preserves_sibling_group() {
    let p = tmp("codex-sibling");
    std::fs::write(&p, r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"other.exe"}]}]}}"#).unwrap();
    install_codex(&p, std::path::Path::new("C:\\bin\\psmux.exe")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    let groups = v["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "must ADD alongside the existing group, not clobber");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --bin psmux install_codex -- --nocapture`
Expected: FAIL — `install_codex` doesn't exist.

- [ ] **Step 3: Implement `install_json_hooks` + `install_codex`, rewire `install_claude`**

```rust
fn install_json_hooks(path: &Path, exe: &Path, agent: &str, events: &[(&str,&str)]) -> Result<InstallReport, String> {
    let marker = owned_marker(agent);
    let lock = acquire_lock(path)?;
    let result = (|| {
        let mut v = load(path)?;
        // "installed" = every event already contains our EXACT desired group.
        let already = events.iter().all(|(file_key, arg)| {
            let want = desired_group(exe, agent, arg);
            v["hooks"][file_key].as_array().map(|_| event_has_exact(&v["hooks"][file_key], &want)).unwrap_or(false)
        });
        if already { return Ok(InstallReport { changed: false, backup: None }); }
        let bak = backup(path)?;
        strip_owned(&mut v, &marker); // remove any stale owned groups first
        if !v["hooks"].is_object() { v["hooks"] = serde_json::json!({}); }
        for (file_key, arg) in events {
            if !v["hooks"][file_key].is_array() { v["hooks"][file_key] = serde_json::json!([]); }
            v["hooks"][file_key].as_array_mut().unwrap().push(desired_group(exe, agent, arg));
        }
        atomic_write(path, &serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?)?;
        write_manifest(path, agent)?;
        Ok(InstallReport { changed: true, backup: bak })
    })();
    let _ = std::fs::remove_file(lock);
    result
}

pub fn install_codex(path: &Path, exe: &Path) -> Result<InstallReport, String> {
    install_json_hooks(path, exe, "codex", &[("Stop", "stop")])
}

pub fn install_claude(settings_path: &Path, psmux_exe: &Path) -> Result<InstallReport, String> {
    install_json_hooks(settings_path, psmux_exe, "claude",
        &[("Stop","stop"), ("SessionStart","session-start"), ("SessionEnd","session-end")])
}
```

Update `strip_owned` to take `marker: &str` and `write_manifest` to take `agent: &str` (Task 6 finalizes the manifest; here just thread the arg — `m[agent] = ...`).

- [ ] **Step 4: Run to verify pass + full suite**

Run: `cargo test --bin psmux install_codex install_into -- --nocapture` then `cargo test --bin psmux`
Expected: PASS, claude tests still green (delegation preserved behavior).

- [ ] **Step 5: Commit**

```bash
git add src/hooks_install.rs tests-rs/test_hooks_install.rs
git commit -m "feat(hooks): install_json_hooks generalizes claude; add install_codex"
```

---

## Task 6: `uninstall_agent` / `status_agent` + manifest generalization

**Files:**
- Modify: `src/hooks_install.rs` (`uninstall_claude` → `uninstall_agent`; `status_claude` → `status_agent`; `write_manifest(path, agent)`; add `clear_manifest(agent)`)
- Test: `tests-rs/test_hooks_install.rs` (append)

**Interfaces:**
- Produces:
  - `pub fn uninstall_agent(path: &Path, agent: &str) -> Result<InstallReport, String>` — strips only the agent's owned command; prunes group only when empty; removes psmux-created empty `hooks` container; clears the agent's manifest entry.
  - `pub fn status_agent(path: &Path, agent: &str, events: &[(&str,&str)], exe: &Path) -> String` — reports installed/not + embedded exe path + whether it still exists, validating EVERY event.
  - back-compat: `uninstall_claude`/`status_claude` delegate.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn uninstall_removes_only_owned_command_keeps_sibling() {
    let p = tmp("codex-uninstall");
    let exe = std::path::Path::new("C:\\bin\\psmux.exe");
    install_codex(&p, exe).unwrap();
    // Add a user sibling command INTO psmux's own group.
    {
        let mut v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        v["hooks"]["Stop"][0]["hooks"].as_array_mut().unwrap()
            .push(serde_json::json!({"type":"command","command":"user-thing.exe"}));
        std::fs::write(&p, v.to_string()).unwrap();
    }
    uninstall_agent(&p, "codex").unwrap();
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    let cmds = v["hooks"]["Stop"][0]["hooks"].as_array().unwrap();
    let joined: String = cmds.iter().map(|c| c["command"].as_str().unwrap_or("")).collect();
    assert!(!joined.contains("hook-notify codex"), "psmux command must be gone");
    assert!(joined.contains("user-thing.exe"), "user sibling must survive");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --bin psmux uninstall_removes_only_owned -- --nocapture`
Expected: FAIL — current `strip_owned` removes the whole GROUP (`user-thing.exe` would be deleted too).

- [ ] **Step 3: Implement command-level strip + generic uninstall/status/manifest**

Add a command-level strip (replaces group-level `strip_owned` for uninstall):

```rust
/// Remove only owned COMMANDS; drop a group only if it becomes empty; drop an
/// event array if it becomes empty. Returns whether anything changed.
fn strip_owned_commands(v: &mut serde_json::Value, marker: &str) -> bool {
    let mut changed = false;
    if let Some(events) = v.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for (_ev, arr) in events.iter_mut() {
            if let Some(groups) = arr.as_array_mut() {
                for g in groups.iter_mut() {
                    if let Some(cmds) = g.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                        let before = cmds.len();
                        cmds.retain(|c| !c["command"].as_str().map(|s| s.contains(marker)).unwrap_or(false));
                        changed |= cmds.len() != before;
                    }
                }
                let gb = groups.len();
                groups.retain(|g| g["hooks"].as_array().map(|h| !h.is_empty()).unwrap_or(false));
                changed |= groups.len() != gb;
            }
        }
        let eb = events.len();
        events.retain(|_k, arr| arr.as_array().map(|a| !a.is_empty()).unwrap_or(false));
        changed |= events.len() != eb;
    }
    changed
}

pub fn uninstall_agent(path: &Path, agent: &str) -> Result<InstallReport, String> {
    let marker = owned_marker(agent);
    let lock = acquire_lock(path)?;
    let result = (|| {
        let mut v = load(path)?;
        if !strip_owned_commands(&mut v, &marker) { return Ok(InstallReport { changed: false, backup: None }); }
        let bak = backup(path)?;
        // Drop an empty psmux-created hooks container.
        if v["hooks"].as_object().map(|o| o.is_empty()).unwrap_or(false) {
            if let Some(o) = v.as_object_mut() { o.remove("hooks"); }
        }
        atomic_write(path, &serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?)?;
        let _ = clear_manifest(agent);
        Ok(InstallReport { changed: true, backup: bak })
    })();
    let _ = std::fs::remove_file(lock);
    result
}

pub fn status_agent(path: &Path, agent: &str, events: &[(&str,&str)], exe: &Path) -> String {
    match load(path) {
        Ok(v) => {
            let all = events.iter().all(|(fk, arg)| {
                let want = desired_group(exe, agent, arg);
                event_has_exact(&v["hooks"][fk], &want)
            });
            let exists = exe.exists();
            let trust = if agent == "codex" && all { format!(" [{}]", codex_trust_state(path)) } else { String::new() };
            format!("{}: {}{} (exe: {} {}) ({})", agent,
                if all { "installed" } else { "not installed" }, trust,
                exe.display(), if exists { "present" } else { "MISSING" }, path.display())
        }
        Err(e) => format!("{}: unreadable ({})", agent, e),
    }
}

/// Codex gates its hooks.json by a content hash recorded in config.toml
/// [hooks.state.'<path>:stop:0:0'].trusted_hash. Report whether the current
/// hooks.json content matches a recorded trust entry (spec §7). Best-effort:
/// unknown => "trust-unknown" (never claims trusted when it can't prove it).
fn codex_trust_state(hooks_path: &Path) -> &'static str {
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_default();
    let cfg = Path::new(&home).join(".codex").join("config.toml");
    let (Ok(cfg_s), Ok(hooks_s)) = (std::fs::read_to_string(&cfg), std::fs::read_to_string(hooks_path)) else {
        return "trust-unknown";
    };
    // Recorded hash form: sha256:<hex> under a [hooks.state.'...hooks.json:stop:0:0'] block.
    // We only need to know a trusted_hash line referencing this hooks.json exists AND
    // matches the current file's sha256. Compute sha256 of hooks_s; compare.
    let want = sha256_hex(hooks_s.as_bytes());
    if cfg_s.contains("hooks.json:stop") && cfg_s.contains(&format!("sha256:{}", want)) {
        "trusted"
    } else if cfg_s.contains("hooks.json:stop") {
        "trust-pending"
    } else {
        "trust-unknown"
    }
}
```

Add a small `sha256_hex(&[u8]) -> String` helper (reuse the crate's existing sha256 if one is imported for the manifest/identity code; otherwise a minimal impl). Add `clear_manifest(agent)` mirroring `write_manifest` (remove `m[agent]`, atomic-ish write). Delegate `uninstall_claude`/`status_claude` to the generic forms.

**Verify during implementation:** grep for an existing sha256 helper (`grep -rn "sha256\|Sha256" src/`) before adding a new one — increment 1's manifest/trust code may already provide one. If codex's recorded hash is computed over a normalized form (not raw bytes), downgrade `codex_trust_state` to report only `trusted-entry-present` vs `trust-pending` (presence of the `hooks.json:stop` block) rather than a byte-hash match, so it never falsely claims "trusted". The status string is advisory; the load-bearing defense is the install-time warning + relay `--timeout`.

- [ ] **Step 4: Run to verify pass + full suite**

Run: `cargo test --bin psmux uninstall status -- --nocapture` then `cargo test --bin psmux`
Expected: PASS, no regressions.

- [ ] **Step 5: Commit**

```bash
git add src/hooks_install.rs tests-rs/test_hooks_install.rs
git commit -m "feat(hooks): command-level uninstall + generic status/manifest per agent"
```

---

## Task 7: `install_gemini` — settings.json-nested, credential-safe — **DROPPED (codex-only scope)**

> **NOT IMPLEMENTED.** Scope revised to codex-only 2026-07-15: antigravity (`agy`, the gemini-family
> CLI in use) has no hook mechanism, and the Google gemini CLI is not used. Skip this task entirely.
> The original spec below is retained for the record only, in case a gemini installer is revived later.

**Files:**
- Modify: `src/hooks_install.rs` (add `install_gemini`; reuse hardened primitives + generic uninstall/status)
- Test: `tests-rs/test_hooks_install.rs` (append)

**Interfaces:**
- Consumes: hardened `load`/`atomic_write`/`backup`, `desired_group`, `event_has_exact`, `strip_owned`.
- Produces: `pub fn install_gemini(path: &Path, exe: &Path) -> Result<InstallReport, String>` — wires `AfterAgent` file-key with hook-notify arg `agent-turn-complete`; preserves all non-hooks keys.

**Note:** gemini's shape is the SAME `hooks.<Event>` object nested in settings.json. So `install_json_hooks` already works IF we point it at the gemini file with `&[("AfterAgent","agent-turn-complete")]` and the "gemini" agent marker — because it merges under `v["hooks"]` and leaves sibling top-level keys (`security`, `general`) untouched. This task VERIFIES that with a credential-preservation test rather than writing a separate merger (spec §5 MINOR: share one merge).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn install_gemini_preserves_oauth_and_general() {
    let p = tmp("gemini");
    std::fs::write(&p, r#"{"general":{"sessionRetention":{"enabled":true}},"security":{"auth":{"selectedType":"oauth-personal"}}}"#).unwrap();
    let exe = std::path::Path::new("C:\\bin\\psmux.exe");
    let r = install_gemini(&p, exe).unwrap();
    assert!(r.changed);
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert_eq!(v["security"]["auth"]["selectedType"], "oauth-personal", "oauth MUST survive");
    assert_eq!(v["general"]["sessionRetention"]["enabled"], true, "general MUST survive");
    let cmd = v["hooks"]["AfterAgent"][0]["hooks"][0]["command"].as_str().unwrap();
    assert!(cmd.contains("hook-notify gemini agent-turn-complete"));
}

#[test]
fn uninstall_gemini_restores_clean_and_keeps_oauth() {
    let p = tmp("gemini-uninstall");
    std::fs::write(&p, r#"{"security":{"auth":{"selectedType":"oauth-personal"}}}"#).unwrap();
    let exe = std::path::Path::new("C:\\bin\\psmux.exe");
    install_gemini(&p, exe).unwrap();
    uninstall_agent(&p, "gemini").unwrap();
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert_eq!(v["security"]["auth"]["selectedType"], "oauth-personal");
    assert!(v["hooks"].is_null(), "psmux-created hooks container removed on uninstall");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --bin psmux gemini -- --nocapture`
Expected: FAIL — `install_gemini` doesn't exist.

- [ ] **Step 3: Implement `install_gemini`**

```rust
pub fn install_gemini(path: &Path, exe: &Path) -> Result<InstallReport, String> {
    install_json_hooks(path, exe, "gemini", &[("AfterAgent", "agent-turn-complete")])
}
```

(No separate merger — `install_json_hooks` merges under `hooks` and leaves `security`/`general` alone. The tests prove it.)

- [ ] **Step 4: Run to verify pass + full suite**

Run: `cargo test --bin psmux gemini -- --nocapture` then `cargo test --bin psmux`
Expected: PASS, oauth/general preserved through install AND uninstall.

- [ ] **Step 5: Commit**

```bash
git add src/hooks_install.rs tests-rs/test_hooks_install.rs
git commit -m "feat(hooks): install_gemini reuses shared merge, preserves user config"
```

---

## Task 8: Real-tmux warning fix (`warn_if_warm_claimed_pane`)

**Files:**
- Modify: `src/main.rs:4709-4720` (`is_warm_claimed_pane` / `warn_if_warm_claimed_pane`)
- Test: `src/main.rs` inline `#[cfg(test)]` (find the existing `is_warm_claimed_pane` test module) or `tests-rs`

**Interfaces:**
- Produces: `is_warm_claimed_pane` gains a `session_uid` arg so the warning fires ONLY when a psmux-specific marker (`PSMUX_SESSION_UID`) is present — i.e. genuinely inside psmux — not in plain tmux.

- [ ] **Step 1: Write the failing test**

Add near the existing `is_warm_claimed_pane` tests:

```rust
#[test]
fn plain_tmux_without_psmux_marker_is_not_warm_claimed() {
    // TMUX_PANE set (plain tmux), no instance, and crucially no PSMUX_SESSION_UID.
    assert!(!is_warm_claimed_pane(Some("%3"), None, ""));
    // Inside psmux (session uid present) but instance missing => warm-claimed.
    assert!(is_warm_claimed_pane(Some("%3"), None, "sess-uid-abc"));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --bin psmux plain_tmux_without_psmux_marker -- --nocapture`
Expected: FAIL — `is_warm_claimed_pane` currently takes 2 args.

- [ ] **Step 3: Add the marker gate**

Replace `is_warm_claimed_pane` (`src/main.rs:4709-4712`):

```rust
pub(crate) fn is_warm_claimed_pane(tmux_pane: Option<&str>, pane_instance: Option<&str>, session_uid: &str) -> bool {
    !session_uid.is_empty() // only inside psmux at all
        && tmux_pane.map(|v| !v.is_empty()).unwrap_or(false)
        && pane_instance.map(|v| v.is_empty()).unwrap_or(true)
}
```

Update `warn_if_warm_claimed_pane` (:4715-4721) to read + pass `PSMUX_SESSION_UID`:

```rust
fn warn_if_warm_claimed_pane() {
    let tmux_pane = std::env::var("TMUX_PANE").ok();
    let pane_instance = std::env::var("PSMUX_PANE_INSTANCE").ok();
    let session_uid = std::env::var("PSMUX_SESSION_UID").unwrap_or_default();
    if is_warm_claimed_pane(tmux_pane.as_deref(), pane_instance.as_deref(), &session_uid) {
        eprintln!("psmux: this pane has no identity (warm-claimed initial pane?) - notify skipped; see docs/agent-events.md warm caveat or set PSMUX_NO_WARM=1");
    }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --bin psmux warm_claimed is_warm -- --nocapture`
Expected: PASS. Update any existing 2-arg callers in tests.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "fix(notify): warm-claimed warning gated on PSMUX_SESSION_UID, silent in plain tmux"
```

---

## Task 9: CLI wiring — codex/gemini install/uninstall/status + trust-hash warn

**Files:**
- Modify: `src/main.rs:3900-3936` (the `"hooks"` arm) + `src/cli.rs` help text (:201-205, :452-521)
- Test: covered by live gate (Task 0) + a small unit test on codex trust-status if feasible

**Interfaces:**
- Consumes: `install_codex`/`install_gemini`/`uninstall_agent`/`status_agent`.
- Produces: CLI verbs per spec §9. codex/gemini reject `--project-local`; no-home fails; codex install prints the trust-hash warning.

- [ ] **Step 1: Rewrite the `"hooks"` arm**

Replace the claude-only guard (`src/main.rs:3904-3907`) and body with a per-agent dispatch:

```rust
"hooks" => {
    let sub = cmd_args.get(1).map(|s| s.as_str()).unwrap_or("");
    let agent = cmd_args.get(2).map(|s| s.as_str()).unwrap_or("");
    let project_local = cmd_args.iter().any(|a| a.as_str() == "--project-local");
    let exe = std::env::current_exe()?;
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_default();

    // Resolve settings path + event list + trust-warn per agent.
    let (settings, events): (std::path::PathBuf, &[(&str,&str)]) = match agent {
        "claude" => {
            let p = if project_local { std::path::PathBuf::from(".claude").join("settings.local.json") }
                    else {
                        if home.is_empty() { eprintln!("psmux hooks: no home dir"); std::process::exit(1); }
                        std::path::Path::new(&home).join(".claude").join("settings.json")
                    };
            (p, &[("Stop","stop"),("SessionStart","session-start"),("SessionEnd","session-end")])
        }
        "codex" | "gemini" => {
            if project_local { eprintln!("psmux hooks: --project-local is not valid for {} (global only)", agent); std::process::exit(1); }
            if home.is_empty() { eprintln!("psmux hooks: no home dir"); std::process::exit(1); }
            if agent == "codex" {
                (std::path::Path::new(&home).join(".codex").join("hooks.json"), &[("Stop","stop")])
            } else {
                (std::path::Path::new(&home).join(".gemini").join("settings.json"), &[("AfterAgent","agent-turn-complete")])
            }
        }
        _ => { eprintln!("psmux hooks: agent must be claude|codex|gemini"); std::process::exit(1); }
    };

    match sub {
        "install" => {
            let r = match agent {
                "claude" => hooks_install::install_claude(&settings, &exe),
                "codex"  => hooks_install::install_codex(&settings, &exe),
                _        => hooks_install::install_gemini(&settings, &exe),
            }.map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            println!("{} hooks: {}{}", agent,
                if r.changed { "installed" } else { "already installed" },
                r.backup.map(|b| format!(" (backup: {})", b.display())).unwrap_or_default());
            if agent == "codex" && r.changed {
                eprintln!("psmux: codex will prompt to RE-TRUST hooks on next launch; until accepted, the done-signal will NOT fire. Relays must use `wait-event --timeout`.");
            }
        }
        "uninstall" => {
            let r = hooks_install::uninstall_agent(&settings, agent)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            println!("{} hooks: {}", agent, if r.changed { "removed" } else { "nothing to remove" });
        }
        "status" | "doctor" => println!("{}", hooks_install::status_agent(&settings, agent, events, &exe)),
        _ => { eprintln!("usage: psmux hooks <install|uninstall|status> claude|codex|gemini [--project-local]"); std::process::exit(1); }
    }
    return Ok(());
}
```

- [ ] **Step 2: Update help text** in `src/cli.rs` (:201-205 short, :452-521 long) to list `codex|gemini` and note global-only + trust-hash re-prompt.

- [ ] **Step 3: Build + smoke the CLI (dry, non-live)**

Run: `cargo build --release --bin psmux && ./target/release/psmux.exe hooks status codex`
Expected: prints `codex: not installed (exe: ...present...) (~/.codex/hooks.json)` (or installed if you ran it). `hooks install codex --project-local` prints the reject message and exits 1.

- [ ] **Step 4: Full unit suite**

Run: `cargo test --bin psmux`
Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs src/cli.rs
git commit -m "feat(cli): hooks install/uninstall/status for codex & gemini + trust-hash warn"
```

---

## Task 10: Live end-to-end done-signal verification (gated by Task 0 PASS)

**Files:**
- Create: `tests/e2e_done_signal_codex_gemini.ps1`

**Interfaces:**
- Consumes: the shipped installers.
- Produces: PASS proof that codex `Stop` and gemini `AfterAgent` each wake `wait-event --name agent-done --pane %N` with correct attribution.

- [ ] **Step 1: Write the e2e script**

Create `tests/e2e_done_signal_codex_gemini.ps1` that, per agent: sets `PSMUX_ALLOW_ELEVATED=1`; `gridup` a 1-pane grid running the agent; `psmux hooks install <agent>`; (codex) accept the re-trust prompt; start `wait-event --name agent-done --pane %1 --timeout 60` in the background; send the agent a trivial prompt; assert `wait-event` exits 0 with a JSON event whose `pane` matches; `hooks uninstall <agent>`; `griddown`.

- [ ] **Step 2: Run live for each PASS agent from Task 0**

Expected: `wait-event` returns the `agent-done` event (exit 0), pane attribution correct. Record results.

- [ ] **Step 3: Commit**

```bash
git add tests/e2e_done_signal_codex_gemini.ps1
git commit -m "test(e2e): live done-signal via codex/gemini installed hooks"
```

---

## Task 11: Docs + spec sync

**Files:**
- Modify: `docs/agent-events.md` (add codex/gemini installer section, trust-hash note, timeout discipline)
- Modify: the spec's status line → implemented

- [ ] **Step 1: Document** `psmux hooks install codex|gemini`, the global-only footprint, the codex re-trust prompt, and the `wait-event --timeout` relay discipline.

- [ ] **Step 2: Commit**

```bash
git add docs/agent-events.md docs/superpowers/specs/2026-07-15-agent-done-signal-installers-design.md
git commit -m "docs: codex/gemini done-signal installers + trust/timeout guidance"
```

---

## Task 12: Fix codex hook command format — PowerShell call-operator (ADDED post-e2e root-cause)

**Why (root cause, triple-confirmed):** codex 0.144.4 runs a hooks.json `type:"command"` string via
`powershell.exe -NoProfile -Command <string>`. The installed form `"<exe>" hook-notify codex stop`
is a PowerShell parser error ("Unexpected token 'hook-notify'") → PS exits 1 → codex reports
"Stop hook (failed): hook exited with code 1" → no agent-done published. Verified: the
call-operator form `& '<exe>' hook-notify codex stop` prints `{}` exit 0 under the same runner.
`hook-notify` itself is flawless in codex's real context (wrapper diag: identity env present, exit 0).
Claude's runner handles the old quoted form fine — claude format UNCHANGED.

**Files:**
- Modify: `src/hooks_install.rs` (`desired_group` — per-agent command format)
- Test: `tests-rs/test_hooks_install.rs` (update codex assertions, add format test)

**Interfaces:** `desired_group(psmux_exe, agent, arg)` signature unchanged; codex command string
becomes `& '<exe>' hook-notify codex <arg>` (single quotes in path escaped by doubling).
Marker `" hook-notify codex "` still present in the string — is_owned/strip logic unchanged.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn codex_command_uses_powershell_call_operator() {
    let exe = std::path::Path::new("C:\bin\psmux.exe");
    let g = desired_group(exe, "codex", "stop");
    let cmd = g["hooks"][0]["command"].as_str().unwrap();
    assert_eq!(cmd, "& 'C:\bin\psmux.exe' hook-notify codex stop",
        "codex hooks run via powershell -Command; must use call-operator form");
    // claude keeps the proven quoted form
    let gc = desired_group(exe, "claude", "stop");
    let cmdc = gc["hooks"][0]["command"].as_str().unwrap();
    assert_eq!(cmdc, "\"C:\bin\psmux.exe\" hook-notify claude stop");
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test --bin psmux codex_command_uses_powershell -- --nocapture` → FAIL (current format is the quoted form for all agents).

- [ ] **Step 3: Implement** — in `desired_group`, build the command per agent:

```rust
fn desired_group(psmux_exe: &Path, agent: &str, arg: &str) -> serde_json::Value {
    // codex executes hook commands via `powershell.exe -NoProfile -Command <string>`;
    // a bare "quoted-exe" args string is a PS parser error (exit 1, hook reported failed).
    // The call operator with a single-quoted path is the form PS actually invokes.
    // claude's runner handles the plain quoted form — keep it (proven since increment 1).
    let command = if agent == "codex" {
        format!("& '{}' hook-notify {} {}", psmux_exe.display().to_string().replace('\'', "''"), agent, arg)
    } else {
        format!("\"{}\" hook-notify {} {}", psmux_exe.display(), agent, arg)
    };
    serde_json::json!({ "hooks": [{ "type": "command", "command": command, "timeout": 10 }] })
}
```

- [ ] **Step 4: Fix any codex-test assertions** that expected the old format (e.g. `install_codex_writes_stop_only_exact` checks `contains("hook-notify codex stop")` — still passes; any exact-string assertions need the new form). Run `cargo test --bin psmux hooks` → all green.

- [ ] **Step 5: Commit**

```bash
git add src/hooks_install.rs tests-rs/test_hooks_install.rs
git commit -m "fix(hooks): codex command uses PowerShell call-operator form"
```

**Follow-up (same task):** spec §7 note — codex `timeout` is SECONDS; hook config is captured at
codex session start (install BEFORE launching codex). Optional future: `commandWindows` field
(unverified in 0.144.4; not used).

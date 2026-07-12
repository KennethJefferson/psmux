# Agent Events Increment 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deterministic agent orchestration in psmux: server-minted identity, in-memory event bus, `events`/`cursor`/`wait-event`/`notify`/`hook-notify` verbs, `capture-pane --settle`, Claude Code hook installer, unified shutdown, pre-auth hardening, elevated refusal.

**Architecture:** psmux is one session per server process; a single-threaded main loop owns `AppState` and processes `CtrlReq` messages from per-connection TCP threads. The event bus lives inside the main loop (no locking; subscribe/publish are serialized by construction). Connection threads stream events to clients via bounded channels, exactly like the existing control-mode notification writer. Spec: `docs/superpowers/specs/2026-07-11-agent-events-design.md`.

**Tech Stack:** Rust 2021, serde/serde_json (already deps), windows-sys 0.61, existing test patterns (`tests-rs/` via `#[path]` includes + `mock_app()`, PowerShell e2e in `tests/` via `_batch_runner.ps1`).

## Global Constraints

- Threat model: same-user same-integrity trust; identity tokens are for determinism, not auth (spec §2).
- Event serialized size cap: 16 KiB; ring capacity: 4096; subscriber channel capacity: 8192 (must exceed ring so replay never blocks); heartbeat: 15 s.
- `psmux notify` bounded-sync deadline: 2000 ms; always exits 0.
- `wait-event` exit codes: 0 match, 2 timeout, 3 cursor gap, 4 bus/session mismatch, 5 session-ended/bus-closed, 1 other error.
- Cursor wire format: `<session_uid>:<bus_id>:<seq>` (single token, colon-separated).
- Event names v1: `window-created`, `pane-exited`, `pane-bell`, `session-renamed`, `agent-done`, `agent-notify`, `stale-notify`, `bus-closed`. Categories: `window`, `pane`, `session`, `agent`, `bus`.
- Payloads: bounded metadata only (lengths, ids); content requires BOTH `--include-content` and server option `event-content on`, cap 4 KiB.
- Warm servers (`__warm__` session names, `session.rs::is_warm_session`): bus stays dormant; no events before claim.
- Popup panes: excluded from identity/events in v1.
- Protected pane env (installed last, matched case-insensitively): `TMUX`, `TMUX_PANE`, `PSMUX_SESSION`, `PSMUX_SESSION_UID`, `PSMUX_BUS_ID`, `PSMUX_PANE_INSTANCE`. User-settable exception: `PSMUX_HOOKS_DISABLED`.
- New-verb recipe (all four places or it does not work): `CtrlReq` variant in `src/types.rs` (~:1179) → CLI arm in `src/main.rs` match (before `_ =>` at ~:3751) → dispatch arm in `src/server/connection.rs` match (~:913..:3051, note `_ => {}` is a silent no-op) → loop handler in `src/server/mod.rs` big `match req`.
- Rust tests: add file under `tests-rs/`, include via `#[cfg(test)] #[path = "../tests-rs/<file>.rs"] mod <name>;` at the bottom of the src file whose privates it needs. Run: `cargo test <name>`.
- Commit after every task; message prefix `feat(events):`, `fix(env):`, `test(events):` as appropriate.

## File Structure

- Create: `src/events.rs` — Event, EventBus, Subscriber, cursor parse/format, uid generation. Single responsibility: bus mechanics; no AppState knowledge.
- Create: `src/hooks_install.rs` — Claude installer (lock, merge, atomic replace, backup, manifest, status/uninstall). Pure client-side; no server types.
- Modify: `src/types.rs` — AppState fields (`session_uid`, `bus`, `next_pane_instance`, `event_content`), `Pane.instance`, new `CtrlReq` variants.
- Modify: `src/pane.rs` — env ordering fix, instance assignment, `set_tmux_env` identity exports.
- Modify: `src/window_ops.rs` — respawn: new instance + env ordering fix.
- Modify: `src/server/mod.rs` — bus activation, producers at chokepoints (:5209 hook consumer, :5677 reap, :5341 bell), loop handlers for new CtrlReqs, unified shutdown, elevated refusal.
- Modify: `src/server/connection.rs` — pre-auth bounded read, new dispatch arms, events stream writer, wait-event deadline loop, capture-pane settle pre-wait.
- Modify: `src/main.rs` — CLI arms: `cursor`, `events`, `wait-event`, `notify`, `hook-notify`, `hooks`.
- Modify: `src/tree.rs` — reap returns transition records.
- Modify: `src/platform.rs` — `is_elevated()`.
- Modify: `Cargo.toml` — windows-sys features `Win32_Security`, `Win32_System_Threading`.
- Tests: `tests-rs/test_agent_events_bus.rs`, `tests-rs/test_agent_events_wiring.rs`, `tests-rs/test_env_protected.rs`, `tests-rs/test_pane_instance.rs`, `tests-rs/test_hooks_install.rs`; e2e `tests/test_agent_events_e2e.ps1`, `tests/test_agent_events_stale.ps1`, `tests/test_agent_events_shutdown.ps1`.

---

### Task 1: Elevated refusal + windows-sys features

**Files:**
- Modify: `Cargo.toml` (windows-sys features list, ~line 60)
- Modify: `src/platform.rs` (append at end)
- Modify: `src/server/mod.rs` (top of `run_server`, ~:725)
- Test: `tests-rs/test_elevation_gate.rs` (include from `src/platform.rs`)

**Interfaces:**
- Consumes: nothing.
- Produces: `platform::is_elevated() -> bool`; server exits 1 with message when elevated unless `PSMUX_ALLOW_ELEVATED=1`.

- [ ] **Step 1: Add windows-sys features**

In `Cargo.toml` extend the feature list:

```toml
windows-sys = { version = "0.61", features = [
    "Win32_Foundation",
    "Win32_System_Memory",
    "Win32_System_DataExchange",
    "Win32_Security",
    "Win32_System_Threading",
] }
```

- [ ] **Step 2: Write the failing test**

Create `tests-rs/test_elevation_gate.rs`:

```rust
use super::*;

#[test]
fn elevation_check_does_not_panic_and_is_stable() {
    let a = is_elevated();
    let b = is_elevated();
    assert_eq!(a, b);
}

#[test]
fn elevation_gate_env_override_recognized() {
    // gate helper: refuse only when elevated and override unset
    assert!(!should_refuse_elevated(false, None));
    assert!(!should_refuse_elevated(true, Some("1".into())));
    assert!(should_refuse_elevated(true, None));
    assert!(should_refuse_elevated(true, Some("0".into())));
}
```

At the bottom of `src/platform.rs` add:

```rust
#[cfg(test)]
#[path = "../tests-rs/test_elevation_gate.rs"]
mod test_elevation_gate;
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test test_elevation_gate`
Expected: FAIL to compile — `is_elevated` / `should_refuse_elevated` not found.

- [ ] **Step 4: Implement in `src/platform.rs`**

```rust
/// True when the current process token is elevated (Windows). Non-Windows: false.
#[cfg(windows)]
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elev = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut ret_len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elev as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        );
        CloseHandle(token);
        ok != 0 && elev.TokenIsElevated != 0
    }
}

#[cfg(not(windows))]
pub fn is_elevated() -> bool { false }

/// Pure gate logic (testable): refuse when elevated and override is not exactly "1".
pub fn should_refuse_elevated(elevated: bool, allow_env: Option<String>) -> bool {
    elevated && allow_env.as_deref() != Some("1")
}
```

Note: if `windows-sys 0.61` declares `OpenProcessToken`'s out-param as `*mut HANDLE` where `HANDLE = *mut c_void`, the code above is correct; if the build errors on pointer types, match the signature the compiler prints (e.g. `HANDLE` as `isize`: use `let mut token: HANDLE = 0;`). Do not cast blindly — fix to the actual signature.

- [ ] **Step 5: Gate the server**

In `src/server/mod.rs`, at the very top of `run_server` (first statements, ~:726):

```rust
if crate::platform::should_refuse_elevated(
    crate::platform::is_elevated(),
    std::env::var("PSMUX_ALLOW_ELEVATED").ok(),
) {
    eprintln!("psmux: refusing to run the server elevated (set PSMUX_ALLOW_ELEVATED=1 to override)");
    std::process::exit(1);
}
```

- [ ] **Step 6: Run tests and build**

Run: `cargo test test_elevation_gate && cargo build --release`
Expected: PASS, clean build.

- [ ] **Step 7: Commit**

```powershell
git add Cargo.toml src/platform.rs src/server/mod.rs tests-rs/test_elevation_gate.rs
git commit -m "feat(events): refuse elevated server unless PSMUX_ALLOW_ELEVATED=1"
```

---

### Task 2: Pre-auth hardening (bounded line reads)

**Files:**
- Modify: `src/server/connection.rs` (auth read ~:248; add helper near top of file)
- Test: `tests-rs/test_bounded_read.rs` (include from `src/server/connection.rs`)

**Interfaces:**
- Consumes: nothing.
- Produces: `read_line_bounded(r: &mut impl BufRead, cap: usize) -> std::io::Result<Option<String>>` — `Ok(None)` = line exceeded cap (protocol violation). Auth cap 1024 bytes; post-auth command cap 1 MiB.

- [ ] **Step 1: Write the failing test**

Create `tests-rs/test_bounded_read.rs`:

```rust
use super::*;
use std::io::BufReader;

#[test]
fn bounded_read_normal_line() {
    let mut r = BufReader::new(&b"AUTH abcd\nrest"[..]);
    let line = read_line_bounded(&mut r, 1024).unwrap().unwrap();
    assert_eq!(line, "AUTH abcd\n");
}

#[test]
fn bounded_read_rejects_oversize() {
    let big = vec![b'x'; 2048];
    let mut r = BufReader::new(&big[..]);
    assert!(read_line_bounded(&mut r, 1024).unwrap().is_none());
}

#[test]
fn bounded_read_eof_empty() {
    let mut r = BufReader::new(&b""[..]);
    let line = read_line_bounded(&mut r, 1024).unwrap().unwrap();
    assert_eq!(line, "");
}
```

At the bottom of `src/server/connection.rs` add:

```rust
#[cfg(test)]
#[path = "../../tests-rs/test_bounded_read.rs"]
mod test_bounded_read;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test test_bounded_read`
Expected: compile FAIL — `read_line_bounded` not found.

- [ ] **Step 3: Implement helper in `src/server/connection.rs`** (near the other free functions at top)

```rust
/// Read one \n-terminated line with a byte cap. Ok(Some(line)) includes the newline;
/// Ok(Some("")) is EOF; Ok(None) means the cap was exceeded before a newline (protocol violation).
pub(crate) fn read_line_bounded<R: std::io::BufRead>(
    r: &mut R,
    cap: usize,
) -> std::io::Result<Option<String>> {
    let mut buf: Vec<u8> = Vec::with_capacity(128);
    loop {
        let mut byte = [0u8; 1];
        match r.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                buf.push(byte[0]);
                if byte[0] == b'\n' { break; }
                if buf.len() > cap { return Ok(None); }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}
```

- [ ] **Step 4: Use it for the auth line**

In `handle_connection` (~:248), replace the existing `read_line` for `auth_line` with:

```rust
let mut auth_line = match read_line_bounded(&mut reader, 1024) {
    Ok(Some(l)) => l,
    _ => {
        let _ = write_stream.write_all(b"ERROR: Protocol violation\n");
        return;
    }
};
```

(The existing 2000 ms pre-auth `set_read_timeout` at ~:244 stays — that is the pre-auth deadline.)

- [ ] **Step 5: Run tests and full build**

Run: `cargo test test_bounded_read && cargo build --release`
Expected: PASS. Also smoke: `.\target\release\psmux.exe new-session -d -s bpre; .\target\release\psmux.exe list-windows; .\target\release\psmux.exe kill-server` works.

- [ ] **Step 6: Commit**

```powershell
git add src/server/connection.rs tests-rs/test_bounded_read.rs
git commit -m "feat(events): bounded pre-auth line reads on server connections"
```

---

### Task 3: Protected-env fix (case-insensitive, minted-last)

**Files:**
- Modify: `src/pane.rs` (`apply_user_environment` ~:721; call order at ~:206-207)
- Modify: `src/window_ops.rs` (respawn env order ~:1676-1677)
- Test: `tests-rs/test_env_protected.rs` (include from `src/pane.rs`)

**Interfaces:**
- Consumes: nothing.
- Produces: `pane::PROTECTED_ENV_KEYS: &[&str]`; `pane::is_protected_env_key(k: &str) -> bool`. Ordering contract: `apply_user_environment` FIRST, `set_tmux_env` LAST at every spawn site.

- [ ] **Step 1: Write the failing test**

Create `tests-rs/test_env_protected.rs`:

```rust
use super::*;

#[test]
fn protected_keys_match_case_insensitively() {
    assert!(is_protected_env_key("TMUX_PANE"));
    assert!(is_protected_env_key("tmux_pane"));
    assert!(is_protected_env_key("Tmux_Pane"));
    assert!(is_protected_env_key("PSMUX_PANE_INSTANCE"));
    assert!(is_protected_env_key("psmux_session_uid"));
    assert!(!is_protected_env_key("PSMUX_HOOKS_DISABLED")); // user-settable exception
    assert!(!is_protected_env_key("MY_VAR"));
}
```

At the bottom of `src/pane.rs` add:

```rust
#[cfg(test)]
#[path = "../tests-rs/test_env_protected.rs"]
mod test_env_protected;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test test_env_protected`
Expected: compile FAIL.

- [ ] **Step 3: Implement in `src/pane.rs`**

```rust
/// Env keys minted by psmux that user/session environment must never override.
/// Matched case-insensitively (Windows env keys are case-insensitive).
/// PSMUX_HOOKS_DISABLED is deliberately NOT protected (user kill-switch).
pub const PROTECTED_ENV_KEYS: &[&str] = &[
    "TMUX", "TMUX_PANE", "PSMUX_SESSION",
    "PSMUX_SESSION_UID", "PSMUX_BUS_ID", "PSMUX_PANE_INSTANCE",
];

pub fn is_protected_env_key(key: &str) -> bool {
    PROTECTED_ENV_KEYS.iter().any(|p| p.eq_ignore_ascii_case(key))
}
```

In `apply_user_environment` (~:721), skip protected keys — inside its loop over `app.environment` entries add as the first line of the loop body:

```rust
if is_protected_env_key(key) { continue; }
```

(match the actual loop variable name in the file.)

- [ ] **Step 4: Swap ordering at both spawn sites**

`src/pane.rs` ~:206-207 — currently `set_tmux_env(...)` then `apply_user_environment(...)`. Swap to:

```rust
apply_user_environment(&mut shell_cmd, &app.environment);
set_tmux_env(&mut shell_cmd, app.next_pane_id, app.control_port, app.socket_name.as_deref(), &app.session_name, app.claude_code_fix_tty, app.claude_code_force_interactive);
```

`src/window_ops.rs` ~:1676-1677 — same swap (apply_user_environment first, set_tmux_env last), keeping the exact argument lists already present there.

- [ ] **Step 5: Run tests**

Run: `cargo test test_env_protected && cargo test` (full suite to catch regressions, especially env-related tests `test_issue137_env_leak`, `test_new_session_env`)
Expected: all PASS.

- [ ] **Step 6: Commit**

```powershell
git add src/pane.rs src/window_ops.rs tests-rs/test_env_protected.rs
git commit -m "fix(env): protected pane env installed last and matched case-insensitively"
```

---

### Task 4: Identity — session_uid, dormant bus handle, pane_instance, env exports

**Files:**
- Create: `src/events.rs` (identity part only in this task: `gen_uid`, `Cursor` type)
- Modify: `src/main.rs` (add `mod events;` — put it with the other `mod` declarations)
- Modify: `src/types.rs` (`AppState` +3 fields ~:401; `Pane` +1 field ~:84)
- Modify: `src/pane.rs` (assign instance at spawn ~:245 and warm transplant ~:165; export env in `set_tmux_env` ~:682)
- Modify: `src/window_ops.rs` (respawn bumps instance ~:1662)
- Modify: `src/server/mod.rs` (activate identity for non-warm at startup after :751; at ClaimSession ~:3030)
- Test: `tests-rs/test_pane_instance.rs` (include from `src/types.rs`)

**Interfaces:**
- Consumes: `is_warm_session` (`src/session.rs:27`).
- Produces:
  - `events::gen_uid() -> String` (24 hex chars, unique per call within a machine)
  - `events::Cursor { session_uid: String, bus_id: String, seq: u64 }` with `parse(s: &str) -> Option<Cursor>` and `Display` as `<session_uid>:<bus_id>:<seq>`
  - `AppState.session_uid: String` (empty = dormant/warm), `AppState.next_pane_instance: u64` (starts 1), `AppState.event_content: bool` (default false)
  - `Pane.instance: u64`
  - `AppState::alloc_pane_instance(&mut self) -> u64`
  - `set_tmux_env` gains params `pane_instance: u64, session_uid: &str, bus_id: &str` and exports `PSMUX_PANE_INSTANCE`, `PSMUX_SESSION_UID`, `PSMUX_BUS_ID` (only when `session_uid` is non-empty).

- [ ] **Step 1: Write the failing test**

Create `tests-rs/test_pane_instance.rs`:

```rust
use super::*;

#[test]
fn pane_instances_monotonic_and_never_reused() {
    let mut app = AppState::new("t".to_string());
    let a = app.alloc_pane_instance();
    let b = app.alloc_pane_instance();
    assert!(b > a);
}

#[test]
fn cursor_roundtrip() {
    let c = crate::events::Cursor {
        session_uid: "aaa".into(), bus_id: "bbb".into(), seq: 42,
    };
    let s = c.to_string();
    assert_eq!(s, "aaa:bbb:42");
    let p = crate::events::Cursor::parse(&s).unwrap();
    assert_eq!(p.seq, 42);
    assert_eq!(p.session_uid, "aaa");
    assert!(crate::events::Cursor::parse("garbage").is_none());
    assert!(crate::events::Cursor::parse("a:b:notanum").is_none());
}

#[test]
fn gen_uid_unique() {
    assert_ne!(crate::events::gen_uid(), crate::events::gen_uid());
}
```

At the bottom of `src/types.rs` add:

```rust
#[cfg(test)]
#[path = "../tests-rs/test_pane_instance.rs"]
mod test_pane_instance;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test test_pane_instance`
Expected: compile FAIL.

- [ ] **Step 3: Create `src/events.rs` (identity part)**

```rust
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

static UID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 24-hex-char unique id: 16 hex of nanos^pid hash + 8 hex process-local counter.
pub fn gen_uid() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let mixed = nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (pid << 32) ^ pid;
    let n = UID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:016x}{:08x}", mixed, n as u32)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cursor {
    pub session_uid: String,
    pub bus_id: String,
    pub seq: u64,
}

impl Cursor {
    pub fn parse(s: &str) -> Option<Cursor> {
        let mut it = s.trim().split(':');
        let session_uid = it.next()?.to_string();
        let bus_id = it.next()?.to_string();
        let seq = it.next()?.parse::<u64>().ok()?;
        if it.next().is_some() || session_uid.is_empty() || bus_id.is_empty() {
            return None;
        }
        Some(Cursor { session_uid, bus_id, seq })
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.session_uid, self.bus_id, self.seq)
    }
}
```

In `src/main.rs`, next to the other module declarations add:

```rust
mod events;
```

- [ ] **Step 4: AppState + Pane fields (`src/types.rs`)**

In `struct AppState` add (near `session_name` ~:510):

```rust
pub session_uid: String,        // empty while dormant (__warm__)
pub next_pane_instance: u64,
pub event_content: bool,        // server option: allow event content payloads
```

In `AppState::new` (~:937) initialize:

```rust
session_uid: String::new(),
next_pane_instance: 1,
event_content: false,
```

Add method on `AppState`:

```rust
impl AppState {
    pub fn alloc_pane_instance(&mut self) -> u64 {
        let i = self.next_pane_instance;
        self.next_pane_instance += 1;
        i
    }
}
```

(if an `impl AppState` block already exists, add the fn there.)

In `struct Pane` (~:84) add:

```rust
pub instance: u64,
```

Then `cargo build` and fix every `Pane { ... }` literal construction site the compiler reports by adding `instance: 0` initially; the REAL values are set in Step 5 (there are constructions at `src/pane.rs:165` (warm transplant), `src/pane.rs:245` (create_window), `src/popup.rs:141` (popups stay `instance: 0` = excluded), plus any mock constructions in tests).

- [ ] **Step 5: Assign real instances + export env**

`src/pane.rs` create_window path (~:245 pane literal): use `instance: pane_instance` where earlier in the function (before the env calls at ~:206) you add:

```rust
let pane_instance = app.alloc_pane_instance();
```

Warm transplant (~:165): also `let pane_instance = app.alloc_pane_instance();` and set on the transplanted pane: `pane.instance = pane_instance;` — but note warm panes get NO identity env (spawned before claim), which is exactly the spec's "warm panes fail stale validation" behavior.

`src/window_ops.rs` respawn (~:1662, same pane id kept): add

```rust
let new_instance = app.alloc_pane_instance();
```

and set `p.instance = new_instance;` on the respawned pane, and pass it to the env call below.

`src/pane.rs` `set_tmux_env` (~:682): change signature to

```rust
pub fn set_tmux_env(
    cmd: &mut CommandBuilder,
    pane_id: usize,
    control_port: Option<u16>,
    socket_name: Option<&str>,
    session_name: &str,
    pane_instance: u64,
    session_uid: &str,
    bus_id: &str,
    claude_fix_tty: bool,
    claude_force_interactive: bool,
)
```

and inside, after the existing `PSMUX_SESSION` set (~:693):

```rust
if !session_uid.is_empty() {
    cmd.env("PSMUX_SESSION_UID", session_uid);
    cmd.env("PSMUX_BUS_ID", bus_id);
    cmd.env("PSMUX_PANE_INSTANCE", format!("{}", pane_instance));
}
```

Update all `set_tmux_env` call sites (compiler will list them: `pane.rs` create path, `window_ops.rs` respawn) passing `pane_instance`, `&app.session_uid`, `&app.bus_id_string()` — for now, until Task 5 adds the bus, add a temporary method on AppState:

```rust
pub fn bus_id_string(&self) -> String { String::new() }
```

(Task 5 replaces its body with the real bus id.)

- [ ] **Step 6: Mint identity at startup and claim (`src/server/mod.rs`)**

After `let mut app = AppState::new(...)` (~:751):

```rust
if !crate::session::is_warm_session(&session_name) {
    app.session_uid = crate::events::gen_uid();
}
```

In the `CtrlReq::ClaimSession` handler, right after `app.session_name = name` (~:3030):

```rust
app.session_uid = crate::events::gen_uid();
```

- [ ] **Step 7: Run tests + build**

Run: `cargo test test_pane_instance && cargo build --release && cargo test`
Expected: PASS; full suite green (mock constructions fixed with `instance: 0`).

- [ ] **Step 8: Commit**

```powershell
git add src/events.rs src/main.rs src/types.rs src/pane.rs src/window_ops.rs src/popup.rs src/server/mod.rs tests-rs/test_pane_instance.rs
git commit -m "feat(events): session_uid + pane_instance identity, exported to pane env"
```

---

### Task 5: Event bus core (`src/events.rs`)

**Files:**
- Modify: `src/events.rs` (append bus types)
- Modify: `src/types.rs` (AppState field `bus: crate::events::EventBus`; replace `bus_id_string` body)
- Modify: `src/server/mod.rs` (activate bus where session_uid is minted — both sites from Task 4 Step 6)
- Test: `tests-rs/test_agent_events_bus.rs` (include from `src/events.rs`)

**Interfaces:**
- Consumes: `gen_uid`, `Cursor` (Task 4).
- Produces (all in `crate::events`):
  - `Event { session_uid, bus_id, seq, name, category, pane: Option<usize>, pane_instance: Option<u64>, occurred_at_ms: u128, payload: serde_json::Value }` (Clone, Serialize)
  - `SubscriberMsg::{ Event(Box<Event>), Heartbeat, Closed(&'static str) }`
  - `EventBus::new_dormant() -> EventBus`
  - `EventBus::activate(&mut self, session_uid: String)` — mints `bus_id`, enables publishing
  - `EventBus::is_active(&self) -> bool`, `EventBus::bus_id(&self) -> &str`, `EventBus::latest_seq(&self) -> u64`, `EventBus::oldest_seq(&self) -> u64`
  - `EventBus::publish(&mut self, name: &str, category: &str, pane: Option<usize>, pane_instance: Option<u64>, payload: serde_json::Value) -> Option<u64>` (None while dormant; enforces 16 KiB cap; fans out)
  - `EventBus::subscribe(&mut self, names: Vec<String>, categories: Vec<String>, after: Option<Cursor>, tx: std::sync::mpsc::SyncSender<SubscriberMsg>) -> SubscribeAck`
  - `SubscribeAck { ack_json: String, gap: bool, mismatch: bool }` — ack_json contains `{"type":"ack","session_uid":...,"bus_id":...,"oldest_seq":N,"latest_seq":N,"replay_count":N,"gap":bool,"gap_reason":"..."}`; replay events are pre-queued into `tx` before the ack is returned
  - `EventBus::close_all(&mut self, reason: &'static str)`
  - Channel capacity constant `SUB_CHANNEL_CAP: usize = 8192`

- [ ] **Step 1: Write the failing tests**

Create `tests-rs/test_agent_events_bus.rs`:

```rust
use super::*;
use std::sync::mpsc::sync_channel;

fn active_bus() -> EventBus {
    let mut b = EventBus::new_dormant();
    b.activate("sess1".to_string());
    b
}

#[test]
fn dormant_bus_publishes_nothing() {
    let mut b = EventBus::new_dormant();
    assert!(b.publish("agent-done", "agent", None, None, serde_json::json!({})).is_none());
}

#[test]
fn publish_increments_seq_and_retains() {
    let mut b = active_bus();
    let s1 = b.publish("a", "agent", Some(1), Some(7), serde_json::json!({})).unwrap();
    let s2 = b.publish("b", "agent", None, None, serde_json::json!({})).unwrap();
    assert_eq!(s2, s1 + 1);
    assert_eq!(b.latest_seq(), s2);
}

#[test]
fn subscribe_replays_filtered_after_cursor() {
    let mut b = active_bus();
    b.publish("agent-done", "agent", Some(1), None, serde_json::json!({})).unwrap();
    b.publish("pane-bell", "pane", Some(1), None, serde_json::json!({})).unwrap();
    let after = Cursor { session_uid: "sess1".into(), bus_id: b.bus_id().to_string(), seq: 0 };
    let (tx, rx) = sync_channel(SUB_CHANNEL_CAP);
    let ack = b.subscribe(vec!["agent-done".into()], vec![], Some(after), tx);
    assert!(!ack.gap && !ack.mismatch);
    let got = rx.try_recv().unwrap();
    match got { SubscriberMsg::Event(e) => assert_eq!(e.name, "agent-done"), _ => panic!() }
    assert!(rx.try_recv().is_err()); // pane-bell filtered out
}

#[test]
fn cursor_mismatch_and_gap_flagged() {
    let mut b = active_bus();
    b.publish("x", "agent", None, None, serde_json::json!({})).unwrap();
    let (tx, _rx) = sync_channel(SUB_CHANNEL_CAP);
    let bad = Cursor { session_uid: "other".into(), bus_id: "zzz".into(), seq: 0 };
    let ack = b.subscribe(vec![], vec![], Some(bad), tx);
    assert!(ack.mismatch);
}

#[test]
fn ring_caps_at_4096_and_old_cursor_gaps() {
    let mut b = active_bus();
    for _ in 0..5000 {
        b.publish("x", "agent", None, None, serde_json::json!({})).unwrap();
    }
    assert_eq!(b.oldest_seq(), 5000 - 4096 + 1);
    let stale = Cursor { session_uid: "sess1".into(), bus_id: b.bus_id().to_string(), seq: 1 };
    let (tx, _rx) = sync_channel(SUB_CHANNEL_CAP);
    let ack = b.subscribe(vec![], vec![], Some(stale), tx);
    assert!(ack.gap);
}

#[test]
fn live_events_reach_subscriber_after_subscribe() {
    let mut b = active_bus();
    let (tx, rx) = sync_channel(SUB_CHANNEL_CAP);
    let _ = b.subscribe(vec![], vec![], None, tx);
    b.publish("agent-done", "agent", Some(3), Some(9), serde_json::json!({"k":1})).unwrap();
    match rx.try_recv().unwrap() {
        SubscriberMsg::Event(e) => { assert_eq!(e.pane, Some(3)); assert_eq!(e.pane_instance, Some(9)); }
        _ => panic!(),
    }
}

#[test]
fn oversize_payload_truncated() {
    let mut b = active_bus();
    let big = "y".repeat(20 * 1024);
    b.publish("x", "agent", None, None, serde_json::json!({ "blob": big })).unwrap();
    let (tx, rx) = sync_channel(SUB_CHANNEL_CAP);
    let after = Cursor { session_uid: "sess1".into(), bus_id: b.bus_id().to_string(), seq: 0 };
    let _ = b.subscribe(vec![], vec![], Some(after), tx);
    match rx.try_recv().unwrap() {
        SubscriberMsg::Event(e) => {
            assert_eq!(e.payload.get("truncated").and_then(|v| v.as_bool()), Some(true));
            assert!(serde_json::to_string(&*e).unwrap().len() <= 16 * 1024);
        }
        _ => panic!(),
    }
}

#[test]
fn slow_consumer_dropped_others_unaffected() {
    let mut b = active_bus();
    let (tx_slow, _rx_slow_kept_full) = sync_channel(1); // tiny channel, we never drain
    let (tx_ok, rx_ok) = sync_channel(SUB_CHANNEL_CAP);
    let _ = b.subscribe(vec![], vec![], None, tx_slow);
    let _ = b.subscribe(vec![], vec![], None, tx_ok);
    for _ in 0..10 {
        b.publish("x", "agent", None, None, serde_json::json!({})).unwrap();
    }
    let mut ok_count = 0;
    while let Ok(SubscriberMsg::Event(_)) = rx_ok.try_recv() { ok_count += 1; }
    assert_eq!(ok_count, 10);
    assert_eq!(b.subscriber_count(), 1); // slow one removed
}

#[test]
fn close_all_sends_terminal() {
    let mut b = active_bus();
    let (tx, rx) = sync_channel(SUB_CHANNEL_CAP);
    let _ = b.subscribe(vec![], vec![], None, tx);
    b.close_all("bus-closed");
    match rx.try_recv().unwrap() { SubscriberMsg::Closed(r) => assert_eq!(r, "bus-closed"), _ => panic!() }
}
```

At the bottom of `src/events.rs` add:

```rust
#[cfg(test)]
#[path = "../tests-rs/test_agent_events_bus.rs"]
mod test_agent_events_bus;
```

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test test_agent_events_bus`
Expected: compile FAIL.

- [ ] **Step 3: Implement the bus in `src/events.rs`**

```rust
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::mpsc::{SyncSender, TrySendError};

pub const RING_CAP: usize = 4096;
pub const SUB_CHANNEL_CAP: usize = 8192; // > RING_CAP so replay can never block
pub const MAX_EVENT_BYTES: usize = 16 * 1024;
pub const HEARTBEAT_SECS: u64 = 15;

#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub session_uid: String,
    pub bus_id: String,
    pub seq: u64,
    pub name: String,
    pub category: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_instance: Option<u64>,
    pub occurred_at_ms: u128,
    pub payload: serde_json::Value,
}

pub enum SubscriberMsg {
    Event(Box<Event>),
    Heartbeat,
    Closed(&'static str),
}

struct Subscriber {
    tx: SyncSender<SubscriberMsg>,
    names: Vec<String>,
    categories: Vec<String>,
}

impl Subscriber {
    fn accepts(&self, e: &Event) -> bool {
        (self.names.is_empty() || self.names.iter().any(|n| n == &e.name))
            && (self.categories.is_empty() || self.categories.iter().any(|c| c == &e.category))
    }
}

pub struct SubscribeAck {
    pub ack_json: String,
    pub gap: bool,
    pub mismatch: bool,
}

pub struct EventBus {
    session_uid: String,
    bus_id: String,
    seq: u64,
    ring: VecDeque<Event>,
    subs: Vec<Subscriber>,
}

impl EventBus {
    pub fn new_dormant() -> EventBus {
        EventBus { session_uid: String::new(), bus_id: String::new(), seq: 0, ring: VecDeque::new(), subs: Vec::new() }
    }

    pub fn activate(&mut self, session_uid: String) {
        self.session_uid = session_uid;
        self.bus_id = gen_uid();
    }

    pub fn is_active(&self) -> bool { !self.bus_id.is_empty() }
    pub fn bus_id(&self) -> &str { &self.bus_id }
    pub fn session_uid(&self) -> &str { &self.session_uid }
    pub fn latest_seq(&self) -> u64 { self.seq }
    pub fn oldest_seq(&self) -> u64 { self.ring.front().map(|e| e.seq).unwrap_or(self.seq + 1) }
    pub fn subscriber_count(&self) -> usize { self.subs.len() }

    fn now_ms() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }

    pub fn publish(
        &mut self,
        name: &str,
        category: &str,
        pane: Option<usize>,
        pane_instance: Option<u64>,
        payload: serde_json::Value,
    ) -> Option<u64> {
        if !self.is_active() { return None; }
        self.seq += 1;
        let mut ev = Event {
            session_uid: self.session_uid.clone(),
            bus_id: self.bus_id.clone(),
            seq: self.seq,
            name: name.to_string(),
            category: category.to_string(),
            pane,
            pane_instance,
            occurred_at_ms: Self::now_ms(),
            payload,
        };
        if serde_json::to_string(&ev).map(|s| s.len()).unwrap_or(usize::MAX) > MAX_EVENT_BYTES {
            ev.payload = serde_json::json!({ "truncated": true });
        }
        if self.ring.len() == RING_CAP { self.ring.pop_front(); }
        self.ring.push_back(ev.clone());
        // fan out; drop slow consumers
        let mut i = 0;
        while i < self.subs.len() {
            if !self.subs[i].accepts(&ev) { i += 1; continue; }
            match self.subs[i].tx.try_send(SubscriberMsg::Event(Box::new(ev.clone()))) {
                Ok(()) => i += 1,
                Err(TrySendError::Full(_)) => {
                    let s = self.subs.swap_remove(i);
                    let _ = s.tx.try_send(SubscriberMsg::Closed("slow_consumer"));
                }
                Err(TrySendError::Disconnected(_)) => { self.subs.swap_remove(i); }
            }
        }
        Some(ev.seq)
    }

    pub fn subscribe(
        &mut self,
        names: Vec<String>,
        categories: Vec<String>,
        after: Option<Cursor>,
        tx: SyncSender<SubscriberMsg>,
    ) -> SubscribeAck {
        let mut gap = false;
        let mut gap_reason = String::new();
        let mut mismatch = false;
        let from_seq = match &after {
            None => self.seq, // live-only
            Some(c) => {
                if c.session_uid != self.session_uid || c.bus_id != self.bus_id {
                    mismatch = true;
                    self.seq
                } else if c.seq + 1 < self.oldest_seq() && c.seq < self.seq {
                    gap = true;
                    gap_reason = "cursor older than retained ring".to_string();
                    self.oldest_seq().saturating_sub(1)
                } else if c.seq > self.seq {
                    gap = true;
                    gap_reason = "cursor newer than latest".to_string();
                    self.seq
                } else {
                    c.seq
                }
            }
        };
        let sub = Subscriber { tx, names, categories };
        let mut replay_count = 0u64;
        if !mismatch {
            for ev in self.ring.iter().filter(|e| e.seq > from_seq) {
                if sub.accepts(ev) {
                    // capacity SUB_CHANNEL_CAP > RING_CAP: cannot block on a fresh channel
                    let _ = sub.tx.try_send(SubscriberMsg::Event(Box::new(ev.clone())));
                    replay_count += 1;
                }
            }
        }
        let ack_json = serde_json::json!({
            "type": "ack",
            "session_uid": self.session_uid,
            "bus_id": self.bus_id,
            "oldest_seq": self.oldest_seq(),
            "latest_seq": self.seq,
            "replay_count": replay_count,
            "gap": gap,
            "gap_reason": gap_reason,
            "mismatch": mismatch,
        })
        .to_string();
        if !mismatch { self.subs.push(sub); }
        SubscribeAck { ack_json, gap, mismatch }
    }

    /// Periodic heartbeat fan-out; call from the main loop every HEARTBEAT_SECS.
    pub fn heartbeat(&mut self) {
        let mut i = 0;
        while i < self.subs.len() {
            match self.subs[i].tx.try_send(SubscriberMsg::Heartbeat) {
                Ok(()) => i += 1,
                Err(TrySendError::Full(_)) => {
                    let s = self.subs.swap_remove(i);
                    let _ = s.tx.try_send(SubscriberMsg::Closed("slow_consumer"));
                }
                Err(TrySendError::Disconnected(_)) => { self.subs.swap_remove(i); }
            }
        }
    }

    pub fn close_all(&mut self, reason: &'static str) {
        for s in self.subs.drain(..) {
            let _ = s.tx.try_send(SubscriberMsg::Closed(reason));
        }
    }
}
```

- [ ] **Step 4: Attach to AppState**

`src/types.rs` — add field to `AppState`:

```rust
pub bus: crate::events::EventBus,
```

initialize in `AppState::new`:

```rust
bus: crate::events::EventBus::new_dormant(),
```

Replace `bus_id_string` body:

```rust
pub fn bus_id_string(&self) -> String { self.bus.bus_id().to_string() }
```

`src/server/mod.rs` — at both identity-mint sites from Task 4 Step 6, activate the bus alongside:

```rust
app.session_uid = crate::events::gen_uid();
app.bus.activate(app.session_uid.clone());
```

- [ ] **Step 5: Heartbeat tick in the main loop**

In the timer section of the loop (near the 5 s registry refresh at ~:1130), add a 15 s tick:

```rust
// events bus heartbeat
if last_bus_heartbeat.elapsed() >= std::time::Duration::from_secs(crate::events::HEARTBEAT_SECS) {
    app.bus.heartbeat();
    last_bus_heartbeat = std::time::Instant::now();
}
```

declaring `let mut last_bus_heartbeat = std::time::Instant::now();` before the loop (next to the other loop-scoped timers).

- [ ] **Step 6: Run tests**

Run: `cargo test test_agent_events_bus && cargo test`
Expected: all PASS.

- [ ] **Step 7: Commit**

```powershell
git add src/events.rs src/types.rs src/server/mod.rs tests-rs/test_agent_events_bus.rs
git commit -m "feat(events): in-memory event bus with replay ring, filters, slow-consumer handling"
```

---

### Task 6: Producers — wire bus into commit chokepoints

**Files:**
- Modify: `src/tree.rs` (reap transition records ~:416, :867)
- Modify: `src/server/mod.rs` (hook_event consumer ~:5209; reap site ~:5677-5732; bell site ~:5341; ClaimSession)
- Test: `tests-rs/test_agent_events_wiring.rs` (include from `src/server/mod.rs`)

**Interfaces:**
- Consumes: `EventBus::publish` (Task 5), `AppState.bus`.
- Produces: events `window-created` (category `window`, payload `{window_id}`), `pane-exited` (category `pane`, payload `{reason}` where reason is `"exited"` or `"killed"`, with `pane` + `pane_instance` set), `pane-bell` (category `pane`), `session-renamed` (category `session`, payload `{name_len}`); `tree::reap_children` now returns `(bool, bool, bool, Vec<PaneTransition>)` where `pub struct PaneTransition { pub pane_id: usize, pub instance: u64, pub window_id: usize }` (defined in `src/tree.rs`).

- [ ] **Step 1: Write the failing test**

Create `tests-rs/test_agent_events_wiring.rs`:

```rust
use super::*;
use std::sync::mpsc::sync_channel;

#[test]
fn hook_event_new_window_publishes_window_created() {
    let mut app = crate::types::AppState::new("t".to_string());
    app.session_uid = "s".to_string();
    app.bus.activate("s".to_string());
    let (tx, rx) = sync_channel(crate::events::SUB_CHANNEL_CAP);
    let _ = app.bus.subscribe(vec![], vec![], None, tx);
    publish_hook_event(&mut app, "after-new-window");
    match rx.try_recv().unwrap() {
        crate::events::SubscriberMsg::Event(e) => {
            assert_eq!(e.name, "window-created");
            assert_eq!(e.category, "window");
        }
        _ => panic!(),
    }
}

#[test]
fn dormant_warm_bus_stays_silent() {
    let mut app = crate::types::AppState::new("__warm__".to_string());
    let (tx, rx) = sync_channel(crate::events::SUB_CHANNEL_CAP);
    let _ = app.bus.subscribe(vec![], vec![], None, tx);
    publish_hook_event(&mut app, "after-new-window");
    assert!(rx.try_recv().is_err());
}

#[test]
fn pane_transitions_publish_pane_exited_with_instance() {
    let mut app = crate::types::AppState::new("t".to_string());
    app.bus.activate("s".to_string());
    let (tx, rx) = sync_channel(crate::events::SUB_CHANNEL_CAP);
    let _ = app.bus.subscribe(vec!["pane-exited".into()], vec![], None, tx);
    let transitions = vec![crate::tree::PaneTransition { pane_id: 5, instance: 12, window_id: 2 }];
    publish_pane_transitions(&mut app, &transitions);
    match rx.try_recv().unwrap() {
        crate::events::SubscriberMsg::Event(e) => {
            assert_eq!(e.pane, Some(5));
            assert_eq!(e.pane_instance, Some(12));
        }
        _ => panic!(),
    }
}
```

At the bottom of `src/server/mod.rs` add:

```rust
#[cfg(test)]
#[path = "../../tests-rs/test_agent_events_wiring.rs"]
mod test_agent_events_wiring;
```

- [ ] **Step 2: Run test to verify failure**

Run: `cargo test test_agent_events_wiring`
Expected: compile FAIL (`publish_hook_event`, `PaneTransition`, `publish_pane_transitions` missing).

- [ ] **Step 3: Implement transition records in `src/tree.rs`**

Add near the top:

```rust
#[derive(Clone, Debug)]
pub struct PaneTransition {
    pub pane_id: usize,
    pub instance: u64,
    pub window_id: usize,
}
```

In `prune_exited` (~:416): before a pane is dropped or marked dead, record `(p.id, p.instance)` into an out-param `transitions: &mut Vec<(usize, u64)>` threaded through the recursion (add the parameter; update its internal recursive call sites).

In `reap_children` (~:867): collect per-window transitions BEFORE windows/panes are removed, change the return type:

```rust
pub fn reap_children(app: &mut AppState) -> (bool, bool, bool, Vec<PaneTransition>)
```

building `Vec<PaneTransition>` from the collected `(pane_id, instance)` pairs plus `win.id`, and return it as the fourth element. Update the existing caller at `src/server/mod.rs:5677` to destructure the extra element.

- [ ] **Step 4: Implement publishers in `src/server/mod.rs`** (free functions near the bottom, above the test include)

```rust
/// Map a committed hook_event tag to a bus event. Called from the hook_event
/// consumer at the single post-command chokepoint.
pub(crate) fn publish_hook_event(app: &mut AppState, event: &str) {
    let (name, category, payload) = match event {
        "after-new-window" => ("window-created", "window",
            serde_json::json!({ "window_id": app.windows.get(app.active_idx).map(|w| w.id) })),
        "after-rename-session" => ("session-renamed", "session",
            serde_json::json!({ "name_len": app.session_name.len() })),
        _ => return,
    };
    let _ = app.bus.publish(name, category, None, None, payload);
}

pub(crate) fn publish_pane_transitions(app: &mut AppState, transitions: &[crate::tree::PaneTransition]) {
    for t in transitions {
        let _ = app.bus.publish(
            "pane-exited", "pane", Some(t.pane_id), Some(t.instance),
            serde_json::json!({ "window_id": t.window_id, "reason": "exited" }),
        );
    }
}
```

- [ ] **Step 5: Call them at the chokepoints**

1. Hook consumer (~:5209): inside `if let Some(event) = hook_event { ... }`, after `fire_hooks`/notification emission, add:

```rust
publish_hook_event(&mut app, event);
```

2. Reap site (~:5677): after destructuring `(all_empty, any_pruned, any_newly_dead, transitions)`, next to the `pane-died`/`pane-exited` hook fire (~:5725-5732), add:

```rust
publish_pane_transitions(&mut app, &transitions);
```

3. Bell site (~:5341-5344, where bell hooks fire during render): for each pane with a pending bell (the code there knows the window/pane), add:

```rust
let _ = app.bus.publish("pane-bell", "pane", Some(pane_id), None, serde_json::json!({}));
```

(match the local variable holding the belling pane's id at that site.)

- [ ] **Step 6: Run tests**

Run: `cargo test test_agent_events_wiring && cargo test`
Expected: PASS.

- [ ] **Step 7: Commit**

```powershell
git add src/tree.rs src/server/mod.rs tests-rs/test_agent_events_wiring.rs
git commit -m "feat(events): publish lifecycle events from post-commit chokepoints"
```

---

### Task 7: CtrlReq variants + loop handlers (subscribe, cursor, notify, pane version)

**Files:**
- Modify: `src/types.rs` (`CtrlReq` enum ~:1179)
- Modify: `src/server/mod.rs` (loop `match req` — add four arms; place near `CtrlReq::WaitFor` handler ~:4512)
- Test: `tests-rs/test_agent_events_handlers.rs` (include from `src/server/mod.rs`, appended to the Task 6 include — use a second `#[path]` module)

**Interfaces:**
- Consumes: `EventBus` (Task 5), identity (Task 4).
- Produces `CtrlReq` variants:

```rust
EventsSubscribe {
    names: Vec<String>,
    categories: Vec<String>,
    after: Option<crate::events::Cursor>,
    tx: std::sync::mpsc::SyncSender<crate::events::SubscriberMsg>,
    ack: std::sync::mpsc::Sender<String>,   // ack_json
},
EventsCursor(std::sync::mpsc::Sender<String>),          // "<uid>:<bus>:<seq>" or "ERR: dormant"
NotifyEvent {
    pane_id: Option<usize>,       // parsed from caller's TMUX_PANE (%N)
    pane_instance: Option<u64>,   // caller's PSMUX_PANE_INSTANCE
    session_uid: String,          // caller's PSMUX_SESSION_UID
    name: String,                 // "agent-done" | "agent-notify"
    title_len: usize,
    content: Option<String>,      // already capped at 4096 by CLI
    resp: std::sync::mpsc::Sender<String>,  // "OK <seq>" | "STALE" | "ERR: ..."
},
PaneDataVersion(std::sync::mpsc::Sender<String>),       // "<u64>" of active pane
```

- Server-side validation rule (the determinism core): a `NotifyEvent` is **stale** unless (a) `session_uid` matches `app.session_uid`, AND (b) `pane_id` resolves to a live pane whose `instance == pane_instance`. Stale publishes `stale-notify` (category `agent`) and answers `STALE`.

- [ ] **Step 1: Write the failing test**

Create `tests-rs/test_agent_events_handlers.rs`:

```rust
use super::*;
use std::sync::mpsc::sync_channel;

#[test]
fn notify_validation_stale_on_wrong_instance() {
    let mut app = crate::types::AppState::new("t".to_string());
    app.session_uid = "s".into();
    app.bus.activate("s".into());
    // live pane %5 with instance 2 (validation helper looks pane up by id)
    let verdict = validate_notify(&app, Some(5), Some(999), "s", |_pid| Some(2));
    assert!(!verdict);
    let verdict = validate_notify(&app, Some(5), Some(2), "s", |_pid| Some(2));
    assert!(verdict);
    let verdict = validate_notify(&app, Some(5), Some(2), "WRONG", |_pid| Some(2));
    assert!(!verdict);
    let verdict = validate_notify(&app, Some(5), Some(2), "s", |_pid| None); // pane gone
    assert!(!verdict);
    let verdict = validate_notify(&app, None, Some(2), "s", |_pid| Some(2)); // no pane id
    assert!(!verdict);
}

#[test]
fn notify_publishes_agent_done_or_stale() {
    let mut app = crate::types::AppState::new("t".to_string());
    app.session_uid = "s".into();
    app.bus.activate("s".into());
    let (tx, rx) = sync_channel(crate::events::SUB_CHANNEL_CAP);
    let _ = app.bus.subscribe(vec![], vec![], None, tx);
    handle_notify_validated(&mut app, true, Some(5), Some(2), "agent-done", 4, None);
    match rx.try_recv().unwrap() {
        crate::events::SubscriberMsg::Event(e) => assert_eq!(e.name, "agent-done"),
        _ => panic!(),
    }
    handle_notify_validated(&mut app, false, Some(5), Some(999), "agent-done", 4, None);
    match rx.try_recv().unwrap() {
        crate::events::SubscriberMsg::Event(e) => assert_eq!(e.name, "stale-notify"),
        _ => panic!(),
    }
}

#[test]
fn content_gated_by_server_option() {
    let mut app = crate::types::AppState::new("t".to_string());
    app.bus.activate("s".into());
    let (tx, rx) = sync_channel(crate::events::SUB_CHANNEL_CAP);
    let _ = app.bus.subscribe(vec![], vec![], None, tx);
    app.event_content = false;
    handle_notify_validated(&mut app, true, Some(1), Some(1), "agent-notify", 5, Some("secret".into()));
    match rx.try_recv().unwrap() {
        crate::events::SubscriberMsg::Event(e) => assert!(e.payload.get("content").is_none()),
        _ => panic!(),
    }
    app.event_content = true;
    handle_notify_validated(&mut app, true, Some(1), Some(1), "agent-notify", 5, Some("ok".into()));
    match rx.try_recv().unwrap() {
        crate::events::SubscriberMsg::Event(e) => assert_eq!(e.payload.get("content").and_then(|v| v.as_str()), Some("ok")),
        _ => panic!(),
    }
}
```

Add below the Task 6 include in `src/server/mod.rs`:

```rust
#[cfg(test)]
#[path = "../../tests-rs/test_agent_events_handlers.rs"]
mod test_agent_events_handlers;
```

- [ ] **Step 2: Run test to verify failure**

Run: `cargo test test_agent_events_handlers`
Expected: compile FAIL.

- [ ] **Step 3: Add the `CtrlReq` variants** to `src/types.rs` (~:1179) exactly as in the Interfaces block above.

- [ ] **Step 4: Implement helpers + loop arms in `src/server/mod.rs`**

Helpers (near the Task 6 publishers):

```rust
/// Look up a live pane's instance by pane id across all windows (None = not found).
pub(crate) fn live_pane_instance(app: &AppState, pane_id: usize) -> Option<u64> {
    for w in &app.windows {
        let mut found = None;
        crate::tree::visit_leaves(&w.root, &mut |p: &crate::types::Pane| {
            if p.id == pane_id && !p.dead { found = Some(p.instance); }
        });
        if found.is_some() { return found; }
    }
    None
}

/// Pure validation used by NotifyEvent (lookup injected for testability).
pub(crate) fn validate_notify(
    app: &AppState,
    pane_id: Option<usize>,
    pane_instance: Option<u64>,
    caller_session_uid: &str,
    lookup: impl Fn(usize) -> Option<u64>,
) -> bool {
    if caller_session_uid.is_empty() || caller_session_uid != app.session_uid { return false; }
    let (Some(pid), Some(pinst)) = (pane_id, pane_instance) else { return false; };
    lookup(pid) == Some(pinst)
}

pub(crate) fn handle_notify_validated(
    app: &mut AppState,
    valid: bool,
    pane_id: Option<usize>,
    pane_instance: Option<u64>,
    name: &str,
    title_len: usize,
    content: Option<String>,
) -> Option<u64> {
    let mut payload = serde_json::json!({ "title_len": title_len });
    if let (true, Some(c)) = (app.event_content, content) {
        payload["content"] = serde_json::Value::String(c);
    }
    if valid {
        app.bus.publish(name, "agent", pane_id, pane_instance, payload)
    } else {
        app.bus.publish("stale-notify", "agent", pane_id, pane_instance, payload)
    }
}
```

If `tree::visit_leaves` does not exist, add it to `src/tree.rs`:

```rust
pub fn visit_leaves(node: &Node, f: &mut impl FnMut(&crate::types::Pane)) {
    match node {
        Node::Leaf(p) => f(p),
        Node::Split { children, .. } => for c in children { visit_leaves(&c.node, f) },
    }
}
```

(match the actual `Node::Split` field names in `types.rs` — the compiler will tell you.)

Loop arms (place near `CtrlReq::WaitFor` ~:4512):

```rust
CtrlReq::EventsSubscribe { names, categories, after, tx: sub_tx, ack } => {
    let a = app.bus.subscribe(names, categories, after, sub_tx);
    let _ = ack.send(a.ack_json);
}
CtrlReq::EventsCursor(resp) => {
    let out = if app.bus.is_active() {
        format!("{}:{}:{}", app.session_uid, app.bus.bus_id(), app.bus.latest_seq())
    } else {
        "ERR: bus dormant".to_string()
    };
    let _ = resp.send(out);
}
CtrlReq::NotifyEvent { pane_id, pane_instance, session_uid, name, title_len, content, resp } => {
    let valid = validate_notify(&app, pane_id, pane_instance, &session_uid,
                                |pid| live_pane_instance(&app, pid));
    let seq = handle_notify_validated(&mut app, valid, pane_id, pane_instance, &name, title_len, content);
    let _ = resp.send(match (valid, seq) {
        (true, Some(s)) => format!("OK {}", s),
        (false, _) => "STALE".to_string(),
        _ => "ERR: bus dormant".to_string(),
    });
}
CtrlReq::PaneDataVersion(resp) => {
    let v = app.windows.get(app.active_idx)
        .and_then(|w| crate::tree::active_pane_ref(&w.root, &w.active_path))
        .map(|p| p.data_version.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(0);
    let _ = resp.send(format!("{}", v));
}
```

(if `active_pane_ref` doesn't exist, use the existing immutable active-pane accessor the file already uses for cursor position at ~:1214 — reuse that exact pattern. `data_version`'s concrete type is whatever `Pane.data_version` is — if it's a plain `u64` bumped by the reader thread via a shared Arc, read it the same way `server/mod.rs:1139-1201` does.)

- [ ] **Step 5: Run tests**

Run: `cargo test test_agent_events_handlers && cargo test`
Expected: PASS.

- [ ] **Step 6: Commit**

```powershell
git add src/types.rs src/server/mod.rs src/tree.rs tests-rs/test_agent_events_handlers.rs
git commit -m "feat(events): CtrlReq handlers for subscribe/cursor/notify with stale validation"
```

---

### Task 8: connection.rs dispatch arms (stream, wait, settle)

**Files:**
- Modify: `src/server/connection.rs` (new arms in the `match cmd` ~:913..:3051)
- Test: e2e coverage in Task 12 (these arms are IO glue; unit coverage comes from Tasks 5/7 internals)

**Interfaces:**
- Consumes: `CtrlReq::{EventsSubscribe, EventsCursor, NotifyEvent, PaneDataVersion}`, `SubscriberMsg`, `read_line_bounded`.
- Produces wire commands (client → server):
  - `events-subscribe [name=<n>]* [category=<c>]* [after=<cursor>]` — long-lived: writes ack line, then one JSON line per event, `{"type":"heartbeat"}` lines, terminal `{"type":"closed","reason":"..."}` line, then closes.
  - `events-cursor` — one line reply.
  - `notify-event <json>` — single JSON arg: `{"pane_id":5,"pane_instance":2,"session_uid":"...","name":"agent-done","title_len":3,"content":null}`; one line reply `OK <seq>` / `STALE` / `ERR: ...`.
  - `wait-event <json>` — `{"pane_id":5,"pane_instance":null,"name":"agent-done","after":"u:b:7","timeout_ms":600000}`; replies with exactly one line: the matching event JSON, or `TIMEOUT`, `GAP`, `MISMATCH`, `ENDED`.
  - `capture-pane` gains `-W <quiet_ms>` and `-Y <settle_timeout_ms>` wire flags (settle pre-wait before the normal capture path).

- [ ] **Step 1: `events-subscribe` arm** (model on the control-mode writer at ~:420-441)

```rust
"events-subscribe" => {
    let mut names = Vec::new();
    let mut categories = Vec::new();
    let mut after = None;
    for a in &args {
        if let Some(v) = a.strip_prefix("name=") { names.push(v.to_string()); }
        else if let Some(v) = a.strip_prefix("category=") { categories.push(v.to_string()); }
        else if let Some(v) = a.strip_prefix("after=") { after = crate::events::Cursor::parse(v); }
    }
    let (sub_tx, sub_rx) = std::sync::mpsc::sync_channel(crate::events::SUB_CHANNEL_CAP);
    let (ack_tx, ack_rx) = std::sync::mpsc::channel::<String>();
    let _ = tx.send(CtrlReq::EventsSubscribe { names, categories, after, tx: sub_tx, ack: ack_tx });
    if let Ok(ack) = ack_rx.recv() {
        if write!(write_stream, "{}\n", ack).is_err() { break; }
        let _ = write_stream.flush();
    } else { break; }
    // stream until closed/disconnect; recv timeout bounds the write-liveness check
    loop {
        match sub_rx.recv_timeout(std::time::Duration::from_secs(crate::events::HEARTBEAT_SECS + 5)) {
            Ok(crate::events::SubscriberMsg::Event(e)) => {
                let line = serde_json::to_string(&*e).unwrap_or_default();
                if write!(write_stream, "{}\n", line).is_err() || write_stream.flush().is_err() { break; }
            }
            Ok(crate::events::SubscriberMsg::Heartbeat) => {
                if write!(write_stream, "{{\"type\":\"heartbeat\"}}\n").is_err() || write_stream.flush().is_err() { break; }
            }
            Ok(crate::events::SubscriberMsg::Closed(r)) => {
                let _ = write!(write_stream, "{{\"type\":\"closed\",\"reason\":\"{}\"}}\n", r);
                let _ = write_stream.flush();
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue, // bus heartbeat missed; keep waiting
            Err(_) => break,
        }
    }
    break; // connection is consumed by the stream
}
```

(When the client disconnects, writes fail → this thread exits → the bus's next `try_send` returns `Disconnected` and the subscriber is pruned.)

- [ ] **Step 2: `events-cursor` and `notify-event` arms**

```rust
"events-cursor" => {
    let (rtx, rrx) = std::sync::mpsc::channel::<String>();
    let _ = tx.send(CtrlReq::EventsCursor(rtx));
    if let Ok(t) = rrx.recv() {
        let _ = write!(write_stream, "{}\n", t);
        let _ = write_stream.flush();
    }
    if !persistent { break; }
}
"notify-event" => {
    let reply = (|| -> String {
        let raw = args.join(" ");
        let v: serde_json::Value = match serde_json::from_str(&raw) { Ok(v) => v, Err(_) => return "ERR: bad json".into() };
        let (rtx, rrx) = std::sync::mpsc::channel::<String>();
        let _ = tx.send(CtrlReq::NotifyEvent {
            pane_id: v.get("pane_id").and_then(|x| x.as_u64()).map(|x| x as usize),
            pane_instance: v.get("pane_instance").and_then(|x| x.as_u64()),
            session_uid: v.get("session_uid").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            name: v.get("name").and_then(|x| x.as_str()).unwrap_or("agent-notify").to_string(),
            title_len: v.get("title_len").and_then(|x| x.as_u64()).unwrap_or(0) as usize,
            content: v.get("content").and_then(|x| x.as_str()).map(|s| s.chars().take(4096).collect()),
            resp: rtx,
        });
        rrx.recv().unwrap_or_else(|_| "ERR: server".into())
    })();
    let _ = write!(write_stream, "{}\n", reply);
    let _ = write_stream.flush();
    if !persistent { break; }
}
```

- [ ] **Step 3: `wait-event` arm** (subscribe + deadline in the connection thread)

```rust
"wait-event" => {
    let raw = args.join(" ");
    let reply = (|| -> String {
        let v: serde_json::Value = match serde_json::from_str(&raw) { Ok(v) => v, Err(_) => return "ERR: bad json".into() };
        let want_pane = v.get("pane_id").and_then(|x| x.as_u64()).map(|x| x as usize);
        let want_inst = v.get("pane_instance").and_then(|x| x.as_u64());
        let want_name = v.get("name").and_then(|x| x.as_str()).map(|s| s.to_string());
        let after = v.get("after").and_then(|x| x.as_str()).and_then(crate::events::Cursor::parse);
        let timeout_ms = v.get("timeout_ms").and_then(|x| x.as_u64()).unwrap_or(60_000);
        if v.get("after").is_some() && after.is_none() { return "ERR: bad cursor".into(); }
        let (sub_tx, sub_rx) = std::sync::mpsc::sync_channel(crate::events::SUB_CHANNEL_CAP);
        let (ack_tx, ack_rx) = std::sync::mpsc::channel::<String>();
        let names = want_name.clone().map(|n| vec![n]).unwrap_or_default();
        let _ = tx.send(CtrlReq::EventsSubscribe { names, categories: vec![], after, tx: sub_tx, ack: ack_tx });
        let ack: serde_json::Value = match ack_rx.recv().ok().and_then(|s| serde_json::from_str(&s).ok()) {
            Some(a) => a, None => return "ERR: server".into(),
        };
        if ack.get("mismatch").and_then(|x| x.as_bool()) == Some(true) { return "MISMATCH".into(); }
        if ack.get("gap").and_then(|x| x.as_bool()) == Some(true) { return "GAP".into(); }
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() { return "TIMEOUT".into(); }
            match sub_rx.recv_timeout(left) {
                Ok(crate::events::SubscriberMsg::Event(e)) => {
                    if want_pane.is_some() && e.pane != want_pane { continue; }
                    if want_inst.is_some() && e.pane_instance != want_inst { continue; }
                    return serde_json::to_string(&*e).unwrap_or_else(|_| "ERR: encode".into());
                }
                Ok(crate::events::SubscriberMsg::Heartbeat) => continue,
                Ok(crate::events::SubscriberMsg::Closed(_)) => return "ENDED".into(),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return "TIMEOUT".into(),
                Err(_) => return "ENDED".into(),
            }
        }
    })();
    let _ = write!(write_stream, "{}\n", reply);
    let _ = write_stream.flush();
    break; // one-shot; the bus prunes the subscriber on next publish (channel disconnected)
}
```

- [ ] **Step 4: capture-pane settle pre-wait**

In the `"capture-pane" | "capturep"` arm (~:983), parse two new flags with the existing flag-parse style: `-W <quiet_ms>` (settle), `-Y <settle_timeout_ms>` (default 10000). If `-W` present, BEFORE building the capture CtrlReq run:

```rust
if let Some(quiet_ms) = settle_quiet_ms {
    let settle_deadline = std::time::Instant::now() + std::time::Duration::from_millis(settle_timeout_ms);
    let mut last_v = String::new();
    let mut stable_since = std::time::Instant::now();
    loop {
        let (rtx, rrx) = std::sync::mpsc::channel::<String>();
        let _ = tx.send(CtrlReq::PaneDataVersion(rtx));
        let v = rrx.recv().unwrap_or_default();
        if v != last_v { last_v = v; stable_since = std::time::Instant::now(); }
        if stable_since.elapsed() >= std::time::Duration::from_millis(quiet_ms) { break; }
        if std::time::Instant::now() >= settle_deadline {
            settle_timed_out = true; // written as "SETTLE-TIMEOUT" marker line before output below
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}
```

If `settle_timed_out`, write the line `SETTLE-TIMEOUT` before the captured text (the CLI maps it to a nonzero exit while still printing the capture).

- [ ] **Step 5: Build + smoke test**

Run: `cargo build --release`, then:

```powershell
.\target\release\psmux.exe new-session -d -s evt
.\target\release\psmux.exe list-windows   # sanity: server up
```

Manual wire smoke (PowerShell, mirrors src/session.rs framing):

```powershell
$port = Get-Content "$env:USERPROFILE\.psmux\evt.port"
$key  = Get-Content "$env:USERPROFILE\.psmux\evt.key"
$c = New-Object Net.Sockets.TcpClient("127.0.0.1", [int]$port)
$w = New-Object IO.StreamWriter($c.GetStream()); $r = New-Object IO.StreamReader($c.GetStream())
$w.WriteLine("AUTH $key"); $w.WriteLine("events-cursor"); $w.Flush()
$r.ReadLine(); $r.ReadLine()   # OK, then <uid>:<bus>:<seq>
.\target\release\psmux.exe kill-server
```

Expected: second line matches `^[0-9a-f]+:[0-9a-f]+:\d+$`.

- [ ] **Step 6: Commit**

```powershell
git add src/server/connection.rs
git commit -m "feat(events): wire arms for events-subscribe/cursor/notify/wait-event and capture settle"
```

---

### Task 9: CLI verbs in main.rs

**Files:**
- Modify: `src/main.rs` (new arms in `match cmd` before `_ =>` at ~:3751; capture-pane arm ~:1514 for `--settle`)
- Modify: `src/cli.rs` (help text `print_help` ~:67, `print_commands` ~:433)
- Modify: `src/server/helpers.rs` (`TMUX_COMMANDS` ~:485 — add `cursor`, `events`, `wait-event`, `notify`)
- Test: `tests-rs/test_cli_events_args.rs` (include from `src/main.rs`) for the pure arg-builder helpers

**Interfaces:**
- Consumes: wire commands from Task 8; `send_control_with_response` (`src/session.rs:1140`).
- Produces user verbs:
  - `psmux cursor` → prints `<uid>:<bus>:<seq>`, exit 0 (exit 1 on `ERR:`).
  - `psmux events [--name X]... [--category Y]... [--after CUR] [--no-heartbeat]` → streams JSON lines to stdout until terminal frame; exit 0 on `closed`, 1 on transport error.
  - `psmux wait-event --pane %N [--instance G] [--name NAME] [--after CUR] [--timeout MS]` → prints event JSON; exit codes 0/2/3/4/5 per Global Constraints.
  - `psmux notify [--done] [--name NAME] [--title T] [--include-content --content C]` → always exit 0; 2000 ms bound.
  - `psmux hook-notify <agent> <event>` → reads stdin (cap 1 MiB), always prints `{}`, always exit 0.
  - `psmux capture-pane ... --settle <ms> [--settle-timeout <ms>]` → maps to `-W`/`-Y` wire flags; exit 1 if `SETTLE-TIMEOUT` marker seen.
  - Pure helpers (testable): `build_notify_json(env: &NotifyEnv, name: &str, title_len: usize, content: Option<&str>) -> String`, `wait_event_exit_code(reply: &str) -> i32`, `parse_hook_event_name(agent: &str, event: &str) -> &'static str`.

- [ ] **Step 1: Write the failing test**

Create `tests-rs/test_cli_events_args.rs`:

```rust
use super::*;

#[test]
fn wait_event_exit_codes() {
    assert_eq!(wait_event_exit_code("{\"seq\":1}"), 0);
    assert_eq!(wait_event_exit_code("TIMEOUT"), 2);
    assert_eq!(wait_event_exit_code("GAP"), 3);
    assert_eq!(wait_event_exit_code("MISMATCH"), 4);
    assert_eq!(wait_event_exit_code("ENDED"), 5);
    assert_eq!(wait_event_exit_code("ERR: x"), 1);
}

#[test]
fn hook_event_mapping() {
    assert_eq!(parse_hook_event_name("claude", "stop"), "agent-done");
    assert_eq!(parse_hook_event_name("claude", "notification"), "agent-notify");
    assert_eq!(parse_hook_event_name("codex", "agent-turn-complete"), "agent-done");
    assert_eq!(parse_hook_event_name("anything", "unknown"), "agent-notify");
}

#[test]
fn notify_json_shape() {
    let env = NotifyEnv {
        pane_id: Some(5), pane_instance: Some(2), session_uid: "u".into(),
    };
    let j: serde_json::Value = serde_json::from_str(
        &build_notify_json(&env, "agent-done", 3, None)).unwrap();
    assert_eq!(j["pane_id"], 5);
    assert_eq!(j["pane_instance"], 2);
    assert_eq!(j["session_uid"], "u");
    assert_eq!(j["name"], "agent-done");
    assert!(j["content"].is_null());
}
```

At the bottom of `src/main.rs` add:

```rust
#[cfg(test)]
#[path = "../tests-rs/test_cli_events_args.rs"]
mod test_cli_events_args;
```

- [ ] **Step 2: Run test to verify failure**

Run: `cargo test test_cli_events_args`
Expected: compile FAIL.

- [ ] **Step 3: Implement pure helpers in `src/main.rs`** (free functions near other helpers)

```rust
pub(crate) struct NotifyEnv {
    pub pane_id: Option<usize>,
    pub pane_instance: Option<u64>,
    pub session_uid: String,
}

impl NotifyEnv {
    pub(crate) fn from_process_env() -> NotifyEnv {
        NotifyEnv {
            pane_id: std::env::var("TMUX_PANE").ok()
                .and_then(|s| s.trim_start_matches('%').parse::<usize>().ok()),
            pane_instance: std::env::var("PSMUX_PANE_INSTANCE").ok()
                .and_then(|s| s.parse::<u64>().ok()),
            session_uid: std::env::var("PSMUX_SESSION_UID").unwrap_or_default(),
        }
    }
}

pub(crate) fn build_notify_json(env: &NotifyEnv, name: &str, title_len: usize, content: Option<&str>) -> String {
    serde_json::json!({
        "pane_id": env.pane_id,
        "pane_instance": env.pane_instance,
        "session_uid": env.session_uid,
        "name": name,
        "title_len": title_len,
        "content": content,
    }).to_string()
}

pub(crate) fn wait_event_exit_code(reply: &str) -> i32 {
    let r = reply.trim();
    if r.starts_with('{') { 0 }
    else if r == "TIMEOUT" { 2 }
    else if r == "GAP" { 3 }
    else if r == "MISMATCH" { 4 }
    else if r == "ENDED" { 5 }
    else { 1 }
}

pub(crate) fn parse_hook_event_name(_agent: &str, event: &str) -> &'static str {
    match event {
        "stop" | "agent-turn-complete" => "agent-done",
        _ => "agent-notify",
    }
}
```

- [ ] **Step 4: Add the CLI arms** (each follows the `display-message` pattern at ~:2469; `cmd_args` is the per-verb arg slice)

```rust
"cursor" => {
    let resp = session::send_control_with_response("events-cursor\n".to_string())?;
    let out = resp.trim();
    println!("{}", out);
    return if out.starts_with("ERR") { Err("bus dormant".into()) } else { Ok(()) };
}
"wait-event" => {
    let mut pane: Option<u64> = None; let mut inst: Option<u64> = None;
    let mut name: Option<String> = None; let mut after: Option<String> = None;
    let mut timeout: u64 = 60_000;
    let mut i = 1;
    while i < cmd_args.len() {
        match cmd_args[i].as_str() {
            "--pane" => { i += 1; pane = cmd_args.get(i).and_then(|s| s.trim_start_matches('%').parse().ok()); }
            "--instance" => { i += 1; inst = cmd_args.get(i).and_then(|s| s.parse().ok()); }
            "--name" => { i += 1; name = cmd_args.get(i).cloned(); }
            "--after" => { i += 1; after = cmd_args.get(i).cloned(); }
            "--timeout" => { i += 1; timeout = cmd_args.get(i).and_then(|s| s.parse().ok()).unwrap_or(60_000); }
            "--json" => {}
            _ => {}
        }
        i += 1;
    }
    let req = serde_json::json!({
        "pane_id": pane, "pane_instance": inst, "name": name,
        "after": after, "timeout_ms": timeout,
    });
    let resp = session::send_control_with_response_timeout(
        format!("wait-event {}\n", req), timeout + 10_000)?;
    let reply = resp.trim();
    println!("{}", reply);
    std::process::exit(wait_event_exit_code(reply));
}
"events" => {
    let mut wire = String::from("events-subscribe");
    let mut show_heartbeat = true;
    let mut i = 1;
    while i < cmd_args.len() {
        match cmd_args[i].as_str() {
            "--name" => { i += 1; if let Some(v) = cmd_args.get(i) { wire.push_str(&format!(" name={}", v)); } }
            "--category" => { i += 1; if let Some(v) = cmd_args.get(i) { wire.push_str(&format!(" category={}", v)); } }
            "--after" => { i += 1; if let Some(v) = cmd_args.get(i) { wire.push_str(&format!(" after={}", v)); } }
            "--no-heartbeat" => { show_heartbeat = false; }
            "--json" => {}
            _ => {}
        }
        i += 1;
    }
    wire.push('\n');
    return session::stream_control_lines(wire, |line| {
        if !show_heartbeat && line.contains("\"type\":\"heartbeat\"") { return true; }
        println!("{}", line);
        !line.contains("\"type\":\"closed\"")
    });
}
"notify" => {
    let mut name = "agent-notify".to_string();
    let mut title = String::new(); let mut content: Option<String> = None;
    let mut include_content = false;
    let mut i = 1;
    while i < cmd_args.len() {
        match cmd_args[i].as_str() {
            "--done" => name = "agent-done".to_string(),
            "--name" => { i += 1; if let Some(v) = cmd_args.get(i) { name = v.clone(); } }
            "--title" => { i += 1; if let Some(v) = cmd_args.get(i) { title = v.clone(); } }
            "--include-content" => include_content = true,
            "--content" => { i += 1; content = cmd_args.get(i).cloned(); }
            _ => {}
        }
        i += 1;
    }
    let env = NotifyEnv::from_process_env();
    if env.pane_instance.is_none() || env.session_uid.is_empty() {
        return Ok(()); // outside psmux (or warm pane): silent no-op success
    }
    let c = if include_content { content.as_deref().map(|s| &s[..s.len().min(4096)]) } else { None };
    let json = build_notify_json(&env, &name, title.len(), c);
    // bounded-sync: 2000 ms total; failures are swallowed (exit 0 always)
    let _ = session::send_control_with_response_timeout(format!("notify-event {}\n", json), 2_000);
    return Ok(());
}
"hook-notify" => {
    let agent = cmd_args.get(1).cloned().unwrap_or_default();
    let event = cmd_args.get(2).cloned().unwrap_or_default();
    // Always satisfy the agent hook contract, no matter what happens below.
    let mut stdin_buf = String::new();
    {
        use std::io::Read;
        let _ = std::io::stdin().take(1024 * 1024).read_to_string(&mut stdin_buf);
    }
    let env = NotifyEnv::from_process_env();
    let disabled = std::env::var("PSMUX_HOOKS_DISABLED").ok().as_deref() == Some("1");
    if !disabled && env.pane_instance.is_some() && !env.session_uid.is_empty() {
        let name = parse_hook_event_name(&agent, &event);
        let json = build_notify_json(&env, name, 0, None);
        let _ = session::send_control_with_response_timeout(format!("notify-event {}\n", json), 2_000);
    }
    println!("{{}}");
    return Ok(());
}
```

`capture-pane` arm (~:1514): parse `--settle <ms>` and `--settle-timeout <ms>`, append ` -W <ms>` / ` -Y <ms>` to the wire line; after receiving the response, if it starts with `SETTLE-TIMEOUT\n`, print the rest and `std::process::exit(1)`.

- [ ] **Step 5: Add the two session.rs helpers**

In `src/session.rs`, clone `send_control_with_response` (~:1140) into:

```rust
pub fn send_control_with_response_timeout(cmd: String, total_ms: u64) -> Result<String, Box<dyn std::error::Error>> {
    // identical to send_control_with_response, but set_read_timeout(total_ms)
    // on the stream and connect_timeout(min(1000, total_ms)).
}
```

(copy the body, threading the timeout into `connect_timeout` and `set_read_timeout`.)

And a line-streaming variant for `events`:

```rust
pub fn stream_control_lines(
    cmd: String,
    mut on_line: impl FnMut(&str) -> bool, // return false to stop
) -> Result<(), Box<dyn std::error::Error>> {
    // same connect + AUTH + TARGET + write(cmd) preamble as send_control_with_response,
    // but NO shutdown(Write) and NO read-to-EOF: read line by line with a
    // BufReader and a long read timeout (HEARTBEAT_SECS+10), skip the initial "OK",
    // call on_line(line) for each; return Ok(()) when on_line returns false or EOF.
}
```

(follow the exact preamble of `send_control_with_response` at `src/session.rs:1140-1170`.)

- [ ] **Step 6: Register verbs in help**

- `src/server/helpers.rs` `TMUX_COMMANDS` (~:485): add `"cursor"`, `"events"`, `"wait-event"`, `"notify"`, `"hook-notify"`.
- `src/cli.rs` `print_commands` (~:433) and `print_help` (~:67): add one-line entries:

```text
cursor                     Print the event-bus cursor (session:bus:seq)
events                     Stream events as JSON lines (--name/--category/--after)
wait-event                 Block until a matching event (--pane/--name/--after/--timeout)
notify                     Publish an agent event from inside a pane (--done)
hook-notify                Agent hook entrypoint (reads stdin, prints {})
```

- [ ] **Step 7: Run tests + end-to-end smoke**

Run: `cargo test test_cli_events_args && cargo build --release`

Smoke (PowerShell):

```powershell
$p = ".\target\release\psmux.exe"
& $p new-session -d -s smoke
$cur = & $p -t smoke cursor
$cur -match '^[0-9a-f]+:[0-9a-f]+:\d+$'          # True
& $p -t smoke split-window -d                     # publishes lifecycle events
& $p -t smoke wait-event --name pane-exited --after $cur --timeout 1000; $LASTEXITCODE  # 2 (timeout, no exit yet)
& $p -t smoke send-keys -t %1 "exit" Enter
& $p -t smoke wait-event --name pane-exited --after $cur --timeout 15000; $LASTEXITCODE # 0 + event JSON
& $p kill-server
```

- [ ] **Step 8: Commit**

```powershell
git add src/main.rs src/session.rs src/cli.rs src/server/helpers.rs tests-rs/test_cli_events_args.rs
git commit -m "feat(events): CLI verbs cursor/events/wait-event/notify/hook-notify + capture --settle"
```

---

### Task 10: Unified shutdown path

**Files:**
- Modify: `src/server/mod.rs` (new `shutdown_server`; route exit sites :1829, :2944, :4153, :4240, :4291, :4510, :5757)
- Test: covered by e2e in Task 12 (`test_agent_events_shutdown.ps1`); pure part tested here

**Interfaces:**
- Consumes: `EventBus::{publish, close_all}`.
- Produces: `pub(crate) fn shutdown_server(app: &mut AppState, reason: &str) -> !` — the ONLY exit path for orderly server termination.

- [ ] **Step 1: Implement `shutdown_server`** in `src/server/mod.rs`:

```rust
/// Single orderly-exit path: publish terminal event, close subscribers,
/// then perform the legacy cleanup the call site used to do inline.
pub(crate) fn shutdown_server(app: &mut AppState, reason: &str) -> ! {
    let _ = app.bus.publish("bus-closed", "bus", None, None, serde_json::json!({ "reason": reason }));
    app.bus.close_all("bus-closed");
    // give stream writer threads a beat to flush the terminal frame
    std::thread::sleep(std::time::Duration::from_millis(80));
    let home = crate::types::home_dir();
    let base = app.port_file_base();
    let _ = std::fs::remove_file(home.join(format!("{}.port", base)));
    let _ = std::fs::remove_file(home.join(format!("{}.key", base)));
    let _ = std::fs::remove_file(home.join(format!("{}.sid", base)));
    let _ = std::fs::remove_file(home.join(format!("{}.pid", base)));
    std::process::exit(0);
}
```

(match the real helper for the registry dir — the same paths the sites already remove; if `home_dir` lives elsewhere, use the same expression the KillServer handler at ~:4498 uses.)

- [ ] **Step 2: Route the exit sites**

At each of `src/server/mod.rs:1829, :2944, :4153, :4240, :4291, :4510, :5757`: keep the site's OWN pre-cleanup (control `%exit` drain, `send_directive_to_all_clients("DETACH")`, `kill_all_children_batch`, warm-pane kill — those differ per site and stay), but replace the final `std::process::exit(0)` + registry-file removal with:

```rust
shutdown_server(&mut app, "kill-server");   // reason per site:
// :1829/:4153/:4240/:4291 -> "detach-exit"
// :2944                    -> "session-teardown"
// :4510                    -> "kill-server"
// :5757                    -> "exit-empty"
```

(Leave the debug-only `:837` fault-injection exit untouched.)

- [ ] **Step 3: Build + verify no double-removal warnings**

Run: `cargo build --release && cargo test`
Expected: clean; full suite green.

- [ ] **Step 4: Manual verify terminal frame**

```powershell
$p = ".\target\release\psmux.exe"
& $p new-session -d -s shut
Start-Job { & ".\target\release\psmux.exe" -t shut events } | Out-Null
Start-Sleep 1
& $p kill-server
Start-Sleep 1
Receive-Job * | Select-String '"bus-closed"'   # terminal frame observed
Remove-Job *
```

- [ ] **Step 5: Commit**

```powershell
git add src/server/mod.rs
git commit -m "feat(events): unified shutdown path publishes bus-closed and closes subscribers"
```

---

### Task 11: Claude hook installer (`psmux hooks ...`)

**Files:**
- Create: `src/hooks_install.rs`
- Modify: `src/main.rs` (`mod hooks_install;` + `"hooks"` CLI arm)
- Test: `tests-rs/test_hooks_install.rs` (include from `src/hooks_install.rs`)

**Interfaces:**
- Consumes: nothing from the server (pure client-side file surgery).
- Produces:
  - `hooks_install::install_claude(settings_path: &Path, psmux_exe: &Path) -> Result<InstallReport, String>`
  - `hooks_install::uninstall_claude(settings_path: &Path) -> Result<InstallReport, String>`
  - `hooks_install::status_claude(settings_path: &Path) -> String`
  - `InstallReport { changed: bool, backup: Option<PathBuf> }`
  - Marker rule: an entry is psmux-owned iff its `command` contains the substring `" hook-notify claude "` (with the exe path prefix). Foreign hooks are NEVER touched.
  - CLI: `psmux hooks install claude [--user|--project-local]`, `psmux hooks uninstall claude [...]`, `psmux hooks status claude [...]`. Default `--user` = `%USERPROFILE%\.claude\settings.json`; `--project-local` = `.\.claude\settings.local.json`.
  - Manifest: `%USERPROFILE%\.psmux\hooks-manifest.json` — `{"claude": {"path": "...", "installed_at_ms": N, "psmux_version": "3.3.6"}}`.

- [ ] **Step 1: Write the failing tests**

Create `tests-rs/test_hooks_install.rs`:

```rust
use super::*;
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("psmux-hooktest-{}-{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.join("settings.json")
}

#[test]
fn install_into_missing_file_creates_stop_hook() {
    let p = tmp("fresh");
    let r = install_claude(&p, std::path::Path::new("C:\\bin\\psmux.exe")).unwrap();
    assert!(r.changed);
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    let cmd = v["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
    assert!(cmd.contains("hook-notify claude stop"));
}

#[test]
fn install_preserves_foreign_hooks_and_is_idempotent() {
    let p = tmp("foreign");
    std::fs::write(&p, r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"my-own-thing"}]}]},"model":"opus"}"#).unwrap();
    install_claude(&p, std::path::Path::new("C:\\bin\\psmux.exe")).unwrap();
    let r2 = install_claude(&p, std::path::Path::new("C:\\bin\\psmux.exe")).unwrap();
    assert!(!r2.changed); // idempotent
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    let s = v["hooks"]["Stop"].as_array().unwrap();
    let all = serde_json::to_string(s).unwrap();
    assert!(all.contains("my-own-thing"));         // foreign preserved
    assert_eq!(all.matches("hook-notify claude stop").count(), 1); // exactly one psmux entry
    assert_eq!(v["model"], "opus");                // unrelated keys untouched
}

#[test]
fn uninstall_removes_only_ours() {
    let p = tmp("uninst");
    std::fs::write(&p, r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"my-own-thing"}]}]}}"#).unwrap();
    install_claude(&p, std::path::Path::new("C:\\bin\\psmux.exe")).unwrap();
    let r = uninstall_claude(&p).unwrap();
    assert!(r.changed);
    let txt = std::fs::read_to_string(&p).unwrap();
    assert!(txt.contains("my-own-thing"));
    assert!(!txt.contains("hook-notify"));
}

#[test]
fn install_creates_backup_when_file_existed() {
    let p = tmp("bak");
    std::fs::write(&p, "{}").unwrap();
    let r = install_claude(&p, std::path::Path::new("C:\\bin\\psmux.exe")).unwrap();
    assert!(r.backup.is_some());
    assert!(r.backup.unwrap().exists());
}
```

At the bottom of `src/hooks_install.rs` (created next step) add:

```rust
#[cfg(test)]
#[path = "../tests-rs/test_hooks_install.rs"]
mod test_hooks_install;
```

- [ ] **Step 2: Run test to verify failure**

Run: `cargo test test_hooks_install`
Expected: compile FAIL.

- [ ] **Step 3: Implement `src/hooks_install.rs`**

```rust
use std::path::{Path, PathBuf};

pub struct InstallReport {
    pub changed: bool,
    pub backup: Option<PathBuf>,
}

const OWNED_MARKER: &str = " hook-notify claude ";

fn hook_entry(psmux_exe: &Path, event: &str) -> serde_json::Value {
    serde_json::json!({
        "hooks": [{
            "type": "command",
            "command": format!("\"{}\" hook-notify claude {}", psmux_exe.display(), event),
            "timeout": 10
        }]
    })
}

fn is_owned(group: &serde_json::Value) -> bool {
    group["hooks"].as_array().map(|hs| {
        hs.iter().any(|h| h["command"].as_str().map(|c| c.contains(OWNED_MARKER)).unwrap_or(false))
    }).unwrap_or(false)
}

fn load(path: &Path) -> Result<serde_json::Value, String> {
    match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(serde_json::json!({})),
        Ok(s) => serde_json::from_str(&s).map_err(|e| format!("{}: invalid JSON: {}", path.display(), e)),
        Err(_) => Ok(serde_json::json!({})),
    }
}

/// Lock via exclusive sibling lockfile; retry ~2s then fail.
fn acquire_lock(path: &Path) -> Result<PathBuf, String> {
    let lock = path.with_extension("json.psmux-lock");
    for _ in 0..40 {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
            Ok(_) => return Ok(lock),
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
    Err(format!("could not lock {}", lock.display()))
}

fn atomic_write(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
    let tmp = path.with_extension("json.psmux-tmp");
    std::fs::write(&tmp, contents).map_err(|e| e.to_string())?;
    // same-directory replace
    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

fn backup(path: &Path) -> Option<PathBuf> {
    if !path.exists() { return None; }
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_millis();
    let bak = path.with_extension(format!("json.bak-{}", ms));
    std::fs::copy(path, &bak).ok()?;
    Some(bak)
}

fn strip_owned(v: &mut serde_json::Value) -> bool {
    let mut changed = false;
    if let Some(events) = v.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for (_ev, arr) in events.iter_mut() {
            if let Some(groups) = arr.as_array_mut() {
                let before = groups.len();
                groups.retain(|g| !is_owned(g));
                changed |= groups.len() != before;
            }
        }
    }
    changed
}

pub fn install_claude(settings_path: &Path, psmux_exe: &Path) -> Result<InstallReport, String> {
    let lock = acquire_lock(settings_path)?;
    let result = (|| {
        let mut v = load(settings_path)?; // reread under lock
        let already = ["Stop", "SessionStart", "SessionEnd"].iter().all(|ev| {
            v["hooks"][ev].as_array().map(|a| a.iter().any(is_owned)).unwrap_or(false)
        });
        if already { return Ok(InstallReport { changed: false, backup: None }); }
        let bak = backup(settings_path);
        strip_owned(&mut v); // remove stale versions before re-adding
        if !v["hooks"].is_object() { v["hooks"] = serde_json::json!({}); }
        for (ev, hook_ev) in [("Stop", "stop"), ("SessionStart", "session-start"), ("SessionEnd", "session-end")] {
            if !v["hooks"][ev].is_array() { v["hooks"][ev] = serde_json::json!([]); }
            v["hooks"][ev].as_array_mut().unwrap().push(hook_entry(psmux_exe, hook_ev));
        }
        atomic_write(settings_path, &serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?)?;
        write_manifest(settings_path)?;
        Ok(InstallReport { changed: true, backup: bak })
    })();
    let _ = std::fs::remove_file(lock);
    result
}

pub fn uninstall_claude(settings_path: &Path) -> Result<InstallReport, String> {
    let lock = acquire_lock(settings_path)?;
    let result = (|| {
        let mut v = load(settings_path)?;
        let bak = backup(settings_path);
        if !strip_owned(&mut v) { return Ok(InstallReport { changed: false, backup: None }); }
        atomic_write(settings_path, &serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?)?;
        Ok(InstallReport { changed: true, backup: bak })
    })();
    let _ = std::fs::remove_file(lock);
    result
}

pub fn status_claude(settings_path: &Path) -> String {
    match load(settings_path) {
        Ok(v) => {
            let installed = v["hooks"]["Stop"].as_array().map(|a| a.iter().any(is_owned)).unwrap_or(false);
            format!("claude: {} ({})", if installed { "installed" } else { "not installed" }, settings_path.display())
        }
        Err(e) => format!("claude: unreadable ({})", e),
    }
}

fn manifest_path() -> PathBuf {
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_default();
    Path::new(&home).join(".psmux").join("hooks-manifest.json")
}

fn write_manifest(settings_path: &Path) -> Result<(), String> {
    let mp = manifest_path();
    if let Some(d) = mp.parent() { let _ = std::fs::create_dir_all(d); }
    let mut m: serde_json::Value = std::fs::read_to_string(&mp).ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64).unwrap_or(0);
    m["claude"] = serde_json::json!({
        "path": settings_path.display().to_string(),
        "installed_at_ms": ms,
        "psmux_version": env!("CARGO_PKG_VERSION"),
    });
    std::fs::write(&mp, serde_json::to_string_pretty(&m).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
#[path = "../tests-rs/test_hooks_install.rs"]
mod test_hooks_install;
```

- [ ] **Step 4: CLI arm in `src/main.rs`** (+ `mod hooks_install;` declaration)

```rust
"hooks" => {
    let sub = cmd_args.get(1).map(|s| s.as_str()).unwrap_or("");
    let agent = cmd_args.get(2).map(|s| s.as_str()).unwrap_or("");
    if agent != "claude" {
        eprintln!("psmux hooks: only 'claude' is supported in this version");
        std::process::exit(1);
    }
    let project_local = cmd_args.iter().any(|a| a == "--project-local");
    let settings = if project_local {
        std::path::PathBuf::from(".claude").join("settings.local.json")
    } else {
        let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_default();
        std::path::Path::new(&home).join(".claude").join("settings.json")
    };
    match sub {
        "install" => {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let r = hooks_install::install_claude(&settings, &exe).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            println!("claude hooks: {}{}",
                if r.changed { "installed" } else { "already installed" },
                r.backup.map(|b| format!(" (backup: {})", b.display())).unwrap_or_default());
        }
        "uninstall" => {
            let r = hooks_install::uninstall_claude(&settings).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            println!("claude hooks: {}", if r.changed { "removed" } else { "nothing to remove" });
        }
        "status" | "doctor" => println!("{}", hooks_install::status_claude(&settings)),
        _ => { eprintln!("usage: psmux hooks <install|uninstall|status> claude [--project-local]"); std::process::exit(1); }
    }
    return Ok(());
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test test_hooks_install && cargo test`
Expected: PASS.

- [ ] **Step 6: Commit**

```powershell
git add src/hooks_install.rs src/main.rs tests-rs/test_hooks_install.rs
git commit -m "feat(events): Claude Code hook installer with lock/backup/manifest/uninstall"
```

---

### Task 12: End-to-end adversarial tests + docs

**Files:**
- Create: `tests/test_agent_events_e2e.ps1`
- Create: `tests/test_agent_events_stale.ps1`
- Create: `tests/test_agent_events_shutdown.ps1`
- Create: `docs/agent-events.md`
- Modify: `README.md` (one line in the docs index linking `docs/agent-events.md`)

**Interfaces:**
- Consumes: everything shipped in Tasks 1-11, real `psmux.exe` (tests resolve `..\target\release\psmux.exe` — NOT the installed PATH copy).
- Produces: e2e regression gate runnable via `pwsh tests/_batch_runner.ps1 -Tests test_agent_events_e2e,test_agent_events_stale,test_agent_events_shutdown` (note: `_batch_runner.ps1` resolves `psmux` from PATH; these three scripts define `$P` themselves so they also run standalone: `pwsh tests\test_agent_events_e2e.ps1`).

- [ ] **Step 1: `tests/test_agent_events_e2e.ps1`** (happy path + race + settle)

```powershell
$ErrorActionPreference = "Stop"
$P = Join-Path $PSScriptRoot "..\target\release\psmux.exe"
$S = "evte2e"
& $P kill-server 2>$null; Start-Sleep -m 500

# --- cursor + dispatch + wait-event (fast-completion race: task finishes before wait starts)
& $P new-session -d -s $S -x 100 -y 30
Start-Sleep -m 800
$cur = (& $P -t $S cursor).Trim()
if ($cur -notmatch '^[0-9a-f]+:[0-9a-f]+:\d+$') { throw "bad cursor: $cur" }

& $P -t $S split-window -d          # %2 spawns
& $P -t $S send-keys -t %2 "exit" Enter
Start-Sleep -Seconds 2               # let it FULLY exit BEFORE we start waiting (replay must catch it)
$ev = & $P -t $S wait-event --pane %2 --name pane-exited --after $cur --timeout 10000
if ($LASTEXITCODE -ne 0) { throw "wait-event exit $LASTEXITCODE (race not caught by replay)" }
if ($ev -notmatch '"name":"pane-exited"') { throw "wrong event: $ev" }

# --- notify from inside a pane reaches an outside waiter
$cur2 = (& $P -t $S cursor).Trim()
& $P -t $S send-keys -t %1 "& '$P' notify --done" Enter
$ev2 = & $P -t $S wait-event --name agent-done --after $cur2 --timeout 10000
if ($LASTEXITCODE -ne 0) { throw "agent-done not received: exit $LASTEXITCODE" }

# --- wait-event timeout path
$cur3 = (& $P -t $S cursor).Trim()
& $P -t $S wait-event --name agent-done --after $cur3 --timeout 1500 | Out-Null
if ($LASTEXITCODE -ne 2) { throw "expected timeout exit 2, got $LASTEXITCODE" }

# --- cursor mismatch path
& $P -t $S wait-event --name agent-done --after "deadbeef:deadbeef:1" --timeout 1500 | Out-Null
if ($LASTEXITCODE -ne 4) { throw "expected mismatch exit 4, got $LASTEXITCODE" }

# --- capture-pane --settle returns completed output
& $P -t $S send-keys -t %1 "echo SETTLED-MARKER" Enter
$cap = & $P -t $S capture-pane -p --settle 300 --settle-timeout 5000
if ($cap -notmatch "SETTLED-MARKER") { throw "settle capture missed output" }

& $P kill-server 2>$null
Write-Host "PASS test_agent_events_e2e"
```

- [ ] **Step 2: `tests/test_agent_events_stale.ps1`** (respawn stale rejection + outside no-op + env protection)

```powershell
$ErrorActionPreference = "Stop"
$P = Join-Path $PSScriptRoot "..\target\release\psmux.exe"
$S = "evstale"
& $P kill-server 2>$null; Start-Sleep -m 500
& $P new-session -d -s $S
Start-Sleep -m 800

# Capture %1's identity env, then respawn %1 so that identity goes stale.
& $P -t $S send-keys -t %1 "`$env:PSMUX_PANE_INSTANCE + ':' + `$env:PSMUX_SESSION_UID > `$env:TEMP\psmux-stale-id.txt" Enter
Start-Sleep -Seconds 2
$old = (Get-Content "$env:TEMP\psmux-stale-id.txt").Trim().Split(':')
& $P -t $S respawn-pane -k -t %1
Start-Sleep -Seconds 1

# Replay the OLD identity from outside (simulates a delayed hook from the dead process).
$cur = (& $P -t $S cursor).Trim()
$json = '{"pane_id":1,"pane_instance":' + $old[0] + ',"session_uid":"' + $old[1] + '","name":"agent-done","title_len":0,"content":null}'
# drive the wire directly (stale caller has no live pane env):
$port = Get-Content "$env:USERPROFILE\.psmux\$S.port"; $key = Get-Content "$env:USERPROFILE\.psmux\$S.key"
$c = New-Object Net.Sockets.TcpClient("127.0.0.1", [int]$port)
$w = New-Object IO.StreamWriter($c.GetStream()); $r = New-Object IO.StreamReader($c.GetStream())
$w.WriteLine("AUTH $key"); $w.WriteLine("notify-event $json"); $w.Flush()
$null = $r.ReadLine()               # OK
$verdict = $r.ReadLine()
$c.Close()
if ($verdict -ne "STALE") { throw "expected STALE, got: $verdict" }

# The stale attempt must NOT satisfy an agent-done waiter, but MUST appear as stale-notify.
& $P -t $S wait-event --name agent-done --after $cur --timeout 1500 | Out-Null
if ($LASTEXITCODE -ne 2) { throw "stale notify satisfied an agent-done wait!" }
& $P -t $S wait-event --name stale-notify --after $cur --timeout 5000 | Out-Null
if ($LASTEXITCODE -ne 0) { throw "stale-notify event missing" }

# notify OUTSIDE any pane: silent no-op success
$env:PSMUX_PANE_INSTANCE = $null; $env:PSMUX_SESSION_UID = $null
& $P notify --done
if ($LASTEXITCODE -ne 0) { throw "outside notify must exit 0" }

# case-folded protected env cannot override TMUX_PANE
& $P -t $S set-environment tmux_pane FAKE
& $P -t $S split-window -d
Start-Sleep -m 800
& $P -t $S send-keys -t %3 "`$env:TMUX_PANE > `$env:TEMP\psmux-envprot.txt" Enter
Start-Sleep -Seconds 2
if ((Get-Content "$env:TEMP\psmux-envprot.txt").Trim() -eq "FAKE") { throw "protected env overridden!" }

& $P kill-server 2>$null
Write-Host "PASS test_agent_events_stale"
```

- [ ] **Step 3: `tests/test_agent_events_shutdown.ps1`** (terminal frame + subscriber teardown)

```powershell
$ErrorActionPreference = "Stop"
$P = Join-Path $PSScriptRoot "..\target\release\psmux.exe"
$S = "evshut"
& $P kill-server 2>$null; Start-Sleep -m 500
& $P new-session -d -s $S
Start-Sleep -m 800
$out = "$env:TEMP\psmux-evshut.jsonl"
Remove-Item $out -ErrorAction SilentlyContinue
$job = Start-Job -ScriptBlock {
    param($exe, $sess, $file)
    & $exe -t $sess events *> $file
} -ArgumentList $P, $S, $out
Start-Sleep -Seconds 2
& $P -t $S kill-server
$null = Wait-Job $job -Timeout 15
if ((Get-Content $out -Raw) -notmatch '"bus-closed"') { throw "no bus-closed terminal frame" }
Remove-Job $job -Force
Write-Host "PASS test_agent_events_shutdown"
```

- [ ] **Step 4: Run all three**

Run:

```powershell
cargo build --release
pwsh tests\test_agent_events_e2e.ps1
pwsh tests\test_agent_events_stale.ps1
pwsh tests\test_agent_events_shutdown.ps1
```

Expected: three `PASS` lines. Fix regressions before proceeding — these ARE the spec §10 gate for increment 1 (warm-server, two-server, and RDP-session scenarios are exercised implicitly by the identity checks here; full matrix items tied to increment-2 features land with increment 2).

- [ ] **Step 5: Write `docs/agent-events.md`**

Content: the orchestration contract from spec §1 (cursor → dispatch → wait-event recipe, exit-code table, event name/category table, `notify`/`hook-notify` env gating, `hooks install claude`, `capture-pane --settle`, threat model summary, warm-pane cold-spawn requirement, popup exclusion). Link it from `README.md`'s docs list (one line: `<a href="docs/agent-events.md">Agent Events</a> ·`). Write it from the spec — do not copy this plan's internals; document only user-facing behavior.

- [ ] **Step 6: Commit**

```powershell
git add tests/test_agent_events_e2e.ps1 tests/test_agent_events_stale.ps1 tests/test_agent_events_shutdown.ps1 docs/agent-events.md README.md
git commit -m "test(events): adversarial e2e gate + agent-events docs"
```

---

## Self-Review Notes

- Spec coverage: §2 threat model → Tasks 1, 2 (elevated refusal, pre-auth) + docs Task 12; §3 identity → Task 4; §4 bus → Tasks 5, 6; §5 verbs/contract → Tasks 7, 8, 9; §6 hooks/installer → Tasks 9 (hook-notify), 11; §8 env fix → Task 3; §9 redaction → Task 7 (metadata-only + `event_content` gate); §10 test matrix → per-task Rust tests + Task 12 e2e; unified shutdown → Task 10. OSC/env-file/disk-log/codex/gemini are increments 2-3 (not in this plan by design).
- `set-environment tmux_pane FAKE` in the stale test assumes `set-environment` lowercases nothing — the assertion is on the CHILD env, which Task 3's filter protects regardless.
- Line-number drift: all `~:NNN` references are anchors, not gospel — locate by the named function/pattern.

use std::path::{Path, PathBuf};

pub struct InstallReport {
    pub changed: bool,
    pub backup: Option<PathBuf>,
}

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

fn is_owned(group: &serde_json::Value, marker: &str) -> bool {
    group["hooks"].as_array().map(|hs| {
        hs.iter().any(|h| h["command"].as_str().map(|c| c.contains(marker)).unwrap_or(false))
    }).unwrap_or(false)
}

fn load(path: &Path) -> Result<serde_json::Value, String> {
    match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(serde_json::json!({})),
        Ok(s) => serde_json::from_str(&s).map_err(|e| format!("{}: invalid JSON: {}", path.display(), e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(format!("{}: cannot read (refusing to overwrite): {}", path.display(), e)),
    }
}

/// Lock via exclusive sibling lockfile; retry ~2s then fail.
/// The lock is taken before any write, so the settings parent dir (e.g. a fresh
/// project's `.claude\`) may not exist yet — create it here, and only retry on
/// AlreadyExists (real contention); any other error is terminal.
fn acquire_lock(path: &Path) -> Result<PathBuf, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {}", dir.display(), e))?;
    }
    let lock = path.with_extension("json.psmux-lock");
    let mut last_err = String::new();
    for _ in 0..40 {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
            Ok(_) => return Ok(lock),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_err = e.to_string();
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(format!("could not lock {}: {}", lock.display(), e)),
        }
    }
    Err(format!("could not lock {}: {}", lock.display(), last_err))
}

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

fn backup(path: &Path) -> Result<Option<PathBuf>, String> {
    if !path.exists() { return Ok(None); }
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis()).unwrap_or(0);
    let bak = path.with_extension(format!("json.bak-{}", ms));
    std::fs::copy(path, &bak).map_err(|e| format!("{}: backup failed: {}", path.display(), e))?;
    Ok(Some(bak))
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

fn install_json_hooks(path: &Path, exe: &Path, agent: &str, events: &[(&str, &str)]) -> Result<InstallReport, String> {
    let marker = owned_marker(agent);
    let lock = acquire_lock(path)?;
    let result = (|| {
        let mut v = load(path)?; // reread under lock
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
        &[("Stop", "stop"), ("SessionStart", "session-start"), ("SessionEnd", "session-end")])
}

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

pub fn status_agent(path: &Path, agent: &str, events: &[(&str, &str)], exe: &Path) -> String {
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
/// [hooks.state.'<path>:stop:0:0'].trusted_hash. We cannot reproduce codex's
/// hash without knowing its exact normalization (raw bytes vs. canonicalized
/// JSON, line endings, etc.), and this repo has no existing sha256 helper to
/// build on (verified via `grep -rn "sha256\|Sha256" src/` — no hits) and no
/// sha2-family crate dependency. Per the brief's fallback, this is downgraded
/// to presence-only detection: whether a trust entry for this hooks.json's
/// stop hook exists at all ("trust-pending") vs none ("trust-unknown"). It
/// NEVER claims "trusted" since that would require the byte-exact hash match
/// we can't safely compute. Advisory only.
fn codex_trust_state(hooks_path: &Path) -> &'static str {
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_default();
    let cfg = Path::new(&home).join(".codex").join("config.toml");
    let Ok(cfg_s) = std::fs::read_to_string(&cfg) else {
        return "trust-unknown";
    };
    let _ = hooks_path; // reserved: path-scoped lookup once codex's key format is confirmed
    if cfg_s.contains("hooks.json:stop") {
        "trust-pending"
    } else {
        "trust-unknown"
    }
}

/// The manifest is shared across all agents/installs, so clearing one agent's
/// entry races the same read-modify-write hazard as `write_manifest` — reuse
/// the identical locked pattern to avoid a lost update.
fn clear_manifest(agent: &str) -> Result<(), String> {
    let mp = manifest_path();
    if let Some(d) = mp.parent() { let _ = std::fs::create_dir_all(d); }
    let lock = acquire_lock(&mp)?;
    let result = (|| {
        let mut m: serde_json::Value = std::fs::read_to_string(&mp).ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        if let Some(o) = m.as_object_mut() { o.remove(agent); }
        std::fs::write(&mp, serde_json::to_string_pretty(&m).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())
    })();
    let _ = std::fs::remove_file(lock);
    result
}

pub fn uninstall_claude(settings_path: &Path) -> Result<InstallReport, String> {
    uninstall_agent(settings_path, "claude")
}

pub fn status_claude(settings_path: &Path) -> String {
    let exe = std::env::current_exe().unwrap_or_default();
    status_agent(settings_path, "claude",
        &[("Stop", "stop"), ("SessionStart", "session-start"), ("SessionEnd", "session-end")], &exe)
}

/// Resolve the hooks manifest location.
///
/// Test/scripting override: when `PSMUX_HOOKS_MANIFEST_DIR` is set (non-empty),
/// the manifest lives at `<that dir>\hooks-manifest.json` instead of the default
/// `%USERPROFILE%\.psmux\hooks-manifest.json`. The unit tests set it so that
/// `cargo test` never writes to the real per-user manifest.
fn manifest_path() -> PathBuf {
    if let Ok(dir) = std::env::var("PSMUX_HOOKS_MANIFEST_DIR") {
        if !dir.trim().is_empty() {
            return Path::new(&dir).join("hooks-manifest.json");
        }
    }
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_default();
    Path::new(&home).join(".psmux").join("hooks-manifest.json")
}

// The manifest is shared across all agents/installs (unlike each agent's own
// settings file), so concurrent installs racing a read-modify-write on it can
// lose an update. Serialize with the same sibling-lockfile scheme used for
// settings files, keyed off the manifest path itself.
fn write_manifest(settings_path: &Path, agent: &str) -> Result<(), String> {
    let mp = manifest_path();
    if let Some(d) = mp.parent() { let _ = std::fs::create_dir_all(d); }
    let lock = acquire_lock(&mp)?;
    let result = (|| {
        let mut m: serde_json::Value = std::fs::read_to_string(&mp).ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64).unwrap_or(0);
        m[agent] = serde_json::json!({
            "path": settings_path.display().to_string(),
            "installed_at_ms": ms,
            "psmux_version": env!("CARGO_PKG_VERSION"),
        });
        std::fs::write(&mp, serde_json::to_string_pretty(&m).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())
    })();
    let _ = std::fs::remove_file(lock);
    result
}

#[cfg(test)]
#[path = "../tests-rs/test_hooks_install.rs"]
mod test_hooks_install;

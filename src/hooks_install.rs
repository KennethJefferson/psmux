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

pub fn uninstall_claude(settings_path: &Path) -> Result<InstallReport, String> {
    let marker = owned_marker("claude");
    let lock = acquire_lock(settings_path)?;
    let result = (|| {
        let mut v = load(settings_path)?;
        if !strip_owned(&mut v, &marker) { return Ok(InstallReport { changed: false, backup: None }); }
        let bak = backup(settings_path)?;
        atomic_write(settings_path, &serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?)?;
        Ok(InstallReport { changed: true, backup: bak })
    })();
    let _ = std::fs::remove_file(lock);
    result
}

pub fn status_claude(settings_path: &Path) -> String {
    let marker = owned_marker("claude");
    match load(settings_path) {
        Ok(v) => {
            let installed = v["hooks"]["Stop"].as_array().map(|a| a.iter().any(|g| is_owned(g, &marker))).unwrap_or(false);
            format!("claude: {} ({})", if installed { "installed" } else { "not installed" }, settings_path.display())
        }
        Err(e) => format!("claude: unreadable ({})", e),
    }
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

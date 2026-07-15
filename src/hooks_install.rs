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

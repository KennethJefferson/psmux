use super::*;
use std::path::PathBuf;

static MANIFEST_SANDBOX: std::sync::Once = std::sync::Once::new();

fn manifest_sandbox_dir() -> PathBuf {
    std::env::temp_dir().join(format!("psmux-hooktest-{}-manifest", std::process::id()))
}

/// Point PSMUX_HOOKS_MANIFEST_DIR at a shared per-process temp dir exactly once,
/// so no test that calls install_claude can write the real per-user manifest.
/// Tests within one binary share the process env, hence the Once.
fn sandbox_manifest() {
    MANIFEST_SANDBOX.call_once(|| {
        let d = manifest_sandbox_dir();
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::env::set_var("PSMUX_HOOKS_MANIFEST_DIR", &d);
    });
}

fn tmp(name: &str) -> PathBuf {
    sandbox_manifest();
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
    // Manifest must land under the PSMUX_HOOKS_MANIFEST_DIR sandbox, not the real home.
    let mf = manifest_sandbox_dir().join("hooks-manifest.json");
    assert!(mf.exists(), "manifest not written under override dir: {}", mf.display());
    // Race-tolerant content check: the other tests in this binary run in
    // parallel and each install_claude rewrites this shared sandbox manifest
    // (plain fs::write, no lock), so a single read can catch a torn/mid-write
    // state or another test's path. Retry until a clean parse shows some
    // psmux-hooktest path from this process.
    let marker = format!("psmux-hooktest-{}", std::process::id());
    let mut last = String::new();
    let ok = (0..40).any(|_| {
        last = std::fs::read_to_string(&mf).unwrap_or_default();
        match serde_json::from_str::<serde_json::Value>(&last) {
            Ok(m) => m["claude"]["path"].as_str().map(|p| p.contains(&marker)).unwrap_or(false),
            Err(_) => { std::thread::sleep(std::time::Duration::from_millis(25)); false }
        }
    });
    assert!(ok, "manifest under override dir never showed a sandbox path; last content: {}", last);
}

#[test]
fn install_into_missing_parent_dir_creates_it() {
    // --project-local in a fresh project: .claude\ does not exist yet. The lock
    // file is acquired before any write, so install must create the parent dir
    // itself rather than failing with a misleading "could not lock" error.
    let p = tmp("freshdir");
    let p = p.parent().unwrap().join(".claude").join("settings.local.json");
    assert!(!p.parent().unwrap().exists());
    let r = install_claude(&p, std::path::Path::new("C:\\bin\\psmux.exe")).unwrap();
    assert!(r.changed);
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert!(v["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap().contains("hook-notify claude stop"));
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

#[test]
fn load_non_not_found_read_error_is_err_not_empty() {
    // Passing a directory (not a missing path) forces a non-NotFound read
    // error (e.g. "Is a directory" / access-denied), portably across platforms.
    // This must NOT be coerced to Ok({}) the way a genuine NotFound is.
    let p = tmp("load-direrr");
    std::fs::create_dir_all(&p).unwrap(); // p itself is now a directory, not a file
    let r = load(&p);
    assert!(r.is_err(), "non-NotFound read error must abort, not coerce to empty");
}

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

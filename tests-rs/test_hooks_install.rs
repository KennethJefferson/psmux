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

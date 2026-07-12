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

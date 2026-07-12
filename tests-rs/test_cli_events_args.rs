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

#[test]
fn notify_json_never_has_null_after_field() {
    // AMENDMENT 1 regression guard: build_notify_json must never itself
    // introduce an "after" key (that key belongs to the wait-event request
    // builder, not the notify payload) — and if it ever did, a present-but-null
    // value would be rejected by the server as a bad cursor.
    let env = NotifyEnv { pane_id: None, pane_instance: None, session_uid: String::new() };
    let j: serde_json::Value = serde_json::from_str(
        &build_notify_json(&env, "agent-notify", 0, None)).unwrap();
    assert!(j.get("after").is_none());
}

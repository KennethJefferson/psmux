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
fn stream_stopped_by_callback_is_clean() {
    // Preamble (OK + ack) skipped, one event line, then the closed frame:
    // callback returns false on "closed" -> clean stop -> true.
    let data = b"OK\n{\"type\":\"ack\"}\n{\"seq\":1}\n{\"type\":\"closed\",\"reason\":\"kill\"}\nignored\n";
    let mut r = std::io::BufReader::new(&data[..]);
    let mut seen = Vec::new();
    let clean = session::stream_lines_until_stopped(&mut r, 2, |line| {
        seen.push(line.to_string());
        !line.contains("\"type\":\"closed\"")
    });
    assert!(clean);
    assert_eq!(seen, vec![
        "{\"seq\":1}".to_string(),
        "{\"type\":\"closed\",\"reason\":\"kill\"}".to_string(),
    ]);
}

#[test]
fn stream_eof_without_closed_frame_is_transport_loss() {
    // Stream ends (EOF) before any closed frame: the callback never stops it
    // -> transport loss -> false.
    let data = b"OK\n{\"type\":\"ack\"}\n{\"seq\":1}\n";
    let mut r = std::io::BufReader::new(&data[..]);
    let clean = session::stream_lines_until_stopped(&mut r, 2, |line| {
        !line.contains("\"type\":\"closed\"")
    });
    assert!(!clean);
}

#[test]
fn stream_eof_during_preamble_is_transport_loss() {
    let data = b"OK\n";
    let mut r = std::io::BufReader::new(&data[..]);
    let clean = session::stream_lines_until_stopped(&mut r, 2, |_| true);
    assert!(!clean);
}

#[test]
fn stream_read_error_is_transport_loss() {
    struct FailingReader;
    impl std::io::Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out"))
        }
    }
    let mut r = std::io::BufReader::new(FailingReader);
    let clean = session::stream_lines_until_stopped(&mut r, 2, |_| true);
    assert!(!clean);
}

#[test]
fn warm_claimed_pane_detection() {
    // TMUX_PANE set + PSMUX_PANE_INSTANCE absent/empty => warm-claimed initial pane.
    assert!(is_warm_claimed_pane(Some("%1"), None, "sess-uid-abc"));
    assert!(is_warm_claimed_pane(Some("%1"), Some(""), "sess-uid-abc"));
    // Normal cold-spawned pane: both present.
    assert!(!is_warm_claimed_pane(Some("%1"), Some("3"), "sess-uid-abc"));
    // Fully outside psmux: TMUX_PANE itself absent — not the warm-claim case.
    assert!(!is_warm_claimed_pane(None, None, "sess-uid-abc"));
    assert!(!is_warm_claimed_pane(Some(""), None, "sess-uid-abc"));
}

#[test]
fn plain_tmux_without_psmux_marker_is_not_warm_claimed() {
    // TMUX_PANE set (plain tmux), no instance, and crucially no PSMUX_SESSION_UID.
    assert!(!is_warm_claimed_pane(Some("%3"), None, ""));
    // Inside psmux (session uid present) but instance missing => warm-claimed.
    assert!(is_warm_claimed_pane(Some("%3"), None, "sess-uid-abc"));
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

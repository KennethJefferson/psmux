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

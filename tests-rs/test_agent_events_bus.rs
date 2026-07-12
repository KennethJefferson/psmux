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

#[test]
fn subscriber_cap_refuses_65th() {
    let mut b = active_bus();
    // Keep every receiver alive so the bus can't prune any of them as
    // disconnected — the cap must be enforced on its own terms, not because
    // of channel cleanup.
    let mut kept_rx = Vec::new();
    for _ in 0..MAX_SUBSCRIBERS {
        let (tx, rx) = sync_channel(SUB_CHANNEL_CAP);
        let ack = b.subscribe(vec![], vec![], None, tx);
        assert!(!ack.refused);
        kept_rx.push(rx);
    }
    assert_eq!(b.subscriber_count(), MAX_SUBSCRIBERS);
    let (tx65, _rx65) = sync_channel(SUB_CHANNEL_CAP);
    let ack65 = b.subscribe(vec![], vec![], None, tx65);
    assert!(ack65.refused);
    assert!(ack65.ack_json.contains("\"refused\":\"max_subscribers\""));
    assert_eq!(b.subscriber_count(), MAX_SUBSCRIBERS);
}

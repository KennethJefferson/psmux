use super::*;
use std::sync::mpsc::sync_channel;

#[test]
fn hook_event_new_window_publishes_window_created() {
    let mut app = crate::types::AppState::new("t".to_string());
    app.session_uid = "s".to_string();
    app.bus.activate("s".to_string());
    let (tx, rx) = sync_channel(crate::events::SUB_CHANNEL_CAP);
    let _ = app.bus.subscribe(vec![], vec![], None, tx);
    publish_hook_event(&mut app, "after-new-window");
    match rx.try_recv().unwrap() {
        crate::events::SubscriberMsg::Event(e) => {
            assert_eq!(e.name, "window-created");
            assert_eq!(e.category, "window");
        }
        _ => panic!(),
    }
}

#[test]
fn dormant_warm_bus_stays_silent() {
    let mut app = crate::types::AppState::new("__warm__".to_string());
    let (tx, rx) = sync_channel(crate::events::SUB_CHANNEL_CAP);
    let _ = app.bus.subscribe(vec![], vec![], None, tx);
    publish_hook_event(&mut app, "after-new-window");
    assert!(rx.try_recv().is_err());
}

#[test]
fn pane_transitions_publish_pane_exited_with_instance() {
    let mut app = crate::types::AppState::new("t".to_string());
    app.bus.activate("s".to_string());
    let (tx, rx) = sync_channel(crate::events::SUB_CHANNEL_CAP);
    let _ = app.bus.subscribe(vec!["pane-exited".into()], vec![], None, tx);
    let transitions = vec![crate::tree::PaneTransition { pane_id: 5, instance: 12, window_id: 2 }];
    publish_pane_transitions(&mut app, &transitions);
    match rx.try_recv().unwrap() {
        crate::events::SubscriberMsg::Event(e) => {
            assert_eq!(e.pane, Some(5));
            assert_eq!(e.pane_instance, Some(12));
        }
        _ => panic!(),
    }
}

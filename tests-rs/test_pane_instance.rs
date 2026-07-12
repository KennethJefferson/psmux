use super::*;

#[test]
fn pane_instances_monotonic_and_never_reused() {
    let mut app = AppState::new("t".to_string());
    let a = app.alloc_pane_instance();
    let b = app.alloc_pane_instance();
    assert!(b > a);
}

#[test]
fn cursor_roundtrip() {
    let c = crate::events::Cursor {
        session_uid: "aaa".into(), bus_id: "bbb".into(), seq: 42,
    };
    let s = c.to_string();
    assert_eq!(s, "aaa:bbb:42");
    let p = crate::events::Cursor::parse(&s).unwrap();
    assert_eq!(p.seq, 42);
    assert_eq!(p.session_uid, "aaa");
    assert!(crate::events::Cursor::parse("garbage").is_none());
    assert!(crate::events::Cursor::parse("a:b:notanum").is_none());
}

#[test]
fn gen_uid_unique() {
    assert_ne!(crate::events::gen_uid(), crate::events::gen_uid());
}

use super::*;

#[test]
fn elevation_check_does_not_panic_and_is_stable() {
    let a = is_elevated();
    let b = is_elevated();
    assert_eq!(a, b);
}

#[test]
fn elevation_gate_env_override_recognized() {
    // gate helper: refuse only when elevated and override unset
    assert!(!should_refuse_elevated(false, None));
    assert!(!should_refuse_elevated(true, Some("1".into())));
    assert!(should_refuse_elevated(true, None));
    assert!(should_refuse_elevated(true, Some("0".into())));
}

// Issue #450: opt-in `@heal-crashed-panes` option parsing + heal-once guard.
use super::*;

fn app() -> AppState { AppState::new("test_heal".to_string()) }

#[test]
fn heal_defaults_off_when_unset() {
    let a = app();
    assert!(!a.heal_crashed_panes(), "must default OFF when @heal-crashed-panes is unset");
}

#[test]
fn heal_on_values_enable() {
    for v in ["on", "1", "true", "yes", "ON", "True"] {
        let mut a = app();
        a.user_options.insert("@heal-crashed-panes".to_string(), v.to_string());
        assert!(a.heal_crashed_panes(), "value {v:?} should enable heal");
    }
}

#[test]
fn heal_off_values_disable() {
    for v in ["off", "0", "false", "no", "", "garbage"] {
        let mut a = app();
        a.user_options.insert("@heal-crashed-panes".to_string(), v.to_string());
        assert!(!a.heal_crashed_panes(), "value {v:?} should NOT enable heal");
    }
}

#[test]
fn heal_once_guard_dedups() {
    // The server heal pass records healed pane ids so a shell that crashes on
    // every startup is respawned at most once (no infinite respawn loop).
    let mut a = app();
    assert!(!a.healed_pane_ids.contains(&7));
    a.healed_pane_ids.insert(7);
    assert!(a.healed_pane_ids.contains(&7), "id should be recorded after first heal");
    // second observation of the same id is a no-op insert
    let newly = a.healed_pane_ids.insert(7);
    assert!(!newly, "re-inserting an already-healed id must report no change");
}

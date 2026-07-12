#[test]
fn protected_keys_match_case_insensitively() {
    assert!(super::is_protected_env_key("TMUX_PANE"));
    assert!(super::is_protected_env_key("tmux_pane"));
    assert!(super::is_protected_env_key("Tmux_Pane"));
    assert!(super::is_protected_env_key("PSMUX_PANE_INSTANCE"));
    assert!(super::is_protected_env_key("psmux_session_uid"));
    assert!(!super::is_protected_env_key("PSMUX_HOOKS_DISABLED")); // user-settable exception
    assert!(!super::is_protected_env_key("MY_VAR"));
}

//! Public keybinding integration contract for library hosts.

use turtletap::{
    BindingContext, BindingId, BindingKind, KeyBinding, KeyCode, KeyModifiers, ShellConfig,
};

#[test]
fn host_uses_the_catalog_to_parse_replace_validate_and_encode() {
    let mut config = ShellConfig::new("host");
    let binding = "ctrl-x"
        .parse::<KeyBinding>()
        .expect("portable binding should parse");

    config
        .bindings
        .set_keys(BindingId::SessionInterrupt, vec![binding])
        .expect("catalog key should accept complete key chords");
    config
        .bindings
        .validate()
        .expect("replacement should preserve binding invariants");

    assert_eq!(
        config.bindings.keys(BindingId::SessionInterrupt),
        Some([binding].as_slice())
    );
    assert_eq!(
        binding.config_label().expect("binding should encode"),
        "ctrl-x"
    );
}

#[test]
fn catalog_exposes_context_and_configuration_metadata() {
    assert_eq!(BindingId::SessionInterrupt.kind(), BindingKind::Keys);
    assert_eq!(
        BindingId::SessionInterrupt.context(),
        BindingContext::Session
    );
    assert_eq!(
        BindingId::SessionInterrupt.context().config_scope(),
        Some("session")
    );
    assert_eq!(BindingId::SessionInterrupt.config_name(), "interrupt");
    assert_eq!(
        BindingId::SessionInterrupt.config_path(),
        "bindings.session.interrupt"
    );
    assert!(
        BindingId::KEY_BINDINGS.contains(&BindingId::DashboardKeybindings),
        "configuration UIs should discover the dashboard editor action"
    );
}

#[test]
fn programmatic_hosts_receive_the_same_text_safety_validation() {
    let mut config = ShellConfig::new("host");
    config
        .bindings
        .set_keys(
            BindingId::SessionInterrupt,
            vec![KeyBinding::new(KeyCode::Left, KeyModifiers::empty())],
        )
        .expect("session interrupt stores complete keys");

    let error = config
        .bindings
        .validate()
        .expect_err("plain arrows must remain available to text input");
    assert!(error.to_string().contains("context accepts text"));
}

#[test]
fn defaults_preserve_terminal_editing_and_use_conflict_free_shell_keys() {
    let bindings = ShellConfig::new("host").bindings;

    assert_eq!(
        bindings.palette,
        vec![
            KeyBinding::new(KeyCode::Char('`'), KeyModifiers::CONTROL),
            KeyBinding::new(KeyCode::Char(' '), KeyModifiers::CONTROL),
        ]
    );
    assert_eq!(
        bindings.redraw,
        vec![KeyBinding::new(KeyCode::F(5), KeyModifiers::empty())]
    );
    assert_eq!(
        bindings.session_line_start.first(),
        Some(&KeyBinding::new(KeyCode::Char('a'), KeyModifiers::CONTROL))
    );
    assert_eq!(
        bindings.session_line_end.first(),
        Some(&KeyBinding::new(KeyCode::Char('e'), KeyModifiers::CONTROL))
    );
    assert_eq!(
        bindings.session_delete_word.first(),
        Some(&KeyBinding::new(KeyCode::Char('w'), KeyModifiers::CONTROL))
    );
    bindings
        .validate()
        .expect("the complete default binding grammar should be valid");
}

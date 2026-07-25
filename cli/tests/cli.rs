//! Command-line help and usage behavior.

use std::process::Command;

fn run(arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_turtletap"))
        .args(arguments)
        .output()
        .expect("turtletap should run")
}

#[test]
fn every_top_level_command_accepts_help() {
    for command in [
        "open",
        "attach",
        "view",
        "take",
        "new",
        "rename",
        "list",
        "start",
        "status",
        "stop",
        "delete",
        "config",
        "doctor",
        "completions",
        "man",
    ] {
        let output = run(&[command, "--help"]);
        let stdout = String::from_utf8_lossy(&output.stdout);

        assert!(output.status.success(), "{command}: {output:?}");
        assert!(stdout.contains("Usage:"), "{command}: {stdout:?}");
        assert!(output.stderr.is_empty(), "{command}: {output:?}");
    }
}

#[test]
fn public_command_aliases_accept_help() {
    for command in ["create", "settings"] {
        let output = run(&[command, "--help"]);
        assert!(output.status.success(), "{command}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("Usage:"),
            "{command}: {output:?}"
        );
    }
}

#[test]
fn config_actions_have_focused_help() {
    for action in ["show", "path", "check", "init", "edit", "reload", "keys"] {
        let output = run(&["config", action, "--help"]);
        let stdout = String::from_utf8_lossy(&output.stdout);

        assert!(output.status.success(), "{action}: {output:?}");
        assert!(
            stdout.contains(&format!("turtletap config {action}")),
            "{action}: {stdout:?}"
        );
    }
}

#[test]
fn keybinding_editor_requires_a_terminal_and_never_writes_when_unavailable() {
    let output = run(&["config", "keys", "--format", "human"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        stderr.contains("requires an interactive terminal"),
        "{stderr:?}"
    );
}

#[test]
fn invalid_config_action_is_a_usage_error_with_recovery() {
    let output = run(&["config", "mystery"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        stderr.contains("unrecognized subcommand 'mystery'"),
        "{stderr:?}"
    );
    assert!(stderr.contains("Usage: turtletap config"), "{stderr:?}");
    assert!(stderr.contains("--help"), "{stderr:?}");
}

#[test]
fn config_check_distinguishes_defaults_from_an_existing_file() {
    let isolated =
        std::env::temp_dir().join(format!("turtletap-config-check-{}", std::process::id()));
    let output = Command::new(env!("CARGO_BIN_EXE_turtletap"))
        .args(["config", "check"])
        .env("XDG_CONFIG_HOME", &isolated)
        .env_remove("TURTLETAP_CONFIG")
        .output()
        .expect("turtletap should run");
    assert!(output.status.success(), "{output:?}");
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("captured output should default to JSON");
    assert_eq!(report["source"], "built-in defaults");
    assert_eq!(report["valid"], true);
}

#[test]
fn config_init_requires_an_explicit_format_switch() {
    let isolated =
        std::env::temp_dir().join(format!("turtletap-config-init-{}", std::process::id()));
    let invoke = |arguments: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_turtletap"))
            .args(arguments)
            .env("XDG_CONFIG_HOME", &isolated)
            .env_remove("TURTLETAP_CONFIG")
            .output()
            .expect("turtletap should run")
    };

    let kdl = invoke(&["config", "init", "kdl"]);
    assert!(kdl.status.success(), "{kdl:?}");
    let ambiguous = invoke(&["config", "init", "toml"]);
    assert_eq!(ambiguous.status.code(), Some(1), "{ambiguous:?}");
    let activated = invoke(&["config", "init", "toml", "--activate"]);
    assert!(activated.status.success(), "{activated:?}");
    let path = invoke(&["config", "path"]);
    let path = String::from_utf8(path.stdout).expect("captured path should be text");
    assert!(path.trim_end().ends_with("config.toml"), "{path:?}");
    let selected_kdl = invoke(&["config", "init", "kdl", "--activate"]);
    assert!(selected_kdl.status.success(), "{selected_kdl:?}");
    let selected: serde_json::Value =
        serde_json::from_slice(&selected_kdl.stdout).expect("selection should be JSON");
    assert_eq!(selected["created"], false);
    let path = invoke(&["config", "path"]);
    let path = String::from_utf8(path.stdout).expect("captured path should be text");
    assert!(path.trim_end().ends_with("config.kdl"), "{path:?}");

    let _ = std::fs::remove_dir_all(isolated);
}

#[test]
fn config_path_is_always_one_raw_path() {
    let isolated =
        std::env::temp_dir().join(format!("turtletap-config-path-{}", std::process::id()));
    let expected = isolated.join("turtletap/config.kdl");

    for arguments in [
        &["config", "path"][..],
        &["--format", "human", "config", "path"],
        &["--format", "json", "config", "path"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_turtletap"))
            .args(arguments)
            .env("XDG_CONFIG_HOME", &isolated)
            .env_remove("TURTLETAP_CONFIG")
            .output()
            .expect("turtletap should run");

        assert!(output.status.success(), "{arguments:?}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("{}\n", expected.display()),
            "{arguments:?}"
        );
        assert!(output.stderr.is_empty(), "{arguments:?}: {output:?}");
    }
}

#[cfg(unix)]
#[test]
fn config_path_is_safe_in_shell_substitution() {
    let isolated =
        std::env::temp_dir().join(format!("turtletap-config-shell-{}", std::process::id()));
    let expected = isolated.join("turtletap/config.kdl");
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg("path=$($TURTLETAP_BIN config path) && printf '%s' \"$path\"")
        .env("TURTLETAP_BIN", env!("CARGO_BIN_EXE_turtletap"))
        .env("XDG_CONFIG_HOME", &isolated)
        .env_remove("TURTLETAP_CONFIG")
        .output()
        .expect("shell should run");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        expected.display().to_string()
    );
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn invalid_report_format_is_a_usage_error() {
    let output = run(&["status", "--format", "yaml"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(stderr.contains("invalid value 'yaml'"), "{stderr:?}");
    assert!(stderr.contains("human"), "{stderr:?}");
    assert!(stderr.contains("json"), "{stderr:?}");
    assert!(stderr.contains("--help"), "{stderr:?}");
}

#[test]
fn help_is_honored_after_positional_arguments() {
    let output = run(&["attach", "build", "--help"]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Session name"),
        "{output:?}"
    );
}

#[test]
fn completions_and_man_page_are_generated() {
    for shell in ["bash", "elvish", "fish", "powershell", "zsh"] {
        let completions = run(&["completions", shell]);
        assert!(completions.status.success(), "{shell}: {completions:?}");
        assert!(
            String::from_utf8_lossy(&completions.stdout).contains("turtletap"),
            "{shell}: {completions:?}"
        );
        assert!(completions.stderr.is_empty(), "{shell}: {completions:?}");
    }

    let man = run(&["man"]);
    assert!(man.status.success(), "{man:?}");
    assert!(
        String::from_utf8_lossy(&man.stdout).contains(".TH turtletap"),
        "{man:?}"
    );
    assert!(man.stderr.is_empty(), "{man:?}");
}

#[test]
fn config_show_check_edit_reload_and_alias_cover_both_formats() {
    let isolated = std::env::temp_dir().join(format!(
        "turtletap-config-surface-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should follow the Unix epoch")
            .as_nanos()
    ));
    let invoke = |arguments: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_turtletap"))
            .args(arguments)
            .env("XDG_CONFIG_HOME", &isolated)
            .env_remove("TURTLETAP_CONFIG")
            .env_remove("VISUAL")
            .env("EDITOR", "/usr/bin/true")
            .output()
            .expect("turtletap should run")
    };

    let initialized = invoke(&["config", "init", "kdl"]);
    assert!(initialized.status.success(), "{initialized:?}");

    let kdl = invoke(&["config", "show", "kdl"]);
    assert!(kdl.status.success(), "{kdl:?}");
    let kdl = String::from_utf8_lossy(&kdl.stdout);
    assert!(kdl.contains("shell mouse-capture="), "{kdl:?}");
    assert!(kdl.contains("bindings {"), "{kdl:?}");

    let toml = invoke(&["settings", "show", "toml"]);
    assert!(toml.status.success(), "{toml:?}");
    let toml = String::from_utf8_lossy(&toml.stdout);
    assert!(toml.contains("[shell]"), "{toml:?}");
    assert!(toml.contains("[bindings.dashboard]"), "{toml:?}");

    let checked = invoke(&["--format", "human", "config", "check"]);
    assert!(checked.status.success(), "{checked:?}");
    assert!(
        String::from_utf8_lossy(&checked.stdout).contains("Configuration is valid:"),
        "{checked:?}"
    );

    let edited = invoke(&["--format", "json", "config", "edit"]);
    assert!(edited.status.success(), "{edited:?}");
    let edited: serde_json::Value =
        serde_json::from_slice(&edited.stdout).expect("edit report should be JSON");
    assert_eq!(edited["edited"], true);
    assert_eq!(edited["valid"], true);

    let reloaded = invoke(&["--format=json", "config", "reload"]);
    assert!(reloaded.status.success(), "{reloaded:?}");
    let reloaded: serde_json::Value =
        serde_json::from_slice(&reloaded.stdout).expect("reload report should be JSON");
    assert_eq!(reloaded["valid"], true);
    assert_eq!(
        reloaded["applies_on"],
        "automatic_file_watch_or_next_attach"
    );

    let _ = std::fs::remove_dir_all(isolated);
}

#[test]
#[cfg(unix)]
fn list_and_status_emit_parseable_json_from_a_live_resident() {
    let isolated = std::path::PathBuf::from("/tmp").join(format!(
        "turtletap-cli-report-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should follow the Unix epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&isolated).expect("isolated test directory should be created");
    let socket = isolated.join("resident.sock");
    let state = isolated.join("state");
    let invoke = |arguments: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_turtletap"))
            .args(arguments)
            .env("TURTLETAP_SOCKET", &socket)
            .env("TURTLETAP_STATE_DIR", &state)
            .output()
            .expect("turtletap should run")
    };

    let started = invoke(&["start"]);
    let list = invoke(&["list", "--format", "json"]);
    let status = invoke(&["status", "--format=json"]);
    let stopped = invoke(&["stop"]);
    let stopped_status = invoke(&["status", "--format", "json"]);

    assert!(started.status.success(), "{started:?}");
    assert!(list.status.success(), "{list:?}");
    assert!(status.status.success(), "{status:?}");
    assert!(stopped.status.success(), "{stopped:?}");
    assert!(stopped_status.status.success(), "{stopped_status:?}");
    let sessions: serde_json::Value =
        serde_json::from_slice(&list.stdout).expect("list should be JSON");
    let resident: serde_json::Value =
        serde_json::from_slice(&status.stdout).expect("status should be JSON");
    let inactive: serde_json::Value =
        serde_json::from_slice(&stopped_status.stdout).expect("stopped status should be JSON");
    assert!(sessions.is_array(), "{sessions:?}");
    assert!(
        sessions
            .as_array()
            .is_some_and(|sessions| sessions.iter().all(|session| {
                session.get("attached_clients").is_some() && session.get("viewers").is_none()
            })),
        "{sessions:?}"
    );
    assert_eq!(resident["resident"], "running");
    assert!(resident["pid"].is_u64(), "{resident:?}");
    assert!(resident["sessions"].is_array(), "{resident:?}");
    assert_eq!(inactive["resident"], "stopped");
    assert!(inactive["pid"].is_null(), "{inactive:?}");

    let _ = std::fs::remove_dir_all(isolated);
}

#[test]
#[cfg(unix)]
fn noninteractive_create_delete_and_stop_have_safe_machine_contracts() {
    let isolated = std::path::PathBuf::from("/tmp").join(format!(
        "turtletap-cli-mutations-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should follow the Unix epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&isolated).expect("isolated test directory should be created");
    let socket = isolated.join("resident.sock");
    let state = isolated.join("state");
    let invoke = |arguments: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_turtletap"))
            .args(arguments)
            .env("TURTLETAP_SOCKET", &socket)
            .env("TURTLETAP_STATE_DIR", &state)
            .output()
            .expect("turtletap should run")
    };

    let created = invoke(&["new", "build", "--no-attach"]);
    assert!(created.status.success(), "{created:?}");
    let created_json: serde_json::Value =
        serde_json::from_slice(&created.stdout).expect("create should emit JSON when captured");
    assert_eq!(created_json["session"]["name"], "build");

    let refused = invoke(&["delete", "build", "--no-input"]);
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");

    let listed = invoke(&["list"]);
    let sessions: serde_json::Value =
        serde_json::from_slice(&listed.stdout).expect("list should emit JSON when captured");
    assert!(
        sessions
            .as_array()
            .is_some_and(|sessions| sessions.iter().any(|session| session["name"] == "build")),
        "{sessions:?}"
    );

    let deleted = invoke(&["delete", "build", "--yes"]);
    assert!(deleted.status.success(), "{deleted:?}");
    let deleted_json: serde_json::Value =
        serde_json::from_slice(&deleted.stdout).expect("delete should emit JSON when captured");
    assert_eq!(deleted_json["deleted"], true);

    let stopped = invoke(&["stop"]);
    assert!(stopped.status.success(), "{stopped:?}");
    let stopped_again = invoke(&["stop"]);
    assert!(stopped_again.status.success(), "{stopped_again:?}");

    let _ = std::fs::remove_dir_all(isolated);
}

#[test]
#[cfg(unix)]
fn start_create_alias_rename_doctor_and_human_list_form_a_complete_cli_lifecycle() {
    let isolated = std::path::PathBuf::from("/tmp").join(format!(
        "turtletap-cli-lifecycle-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should follow the Unix epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&isolated).expect("isolated test directory should be created");
    let socket = isolated.join("resident.sock");
    let state = isolated.join("state");
    let config = isolated.join("config");
    let invoke = |arguments: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_turtletap"))
            .args(arguments)
            .env("TURTLETAP_SOCKET", &socket)
            .env("TURTLETAP_STATE_DIR", &state)
            .env("XDG_CONFIG_HOME", &config)
            .output()
            .expect("turtletap should run")
    };

    let started = invoke(&["--format=json", "start"]);
    assert!(started.status.success(), "{started:?}");
    let started: serde_json::Value =
        serde_json::from_slice(&started.stdout).expect("start report should be JSON");
    assert_eq!(started["resident"], "running");
    assert_eq!(started["started"], true);

    let created = invoke(&["--format", "json", "create", "build", "--no-attach"]);
    assert!(created.status.success(), "{created:?}");
    let created: serde_json::Value =
        serde_json::from_slice(&created.stdout).expect("create report should be JSON");
    assert_eq!(created["session"]["name"], "build");
    assert_eq!(created["attached"], false);

    let renamed = invoke(&["--format=json", "rename", "build", "ci"]);
    assert!(renamed.status.success(), "{renamed:?}");
    let renamed: serde_json::Value =
        serde_json::from_slice(&renamed.stdout).expect("rename report should be JSON");
    assert_eq!(renamed["old_name"], "build");
    assert_eq!(renamed["session"]["name"], "ci");

    let listed = invoke(&["--format", "human", "--no-color", "list"]);
    assert!(listed.status.success(), "{listed:?}");
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(listed.contains("NAME"), "{listed:?}");
    assert!(listed.contains("ci"), "{listed:?}");
    assert!(!listed.contains('\u{1b}'), "{listed:?}");

    let doctor = invoke(&["--format=json", "doctor"]);
    assert!(doctor.status.success(), "{doctor:?}");
    let doctor: serde_json::Value =
        serde_json::from_slice(&doctor.stdout).expect("doctor report should be JSON");
    assert_eq!(doctor["platform"]["persistent_sessions_supported"], true);
    assert_eq!(doctor["resident"]["running"], true);
    assert_eq!(
        doctor["resident"]["socket"],
        socket.to_string_lossy().as_ref()
    );
    assert_eq!(
        doctor["resident"]["state_dir"],
        state.to_string_lossy().as_ref()
    );
    assert_eq!(doctor["config"]["valid"], true);

    let deleted = invoke(&["--format=json", "delete", "ci", "--yes"]);
    assert!(deleted.status.success(), "{deleted:?}");
    let stopped = invoke(&["--format=json", "stop"]);
    assert!(stopped.status.success(), "{stopped:?}");

    let _ = std::fs::remove_dir_all(isolated);
}

#[test]
#[cfg(unix)]
fn invalid_session_names_are_rejected_before_creation() {
    let isolated = std::path::PathBuf::from("/tmp")
        .join(format!("turtletap-cli-names-{}", std::process::id()));
    for invalid in ["bad\nname", "   ", &"x".repeat(65)] {
        let output = Command::new(env!("CARGO_BIN_EXE_turtletap"))
            .args(["new", invalid, "--no-attach"])
            .env("TURTLETAP_SOCKET", isolated.join("resident.sock"))
            .env("TURTLETAP_STATE_DIR", isolated.join("state"))
            .output()
            .expect("turtletap should run");

        assert_eq!(output.status.code(), Some(2), "{output:?}");
        let error: serde_json::Value =
            serde_json::from_slice(&output.stderr).expect("usage error should be JSON");
        assert!(
            error["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("1-64 printable characters")),
            "{error:?}"
        );
    }
    let status = Command::new(env!("CARGO_BIN_EXE_turtletap"))
        .args(["status"])
        .env("TURTLETAP_SOCKET", isolated.join("resident.sock"))
        .env("TURTLETAP_STATE_DIR", isolated.join("state"))
        .output()
        .expect("status should run");
    let status: serde_json::Value =
        serde_json::from_slice(&status.stdout).expect("status should emit JSON");
    assert_eq!(status["resident"], "stopped");
    let _ = std::fs::remove_dir_all(isolated);
}

#[test]
#[cfg(unix)]
fn interactive_commands_reject_json_before_starting_or_mutating_the_resident() {
    let isolated = std::path::PathBuf::from("/tmp")
        .join(format!("turtletap-cli-preflight-{}", std::process::id()));
    let socket = isolated.join("resident.sock");
    let state = isolated.join("state");
    let invoke = |arguments: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_turtletap"))
            .args(arguments)
            .env("TURTLETAP_SOCKET", &socket)
            .env("TURTLETAP_STATE_DIR", &state)
            .output()
            .expect("turtletap should run")
    };

    for arguments in [
        &["open", "--format", "json"][..],
        &["attach", "default", "--format", "json"],
        &["view", "default", "--format", "json"],
        &["take", "default", "--yes", "--format", "json"],
        &["new", "build", "--format", "json"],
        &["config", "keys", "--format", "json"],
    ] {
        let rejected = invoke(arguments);
        assert_eq!(
            rejected.status.code(),
            Some(2),
            "{arguments:?}: {rejected:?}"
        );
        let error: serde_json::Value =
            serde_json::from_slice(&rejected.stderr).expect("usage error should be JSON");
        assert_eq!(error["error"]["code"], "usage_error", "{arguments:?}");
    }

    let status = invoke(&["status"]);
    let report: serde_json::Value =
        serde_json::from_slice(&status.stdout).expect("status should be JSON");
    assert_eq!(report["resident"], "stopped");

    let _ = std::fs::remove_dir_all(isolated);
}

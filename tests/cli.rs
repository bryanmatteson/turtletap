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
        "attach", "view", "take", "new", "rename", "list", "start", "status", "stop", "config",
    ] {
        let output = run(&[command, "--help"]);
        let stdout = String::from_utf8_lossy(&output.stdout);

        assert!(output.status.success(), "{command}: {output:?}");
        assert!(stdout.contains("Usage:"), "{command}: {stdout:?}");
        assert!(output.stderr.is_empty(), "{command}: {output:?}");
    }
}

#[test]
fn config_actions_have_focused_help() {
    for action in ["show", "path", "check", "init"] {
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
fn invalid_config_action_is_a_usage_error_with_recovery() {
    let output = run(&["config", "mystery"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(stderr.contains("turtletap config --help"), "{stderr:?}");
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
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success(), "{output:?}");
    assert!(stdout.contains("No configuration file"), "{stdout:?}");
    assert!(stdout.contains("built-in defaults are valid"), "{stdout:?}");
}

#[test]
fn invalid_report_format_is_a_usage_error() {
    let output = run(&["status", "--format", "yaml"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(stderr.contains("expected human or json"), "{stderr:?}");
    assert!(stderr.contains("turtletap status --help"), "{stderr:?}");
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
    assert_eq!(resident["resident"], "running");
    assert!(resident["pid"].is_u64(), "{resident:?}");
    assert!(resident["sessions"].is_array(), "{resident:?}");
    assert_eq!(inactive["resident"], "stopped");
    assert!(inactive["pid"].is_null(), "{inactive:?}");

    let _ = std::fs::remove_dir_all(isolated);
}

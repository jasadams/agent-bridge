use std::process::Command;

#[test]
fn obsolete_ack_command_is_not_exposed_or_accepted() {
    let state = tempfile::tempdir().unwrap();
    let help = Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .args(["--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(
        !String::from_utf8_lossy(&help.stdout)
            .lines()
            .any(|line| line.split_whitespace().next() == Some("ack"))
    );

    let output = Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .arg("--state-dir")
        .arg(state.path())
        .args(["ack", "--agent", "worker", "message-id"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unrecognized subcommand 'ack'"));
    assert!(!state.path().join("messages.sqlite3").exists());
}

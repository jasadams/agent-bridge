use agent_bridge::{
    claude_discovery::State,
    clients::{ConnectorState, StateError},
    protocol::Provider,
};
use serde_json::json;
use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    time::{Duration, Instant},
};

const A: &str = "11111111-1111-4111-8111-111111111111";
const B: &str = "22222222-2222-4222-8222-222222222222";

fn fixture(script: &str) -> (tempfile::TempDir, State) {
    let dir = tempfile::tempdir().unwrap();
    let executable = dir.path().join("claude");
    fs::write(&executable, format!("#!/bin/sh\n[ \"$#\" = 2 ] && [ \"$1\" = agents ] && [ \"$2\" = --json ] || exit 90\n{script}\n")).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let state = State::new(executable);
    (dir, state)
}
fn output(text: &str) -> (tempfile::TempDir, State) {
    fixture(&format!("cat <<'NATIVE_JSON'\n{text}\nNATIVE_JSON"))
}

#[test]
fn native_directory_needs_no_registration_and_names_are_only_metadata() {
    let (_dir, state) = output(&json!([
        {"sessionId":A,"name":"same","cwd":"/a","kind":"interactive","startedAt":1,"pid":1,"status":"idle","future":true},
        {"sessionId":B,"name":"same","cwd":"/b","kind":"background","state":"blocked"}
    ]).to_string());
    let rows = state.list(7).unwrap();
    assert_eq!(state.provider(), Provider::Claude);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].session, A);
    assert_eq!(rows[1].session, B);
    assert_eq!(rows[0].display_name.as_deref(), Some("same"));
    assert_eq!(rows[1].directory.as_deref(), Some("/b"));
    assert_eq!(rows[0].status.as_deref(), Some("idle"));
    assert_eq!(rows[1].status.as_deref(), Some("blocked"));
    assert!(
        rows.iter()
            .all(|s| s.updated_ms.is_none() && s.transport_endpoint.is_none())
    );
}

#[test]
fn exact_lookup_refreshes_rename_without_age_filtering_or_cached_state() {
    let (dir, state) = output(&json!([{"sessionId":A,"name":"before","startedAt":1}]).to_string());
    assert_eq!(
        state.get(A).unwrap().unwrap().display_name.as_deref(),
        Some("before")
    );
    fs::write(
        dir.path().join("claude"),
        format!("#!/bin/sh\nprintf '%s' '[{{\"sessionId\":\"{A}\",\"name\":\"after\"}}]'\n"),
    )
    .unwrap();
    let row = state.get(A).unwrap().unwrap();
    assert_eq!(row.display_name.as_deref(), Some("after"));
    assert!(row.status.is_none());
    assert_eq!(state.list(30).unwrap(), state.list(7).unwrap());
    assert!(state.get(B).unwrap().is_none());
}

#[test]
fn titles_short_and_noncanonical_ids_are_rejected_before_spawning() {
    let (_dir, state) = fixture("exit 7");
    for id in [
        "before",
        "claude:before",
        "11111111",
        "11111111111141118111111111111111",
    ] {
        assert!(matches!(state.get(id), Err(StateError::Incomplete(_))));
    }
}

#[test]
fn complete_empty_and_unknown_metadata_are_not_errors() {
    let (_dir, empty) = output("[]");
    assert!(empty.list(7).unwrap().is_empty());
    assert!(empty.get(A).unwrap().is_none());
    let (_dir, state) = output(&json!([{"sessionId":A}]).to_string());
    assert!(state.list(7).unwrap()[0].status.is_none());
}

#[test]
fn malformed_partial_and_conflicting_rows_never_become_empty_success() {
    for data in [
        "not-json".to_string(),
        "{}".into(),
        "[null]".into(),
        "[{}]".into(),
        json!([{"id":"11111111"}]).to_string(),
        json!([{"sessionId":"invalid"}]).to_string(),
        json!([{"sessionId":A,"name":7}]).to_string(),
        json!([{"sessionId":A,"startedAt":"bad"}]).to_string(),
        json!([{"sessionId":A},{"id":"short"}]).to_string(),
        json!([{"sessionId":A,"name":"a"},{"sessionId":A,"name":"b"}]).to_string(),
    ] {
        let (_dir, state) = output(&data);
        assert!(
            matches!(state.list(7), Err(StateError::Incomplete(_))),
            "{data}"
        );
        assert!(matches!(state.get(B), Err(StateError::Incomplete(_))));
    }
    let (_dir, state) = output(&json!([{"sessionId":A},{"sessionId":A}]).to_string());
    assert_eq!(state.list(7).unwrap().len(), 1);
}

#[test]
fn bounds_are_incomplete_not_truncated_results() {
    let (_dir, state) = output(&json!(vec![json!({"sessionId":A}); 4097]).to_string());
    assert!(matches!(state.list(7), Err(StateError::Incomplete(_))));
    let (_dir, state) = fixture("exec head -c 1048577 /dev/zero");
    assert!(matches!(state.list(7), Err(StateError::Incomplete(_))));
}

#[test]
fn command_failures_are_unavailable_and_stderr_cannot_block_or_leak() {
    let missing = State::new("/nonexistent/agent-bridge-test-claude");
    assert!(matches!(missing.list(7), Err(StateError::Unavailable(_))));
    let (_dir, state) =
        fixture("head -c 131072 /dev/zero >&2; echo private-diagnostic >&2; exit 7");
    let Err(StateError::Unavailable(error)) = state.list(7) else {
        panic!("expected unavailable")
    };
    assert!(error.contains("exited"), "{error}");
    assert!(!error.contains("private-diagnostic"));
    let (_dir, state) = fixture("head -c 131072 /dev/zero >&2; printf '[]'");
    assert!(state.list(7).unwrap().is_empty());
}

#[test]
fn timeout_kills_and_reaps_only_the_query_child() {
    let (dir, state) = fixture("echo $$ > \"$0.pid\"; exec sleep 30");
    let start = Instant::now();
    let Err(StateError::Unavailable(error)) = state.list(7) else {
        panic!("expected timeout")
    };
    assert!(error.contains("timed out"));
    assert!(start.elapsed() < Duration::from_secs(8));
    let pid: i32 = fs::read_to_string(dir.path().join("claude.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
}

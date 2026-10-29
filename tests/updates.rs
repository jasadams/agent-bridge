use agent_bridge::{broker::Broker, protocol::Request, update};
use serde_json::json;

#[tokio::test]
async fn status_exposes_cached_release_without_touching_board_or_existing_fields() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    let board = bound.broker.request(Request::BoardRead).unwrap();
    let before = bound.broker.request(Request::Status).unwrap();
    std::fs::write(
        state.join("update-status.json"),
        serde_json::to_vec(&json!({
            "latest_version":"99.0.0", "last_checked_at":123,
            "automatic_checks":false
        }))
        .unwrap(),
    )
    .unwrap();
    let after = bound.broker.request(Request::Status).unwrap();
    for field in [
        "version",
        "build_commit",
        "pid",
        "state_dir",
        "paused",
        "messages",
    ] {
        assert_eq!(before[field], after[field], "{field}");
    }
    assert_eq!(after["updates"]["available_version"], "99.0.0");
    assert_eq!(after["updates"]["automatic_checks"], false);
    assert_eq!(after["updates"]["last_checked_at"], 123);
    assert_eq!(
        after["updates"]["release_notes_url"],
        "https://github.com/jasadams/agent-bridge/releases/tag/v99.0.0"
    );
    assert_eq!(bound.broker.request(Request::BoardRead).unwrap(), board);

    update::record_installed(&state, "99.0.0").unwrap();
    let installed = bound.broker.request(Request::Status).unwrap();
    assert!(installed["updates"]["available_version"].is_null());
}

#[tokio::test]
async fn broken_advisory_cache_does_not_break_broker_status_or_board() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    std::fs::write(state.join("update-status.json"), "malformed").unwrap();
    let status = bound.broker.request(Request::Status).unwrap();
    assert_eq!(status["version"], agent_bridge::VERSION);
    assert!(
        status["updates"]["error"]
            .as_str()
            .unwrap()
            .contains("cache")
    );
    assert!(bound.broker.request(Request::BoardRead).is_ok());
}

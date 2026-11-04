use agent_bridge::{
    broker::{Broker, private_dir},
    protocol::{Provider, Request},
};
use std::{
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::Path,
};

fn listener(socket: &Path) -> UnixListener {
    let listener = UnixListener::bind(socket).unwrap();
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    listener
}

#[tokio::test]
async fn claude_binding_start_end_and_replacement_are_exact_endpoint_fenced() {
    let dir = tempfile::tempdir().unwrap();
    private_dir(dir.path()).unwrap();
    let bound = Broker::bind(dir.path().join("state"), "/missing/codex".into()).unwrap();
    let first = dir.path().join("first.sock");
    let replacement = dir.path().join("replacement.sock");
    let _first = listener(&first);
    let _replacement = listener(&replacement);
    let session = uuid::Uuid::new_v4().to_string();
    bound
        .broker
        .request(Request::ClaudeBind {
            session: session.clone(),
            endpoint: first.to_string_lossy().into_owned(),
        })
        .unwrap();
    bound
        .broker
        .request(Request::ClaudeBind {
            session: session.clone(),
            endpoint: replacement.to_string_lossy().into_owned(),
        })
        .unwrap();
    // A delayed SessionEnd for the old socket cannot remove the replacement.
    assert!(
        bound
            .broker
            .request(Request::ClaudeUnbind {
                session: session.clone(),
                endpoint: first.to_string_lossy().into_owned(),
            })
            .is_err()
    );
    bound
        .broker
        .request(Request::ClaudeUnbind {
            session: session.clone(),
            endpoint: replacement.to_string_lossy().into_owned(),
        })
        .unwrap();
    assert!(
        bound
            .broker
            .request(Request::ClaudeUnbind {
                session,
                endpoint: replacement.to_string_lossy().into_owned(),
            })
            .is_err()
    );
}

#[tokio::test]
async fn listing_reports_connector_state_and_never_a_stored_native_catalog() {
    let dir = tempfile::tempdir().unwrap();
    private_dir(dir.path()).unwrap();
    let bound = Broker::bind(dir.path().join("state"), "/missing/codex".into()).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    bound
        .broker
        .request(Request::Register {
            name: "alias".into(),
            provider: Provider::Claude,
            session: session.clone(),
            replace: false,
        })
        .unwrap();
    let listed = bound
        .broker
        .request(Request::List {
            all: true,
            max_age_days: 7,
        })
        .unwrap();
    // The alias is configuration, not a native row. It appears only if the
    // connector reports that exact session in this request.
    assert!(
        !listed["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "alias")
    );
    assert!(
        listed["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|provider| provider["provider"] == "claude")
    );
    assert_eq!(
        bound
            .broker
            .request(Request::Unregister {
                name: "alias".into(),
                session: session.clone(),
            })
            .unwrap()["unregistered"],
        "alias"
    );
    assert!(
        bound
            .broker
            .request(Request::Unregister {
                name: format!("claude-{session}"),
                session,
            })
            .is_err()
    );
}

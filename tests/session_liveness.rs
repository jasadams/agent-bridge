use agent_bridge::{
    broker::{Broker, private_dir},
    protocol::{Agent, Provider, Request},
};
use std::{
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::Path,
};

fn register(broker: &Broker, socket: &Path) -> String {
    let session = uuid::Uuid::new_v4().to_string();
    let name = format!("claude-{session}");
    broker
        .request(Request::Register {
            agent: Agent {
                name: name.clone(),
                provider: Provider::Claude,
                session,
                endpoint: socket.to_string_lossy().into_owned(),
                enabled: true,
            },
            replace: false,
        })
        .unwrap();
    name
}

fn listener(socket: &Path) -> UnixListener {
    let listener = UnixListener::bind(socket).unwrap();
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    listener
}

#[tokio::test]
async fn listing_hides_dead_claude_sockets_and_all_explains_them() {
    let dir = tempfile::tempdir().unwrap();
    private_dir(dir.path()).unwrap();
    let bound = Broker::bind(dir.path().join("state"), "/missing/codex".into()).unwrap();
    let socket = dir.path().join("native.sock");
    let live = listener(&socket);
    let name = register(&bound.broker, &socket);
    let rows = bound.broker.request(Request::List { all: false }).unwrap();
    assert!(rows.as_array().unwrap().iter().any(|r| r["name"] == name));
    drop(live);
    let rows = bound.broker.request(Request::List { all: false }).unwrap();
    assert!(!rows.as_array().unwrap().iter().any(|r| r["name"] == name));
    let rows = bound.broker.request(Request::List { all: true }).unwrap();
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == name)
        .unwrap();
    assert_eq!(row["stale_reason"], "socket_refused");
    assert_eq!(row["enabled"], true); // Listing never changes durable state.
    std::fs::remove_file(&socket).unwrap();
    let rows = bound.broker.request(Request::List { all: true }).unwrap();
    assert_eq!(
        rows.as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .unwrap()["stale_reason"],
        "socket_missing"
    );
}

#[tokio::test]
async fn reaper_requires_two_failures_and_resets_after_recovery_or_registration() {
    let dir = tempfile::tempdir().unwrap();
    private_dir(dir.path()).unwrap();
    let bound = Broker::bind(dir.path().join("state"), "/missing/codex".into()).unwrap();
    let socket = dir.path().join("native.sock");
    let live = listener(&socket);
    let name = register(&bound.broker, &socket);
    let enabled = || {
        let rows = bound.broker.request(Request::List { all: true }).unwrap();
        rows.as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .unwrap()["enabled"]
            .as_bool()
            .unwrap()
    };
    drop(live);
    bound.broker.reap_claude_sessions().unwrap();
    assert!(enabled());
    std::fs::remove_file(&socket).unwrap();
    let live = listener(&socket);
    bound.broker.reap_claude_sessions().unwrap(); // Recovery clears first failure.
    drop(live);
    bound.broker.reap_claude_sessions().unwrap();
    assert!(enabled());
    std::fs::remove_file(&socket).unwrap();
    let live = listener(&socket);
    let rows = bound.broker.request(Request::List { all: true }).unwrap();
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == name)
        .unwrap();
    let agent: Agent = serde_json::from_value(row.clone()).unwrap();
    bound
        .broker
        .request(Request::Register {
            agent,
            replace: false,
        })
        .unwrap();
    drop(live);
    bound.broker.reap_claude_sessions().unwrap();
    assert!(enabled());
    bound.broker.reap_claude_sessions().unwrap();
    assert!(!enabled());
    std::fs::remove_file(&socket).unwrap();
    let _live = listener(&socket);
    bound.broker.reap_claude_sessions().unwrap();
    assert!(!enabled()); // A new listener cannot revive an explicitly disabled route.
}

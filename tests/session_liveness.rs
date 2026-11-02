use agent_bridge::{
    broker::{Broker, private_dir},
    protocol::{Agent, Provider, Request},
};
use std::{
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::Path,
};

#[path = "support/claude_directory.rs"]
mod claude_directory;

const SESSION: &str = "123e4567-e89b-12d3-a456-426614174043";

fn native_rows() -> serde_json::Value {
    serde_json::json!([{
        "sessionId": SESSION,
        "name": "native liveness fixture",
        "cwd": "/fixture/workspace",
        "kind": "interactive",
        "startedAt": 1_700_000_000_000_u64,
        "status": "idle"
    }])
}

fn register(broker: &Broker, socket: &Path) -> String {
    let session = SESSION.to_string();
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
async fn listing_keeps_native_presence_separate_from_socket_delivery() {
    let dir = tempfile::tempdir().unwrap();
    private_dir(dir.path()).unwrap();
    let native = claude_directory::write_cli(dir.path(), &native_rows());
    let mut bound = Broker::bind(dir.path().join("state"), "/missing/codex".into()).unwrap();
    bound.broker = bound.broker.clone().with_claude_binary(native);
    let socket = dir.path().join("native.sock");
    let live = listener(&socket);
    let name = register(&bound.broker, &socket);
    let rows = bound.broker.request(Request::List { all: false }).unwrap();
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == name)
        .unwrap();
    assert_eq!(row["native_available"], true);
    assert_eq!(row["deliverable"], true);
    drop(live);
    let rows = bound.broker.request(Request::List { all: false }).unwrap();
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == name)
        .unwrap();
    assert_eq!(row["native_available"], true);
    assert_eq!(row["deliverable"], false);
    assert_eq!(row["delivery_unavailable_reason"], "socket_refused");
    let rows = bound.broker.request(Request::List { all: true }).unwrap();
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == name)
        .unwrap();
    assert_eq!(row["delivery_unavailable_reason"], "socket_refused");
    assert_eq!(row["enabled"], true); // Listing never changes durable state.
    std::fs::remove_file(&socket).unwrap();
    let rows = bound.broker.request(Request::List { all: true }).unwrap();
    assert_eq!(
        rows.as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .unwrap()["delivery_unavailable_reason"],
        "socket_missing"
    );
    // A live binding alone is not evidence of native membership.
    let _live = listener(&socket);
    claude_directory::set_rows(dir.path(), &serde_json::json!([]));
    let rows = bound.broker.request(Request::List { all: false }).unwrap();
    assert!(!rows.as_array().unwrap().iter().any(|r| r["name"] == name));
    let rows = bound.broker.request(Request::List { all: true }).unwrap();
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == name)
        .unwrap();
    assert_eq!(row["native_available"], false);
    assert_eq!(row["stale_reason"], "native_absent");
    assert_eq!(row["enabled"], true);
}

#[tokio::test]
async fn native_presence_survives_socket_retirement_without_reviving_owner_disable() {
    let dir = tempfile::tempdir().unwrap();
    private_dir(dir.path()).unwrap();
    let native = claude_directory::write_cli(dir.path(), &native_rows());
    let mut bound = Broker::bind(dir.path().join("state"), "/missing/codex".into()).unwrap();
    bound.broker = bound.broker.clone().with_claude_binary(native);
    let socket = dir.path().join("native.sock");
    let live = listener(&socket);
    let name = register(&bound.broker, &socket);
    drop(live);
    bound.broker.reap_claude_sessions().unwrap();
    bound.broker.reap_claude_sessions().unwrap();
    let rows = bound.broker.request(Request::List { all: false }).unwrap();
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .expect("a socket-only delivery retirement must not hide a natively present session");
    assert_eq!(row["native_available"], true);
    assert_eq!(row["deliverable"], false);
    let mut binding: Agent = serde_json::from_value(row.clone()).unwrap();
    bound
        .broker
        .request(Request::Unregister {
            name: name.clone(),
            session: binding.session.clone(),
        })
        .unwrap();
    std::fs::remove_file(&socket).unwrap();
    let _live = listener(&socket);
    binding.enabled = true;
    let rebound = bound
        .broker
        .request(Request::DeliveryBind {
            agent: binding.clone(),
        })
        .unwrap();
    assert_eq!(
        rebound["enabled"], false,
        "native rebind must not revive explicit owner disable"
    );
    let rows = bound.broker.request(Request::List { all: false }).unwrap();
    assert!(
        !rows
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["session"] == binding.session)
    );
    bound
        .broker
        .request(Request::Register {
            agent: binding,
            replace: false,
        })
        .unwrap();
    let rows = bound.broker.request(Request::List { all: false }).unwrap();
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == name && row["deliverable"] == true)
    );
}

#[tokio::test]
async fn reaper_requires_two_failures_and_resets_after_recovery_or_registration() {
    let dir = tempfile::tempdir().unwrap();
    private_dir(dir.path()).unwrap();
    let native = claude_directory::write_cli(dir.path(), &native_rows());
    let mut bound = Broker::bind(dir.path().join("state"), "/missing/codex".into()).unwrap();
    bound.broker = bound.broker.clone().with_claude_binary(native);
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
    assert!(enabled()); // Delivery retirement does not mutate owner intent.
    let db = rusqlite::Connection::open(dir.path().join("state/messages.sqlite3")).unwrap();
    let binding_enabled: bool = db
        .query_row("SELECT enabled FROM agents WHERE name=?", [&name], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(!binding_enabled);
    std::fs::remove_file(&socket).unwrap();
    let _live = listener(&socket);
    bound.broker.reap_claude_sessions().unwrap();
    assert!(enabled()); // Native identity remains present, but a listener is not a rebind.
}

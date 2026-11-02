use agent_bridge::{
    broker::Broker,
    protocol::{Agent, Provider, Request},
};
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
#[path = "support/claude_directory.rs"]
mod native_claude;

#[tokio::test]
async fn claude_native_membership_replaces_stored_discovery_without_registration() {
    let root = tempfile::Builder::new()
        .prefix("ab43-")
        .tempdir()
        .unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let native = uuid::Uuid::new_v4().to_string();
    let stale = uuid::Uuid::new_v4().to_string();
    let cli = root.path().join("claude");
    let rows = json!([{"sessionId":native,"name":"native-name","cwd":"/native/project","kind":"interactive","startedAt":1,"pid":42,"status":"idle"}]);
    std::fs::write(
        &cli,
        format!(
            "#!/bin/sh\n[ \"$1 $2\" = 'agents --json' ] || exit 9\nprintf '%s' '{}'\n",
            rows
        ),
    )
    .unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
    let bound = Broker::bind(root.path().join("state"), "/missing/codex".into()).unwrap();
    let broker = bound.broker.clone().with_claude_binary(cli);
    let socket = root.path().join("cc.sock");
    let _listener = tokio::net::UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    broker
        .request(Request::Register {
            agent: Agent {
                name: format!("claude-{stale}"),
                provider: Provider::Claude,
                session: stale.clone(),
                endpoint: socket.to_string_lossy().into(),
                enabled: true,
            },
            replace: false,
        })
        .unwrap();
    let rows = broker
        .request_async(Request::List { all: false })
        .await
        .unwrap();
    let rows = rows.as_array().unwrap();
    assert!(
        rows.iter().any(|row| row["session"] == native),
        "native UUID without a stored registration must appear: {rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row["session"] == stale),
        "stored registration must not override native membership"
    );
    let native_row = rows.iter().find(|row| row["session"] == native).unwrap();
    assert_eq!(native_row["deliverable"], false);
    assert_eq!(native_row["cwd"], "/native/project");
    assert_eq!(native_row["native_status"]["status"], "idle");
    assert!(native_row.get("native_updated_at").is_none());
}

#[tokio::test]
async fn unavailable_native_directory_is_explicit_and_does_not_erase_other_results() {
    let root = tempfile::Builder::new()
        .prefix("ab43-")
        .tempdir()
        .unwrap();
    let bound = Broker::bind(root.path().join("state"), "/missing/codex".into()).unwrap();
    let broker = bound
        .broker
        .clone()
        .with_claude_binary(root.path().join("missing-claude"));
    let value = broker
        .request_async(Request::Directory {
            all: false,
            max_age_days: 7,
        })
        .await
        .unwrap();
    assert_eq!(value["providers"]["claude"]["state"], "unavailable");
    assert!(
        value["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "user")
    );
    assert!(
        broker
            .request_async(Request::List { all: false })
            .await
            .is_err()
    );
    assert!(
        broker
            .request_async(Request::Directory {
                all: false,
                max_age_days: 0
            })
            .await
            .is_err()
    );
}

fn native(session: &str, name: &str) -> agent_bridge::directory::NativeSession {
    agent_bridge::directory::NativeSession {
        session: session.into(),
        name: Some(name.into()),
        cwd: Some("/fixture".into()),
        status: None,
        updated: None,
    }
}

#[test]
fn native_rename_exit_resume_preserves_overlay_without_mirroring_observations() {
    use agent_bridge::{
        db::Db,
        directory::{NativeSnapshot, project, resolve},
    };
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("db");
    let session = uuid::Uuid::new_v4().to_string();
    let replacement = uuid::Uuid::new_v4().to_string();
    let mut db = Db::open(&path).unwrap();
    let route = format!("claude-{session}");
    let binding = Agent {
        name: route.clone(),
        provider: Provider::Claude,
        session: session.clone(),
        endpoint: "/missing.sock".into(),
        enabled: true,
    };
    db.register(&binding, false).unwrap();
    db.observe_claude_name(&session, "obsolete-cached-name")
        .unwrap();
    let snapshot = |name| {
        vec![(
            Provider::Claude,
            NativeSnapshot::Complete(vec![native(&session, name)]),
        )]
    };
    assert_eq!(
        resolve(&db, &snapshot("first"), "claude:first")
            .unwrap()
            .name,
        route
    );
    assert!(resolve(&db, &snapshot("second"), "claude:first").is_err());
    assert!(resolve(&db, &snapshot("second"), "claude:obsolete-cached-name").is_err());
    assert_eq!(
        resolve(&db, &snapshot("second"), "claude:second")
            .unwrap()
            .session,
        session
    );
    let count = db.agents().unwrap().len();
    let absent = vec![(Provider::Claude, NativeSnapshot::Complete(vec![]))];
    assert!(
        !project(&db, &absent, false)
            .unwrap()
            .agents
            .iter()
            .any(|row| row["provider"] == "claude")
    );
    assert_eq!(
        project(&db, &absent, true)
            .unwrap()
            .agents
            .iter()
            .find(|row| row["name"] == route)
            .unwrap()["stale_reason"],
        "native_absent"
    );
    assert!(db.agent(&route).unwrap().enabled);
    assert_eq!(db.agents().unwrap().len(), count);
    assert_eq!(
        resolve(&db, &snapshot("resumed"), &route).unwrap().session,
        session
    );
    let other = vec![(
        Provider::Claude,
        NativeSnapshot::Complete(vec![native(&replacement, "second")]),
    )];
    let error = resolve(&db, &other, "claude:second")
        .unwrap_err()
        .to_string();
    assert!(error.contains("no delivery binding"));
    assert!(
        !project(&db, &other, false)
            .unwrap()
            .agents
            .iter()
            .any(|row| row["session"] == session)
    );
    assert_eq!(db.agents().unwrap().len(), count);
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(db.agent(&route).unwrap().session, session);
    assert_eq!(
        db.display_name(&Provider::Claude, &session)
            .unwrap()
            .as_deref(),
        Some("obsolete-cached-name")
    );
}

#[test]
fn disabled_aliases_and_canonical_collisions_never_auto_enable_identity() {
    use agent_bridge::{
        db::Db,
        directory::{NativeSnapshot, project},
    };
    let root = tempfile::tempdir().unwrap();
    let mut db = Db::open(&root.path().join("db")).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    db.register(
        &Agent {
            name: "explicit-alias".into(),
            provider: Provider::Claude,
            session: session.clone(),
            endpoint: "/missing.sock".into(),
            enabled: false,
        },
        false,
    )
    .unwrap();
    let snapshots = vec![(
        Provider::Claude,
        NativeSnapshot::Complete(vec![native(&session, "same")]),
    )];
    assert!(
        !project(&db, &snapshots, false)
            .unwrap()
            .agents
            .iter()
            .any(|row| row["provider"] == "claude")
    );
    let all = project(&db, &snapshots, true).unwrap();
    assert!(
        all.agents
            .iter()
            .filter(|row| row["provider"] == "claude")
            .all(|row| row["enabled"] == false)
    );
    let collision = uuid::Uuid::new_v4().to_string();
    db.register(
        &Agent {
            name: format!("claude-{session}"),
            provider: Provider::Claude,
            session: collision,
            endpoint: "/missing.sock".into(),
            enabled: true,
        },
        false,
    )
    .unwrap();
    assert!(project(&db, &snapshots, false).is_err());
}

#[test]
fn snapshot_errors_and_duplicate_titles_never_guess_or_fall_back_to_stored_names() {
    use agent_bridge::{
        db::Db,
        directory::{NativeSnapshot, project, resolve},
    };
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(&root.path().join("db")).unwrap();
    let a = uuid::Uuid::new_v4().to_string();
    let b = uuid::Uuid::new_v4().to_string();
    let snapshot = vec![(
        Provider::Claude,
        NativeSnapshot::Complete(vec![native(&a, "same"), native(&b, "same")]),
    )];
    assert!(
        resolve(&db, &snapshot, "same")
            .unwrap_err()
            .to_string()
            .contains("ambiguous")
    );
    assert_eq!(project(&db, &snapshot, false).unwrap().agents.len(), 3);
    assert_eq!(db.agents().unwrap().len(), 1);
    let duplicate = vec![(
        Provider::Claude,
        NativeSnapshot::Complete(vec![native(&a, "a"), native(&a, "a")]),
    )];
    assert!(project(&db, &duplicate, false).is_err());
    let incomplete = vec![(
        Provider::Claude,
        NativeSnapshot::Incomplete("partial page".into()),
    )];
    let directory = project(&db, &incomplete, false).unwrap();
    assert_eq!(directory.providers["claude"].state, "incomplete");
    assert!(directory.complete_agents().is_err());
    assert!(resolve(&db, &incomplete, "same").is_err());
}

#[test]
fn native_title_never_uses_a_canonical_looking_binding_for_another_identity() {
    use agent_bridge::{
        db::Db,
        directory::{NativeSnapshot, resolve},
    };
    let root = tempfile::tempdir().unwrap();
    let mut db = Db::open(&root.path().join("db")).unwrap();
    let native_id = uuid::Uuid::new_v4().to_string();
    let other = uuid::Uuid::new_v4().to_string();
    db.register(
        &Agent {
            name: format!("claude-{native_id}"),
            provider: Provider::Codex,
            session: other,
            endpoint: String::new(),
            enabled: true,
        },
        false,
    )
    .unwrap();
    let snapshot = vec![(
        Provider::Claude,
        NativeSnapshot::Complete(vec![native(&native_id, "reviewer")]),
    )];
    assert!(
        resolve(&db, &snapshot, "claude:reviewer").is_err(),
        "native label must never select a colliding alias with another UUID/provider"
    );
}

#[test]
fn legacy_ambiguity_is_not_discarded_when_a_fresh_claude_title_matches() {
    use agent_bridge::{
        db::Db,
        directory::{NativeSnapshot, resolve},
    };
    let root = tempfile::tempdir().unwrap();
    let mut db = Db::open(&root.path().join("db")).unwrap();
    let cc = uuid::Uuid::new_v4().to_string();
    db.register(
        &Agent {
            name: format!("claude-{cc}"),
            provider: Provider::Claude,
            session: cc.clone(),
            endpoint: "/missing.sock".into(),
            enabled: true,
        },
        false,
    )
    .unwrap();
    for _ in 0..2 {
        let id = uuid::Uuid::new_v4().to_string();
        db.observe_codex(&id, Some("reviewer"), true).unwrap();
    }
    let snapshot = vec![(
        Provider::Claude,
        NativeSnapshot::Complete(vec![native(&cc, "reviewer")]),
    )];
    assert!(
        resolve(&db, &snapshot, "reviewer").is_err(),
        "ambiguous legacy candidates must not disappear from a joint resolution"
    );
    assert_eq!(
        resolve(&db, &snapshot, "claude:reviewer").unwrap().session,
        cc
    );
    assert!(resolve(&db, &snapshot, "codex:reviewer").is_err());
}

#[tokio::test]
async fn sharing_and_sender_labels_follow_native_rename_not_cached_old_uuid() {
    let root = tempfile::Builder::new()
        .prefix("ab43-names-")
        .tempdir()
        .unwrap();
    agent_bridge::broker::private_dir(root.path()).unwrap();
    let a = uuid::Uuid::new_v4().to_string();
    let b = uuid::Uuid::new_v4().to_string();
    let cli = native_claude::write_cli(
        root.path(),
        &json!([
            {"sessionId":a,"name":"author","cwd":"/fixture","kind":"interactive","startedAt":1,"pid":1,"status":"idle"},
            {"sessionId":b,"name":"reviewer","cwd":"/fixture","kind":"interactive","startedAt":1,"pid":2,"status":"idle"}
        ]),
    );
    let state = root.path().join("state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    let broker = bound.broker.clone().with_claude_binary(cli);
    let socket = root.path().join("native.sock");
    let _listener = tokio::net::UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    for session in [&a, &b] {
        broker
            .request(Request::Register {
                agent: Agent {
                    name: format!("claude-{session}"),
                    provider: Provider::Claude,
                    session: session.clone(),
                    endpoint: socket.to_string_lossy().into(),
                    enabled: true,
                },
                replace: false,
            })
            .unwrap();
    }
    {
        let db = agent_bridge::db::Db::open(&state.join("messages.sqlite3")).unwrap();
        db.observe_claude_name(&a, "reviewer").unwrap();
        db.observe_claude_name(&b, "old-b-name").unwrap();
    }
    let peer = libp2p::identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id()
        .to_string();
    broker
        .request(Request::PeerPair {
            peer_id: peer.clone(),
            fingerprint: peer.clone(),
            alias: Some("peer".into()),
        })
        .unwrap();
    let shared = broker
        .request(Request::PeerShare {
            peer: "peer".into(),
            agent: "claude:reviewer".into(),
            shared: true,
        })
        .unwrap();
    let sent = broker
        .request(Request::Send {
            id: uuid::Uuid::new_v4().to_string(),
            sender: "claude:reviewer".into(),
            recipient: "user".into(),
            body: "fixture".into(),
            parent: None,
        })
        .unwrap();
    assert!(
        shared["grants"]
            .as_array()
            .unwrap()
            .iter()
            .all(|grant| grant["session"] == b)
            && sent["sender"] == format!("claude-{b}"),
        "current native name must not select the old UUID for sharing or sender: grants={shared}, send={sent}"
    );
    assert_eq!(sent["sender"], format!("claude-{b}"));
    assert!(
        broker
            .request(Request::PeerShare {
                peer: "peer".into(),
                agent: "claude:old-b-name".into(),
                shared: true
            })
            .is_err()
    );
}

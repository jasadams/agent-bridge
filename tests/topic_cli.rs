use agent_bridge::{
    broker::{self, Broker},
    protocol::Request,
};
use serde_json::{Value, json};
use std::path::Path;

async fn command(state: &Path, args: &[&str]) -> std::process::Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .arg("--state-dir")
        .arg(state)
        .args(args)
        .output()
        .await
        .unwrap()
}
async fn success(state: &Path, args: &[&str]) -> Value {
    let output = command(state, args).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn topic_cli_persists_id_addressed_conversations_without_delivery_work() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    let broker = bound.broker.clone();
    let server = tokio::spawn(broker::serve(bound));
    let created = success(&state, &["topic", "create", "Planning"]).await;
    let topic = created["reference"]["topic_id"].as_str().unwrap();
    let bridge = created["reference"]["bridge_id"].as_str().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let args = [
        "topic",
        "publish",
        topic,
        "--id",
        &id,
        "--author",
        "inbox-user",
        "--message",
        "Ready",
    ];
    let accepted = success(&state, &args).await;
    assert_eq!(accepted["duplicate"], false);
    assert_eq!(accepted["post"]["author"]["bridge_id"], bridge);
    assert_eq!(success(&state, &args).await["duplicate"], true);
    let mut conflict = args;
    conflict[8] = "Different";
    let failed = command(&state, &conflict).await;
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("topic_conflict"));
    let reply_id = uuid::Uuid::new_v4().to_string();
    let reply = success(
        &state,
        &[
            "topic",
            "publish",
            topic,
            "--id",
            &reply_id,
            "--author",
            "inbox-user",
            "--message",
            "Reply",
            "--parent",
            &id,
            "--mentions",
            r#"[{"agent_id":"inbox-user"}]"#,
        ],
    )
    .await;
    assert_eq!(reply["post"]["parent"], id);
    let read = success(
        &state,
        &["topic", "read", topic, "--cursor", "", "--limit", "1"],
    )
    .await;
    assert_eq!(read["posts"][0]["id"], id);
    assert_eq!(read["has_more"], true);
    let next = read["next_cursor"].as_str().unwrap();
    assert_eq!(
        success(&state, &["topic", "read", topic, "--cursor", next]).await["posts"][0]["id"],
        reply_id
    );
    let latest = success(&state, &["topic", "read", topic, "--limit", "1"]).await;
    let previous = latest["previous_cursor"].as_str().unwrap();
    assert_eq!(
        success(&state, &["topic", "read", topic, "--before", previous]).await["posts"][0]["id"],
        id
    );
    let explicit = success(&state, &["topic", "get", topic, "--bridge-id", bridge]).await;
    assert_eq!(explicit["reference"], created["reference"]);
    assert_eq!(
        success(&state, &["topic", "rename", topic, "Delivery"]).await["reference"],
        created["reference"]
    );
    success(&state, &["topic", "archive", topic]).await;
    assert!(
        success(&state, &["topic", "list"]).await["topics"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        success(&state, &["topic", "list", "--include-archived"]).await["topics"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let blocked = command(
        &state,
        &[
            "topic",
            "publish",
            topic,
            "--id",
            &uuid::Uuid::new_v4().to_string(),
            "--author",
            "inbox-user",
            "--message",
            "Blocked",
        ],
    )
    .await;
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("topic_archived"));
    assert_eq!(success(&state, &args).await["post"], accepted["post"]);
    success(&state, &["topic", "restore", topic]).await;
    assert_eq!(
        success(&state, &["topic", "read", topic]).await["posts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let agents = success(&state, &["topic", "agents"]).await;
    assert_eq!(agents["agents"][0]["reference"]["agent_id"], "inbox-user");
    assert_eq!(
        broker
            .request(Request::History {
                agent: None,
                limit: 100
            })
            .unwrap(),
        json!([])
    );
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn topic_cli_rejects_name_targets_foreign_scope_and_missing_retry_ids() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    let server = tokio::spawn(broker::serve(bound));
    let created = success(&state, &["topic", "create", "Planning"]).await;
    let topic = created["reference"]["topic_id"].as_str().unwrap();
    for args in [
        vec!["topic", "get", "Planning"],
        vec![
            "topic",
            "publish",
            topic,
            "--author",
            "inbox-user",
            "--message",
            "No UUID",
        ],
        vec![
            "topic",
            "publish",
            topic,
            "--id",
            "bad",
            "--author",
            "user",
            "--message",
            "No alias",
        ],
        vec!["topic", "read", topic, "--limit", "101"],
        vec!["topic", "read", topic, "--cursor", "bad"],
        vec!["topic", "read", topic, "--before", ""],
        vec!["topic", "read", topic, "--cursor", "", "--before", "bad"],
    ] {
        assert!(!command(&state, &args).await.status.success(), "{args:?}");
    }
    let foreign = libp2p_identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id()
        .to_string();
    let failed = command(&state, &["topic", "get", topic, "--bridge-id", &foreign]).await;
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("topic_not_local"));
    let publication_id = uuid::Uuid::new_v4().to_string();
    let alias = command(
        &state,
        &[
            "topic",
            "publish",
            topic,
            "--id",
            &publication_id,
            "--author",
            "user",
            "--message",
            "No friendly-name targeting",
        ],
    )
    .await;
    assert!(!alias.status.success());
    assert!(String::from_utf8_lossy(&alias.stderr).contains("topic_agent_unknown"));
    let failed = command(
        &state,
        &[
            "topic",
            "publish",
            topic,
            "--id",
            &publication_id,
            "--author",
            "inbox-user",
            "--author-bridge-id",
            &foreign,
            "--message",
            "No remote authors",
        ],
    )
    .await;
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("topic_not_local"));
    server.abort();
    let _ = server.await;
}

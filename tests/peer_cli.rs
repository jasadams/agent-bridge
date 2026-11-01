mod support;

use agent_bridge::{
    broker::{self, Broker},
    network::identity::Identity,
};
use serde_json::Value;
use std::path::Path;

async fn cli(state: &Path, args: &[&str]) -> std::process::Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .arg("--state-dir")
        .arg(state)
        .args(args)
        .output()
        .await
        .unwrap()
}

async fn json(state: &Path, args: &[&str]) -> Value {
    let output = cli(state, args).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn local_cli_manages_peer_trust_and_explicit_network_lifecycle() {
    let dir = tempfile::Builder::new()
        .prefix("ab-peer-")
        .tempdir_in("/tmp")
        .unwrap();
    let state = dir.path().join("local");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    let server = tokio::spawn(broker::serve(bound));
    let own = json(&state, &["identity", "show"]).await;
    assert_eq!(own, json(&state, &["identity", "show"]).await);
    let remote = Identity::load_or_create(&dir.path().join("remote"))
        .unwrap()
        .info();
    let denied = cli(
        &state,
        &[
            "peers",
            "pair",
            &remote.peer_id,
            "--fingerprint",
            "incorrect",
        ],
    )
    .await;
    assert!(!denied.status.success());
    assert_eq!(
        json(&state, &["peers", "list"]).await,
        serde_json::json!([])
    );
    let own_id = own["peer_id"].as_str().unwrap();
    assert!(
        !cli(&state, &["peers", "pair", own_id, "--fingerprint", own_id])
            .await
            .status
            .success()
    );
    let paired = json(
        &state,
        &[
            "peers",
            "pair",
            &remote.peer_id,
            "--fingerprint",
            &remote.fingerprint,
            "--alias",
            "workstation",
        ],
    )
    .await;
    assert_eq!(paired["peer_id"], remote.peer_id);
    json(
        &state,
        &[
            "peers",
            "add",
            "workstation",
            "--address",
            "/ip4/127.0.0.1/udp/44001/quic-v1",
        ],
    )
    .await;
    assert_eq!(
        json(&state, &["list", "--remote"]).await,
        serde_json::json!([])
    );
    json(
        &state,
        &[
            "register",
            "inbox",
            "reviewer",
            "--session",
            "stable-session",
        ],
    )
    .await;
    let allowed = json(
        &state,
        &[
            "peers",
            "allow",
            "workstation",
            "--agent",
            "reviewer",
            "--permission",
            "receive",
        ],
    )
    .await;
    assert_eq!(allowed["grants"].as_array().unwrap().len(), 1);
    assert_eq!(allowed["grants"][0]["session"], "stable-session");
    let denied = json(
        &state,
        &[
            "peers",
            "deny",
            "workstation",
            "--agent",
            "reviewer",
            "--permission",
            "receive",
        ],
    )
    .await;
    assert!(denied["grants"].as_array().unwrap().is_empty());
    json(&state, &["peers", "revoke", "workstation"]).await;
    let peers = json(&state, &["peers", "list"]).await;
    assert_eq!(peers[0]["revoked"], true);
    assert!(peers[0]["grants"].as_array().unwrap().is_empty());
    assert_eq!(json(&state, &["status"]).await["network"]["enabled"], false);
    assert_eq!(
        json(
            &state,
            &[
                "network",
                "enable",
                "--listen",
                "/ip4/127.0.0.1/udp/0/quic-v1"
            ]
        )
        .await["enabled"],
        true
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let status = json(&state, &["status"]).await;
            if status["network"]["listening"]
                .as_array()
                .is_some_and(|addresses| !addresses.is_empty())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("CLI enable did not start listener");
    assert_eq!(
        json(&state, &["network", "disable"]).await["enabled"],
        false
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if json(&state, &["status"]).await["network"]["enabled"] == false {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("CLI disable did not stop listener");
    server.abort();
    let _ = server.await;
    support::wait_for_shutdown(&state).await;
    let rebound = Broker::bind(state, "/missing/codex".into()).unwrap();
    assert_eq!(
        rebound
            .broker
            .request(agent_bridge::protocol::Request::Identity)
            .unwrap(),
        own
    );
    assert_eq!(
        rebound
            .broker
            .request(agent_bridge::protocol::Request::PeersList)
            .unwrap(),
        peers
    );
}

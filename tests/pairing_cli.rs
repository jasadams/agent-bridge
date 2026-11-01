mod support;

use agent_bridge::broker::{self, Broker};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio};
use tokio::io::AsyncWriteExt;

fn start(state: &Path) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    tokio::spawn(broker::serve(
        Broker::bind(state.into(), "/missing/codex".into()).unwrap(),
    ))
}

async fn cli(state: &Path, args: &[&str], input: Option<&str>) -> std::process::Output {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .arg("--state-dir")
        .arg(state)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = input {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input.as_bytes()).await.unwrap();
        stdin.write_all(b"\n").await.unwrap();
    } else {
        drop(child.stdin.take());
    }
    child.wait_with_output().await.unwrap()
}

async fn value(state: &Path, args: &[&str], input: Option<&str>) -> Value {
    let output = cli(state, args, input).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn mutual_pairing_cli_uses_stdin_persists_offer_and_grants_no_access() {
    let root = tempfile::Builder::new()
        .prefix("ab-pair-cli-")
        .tempdir_in("/tmp")
        .unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    let alice = start(&a);
    let bob = start(&b);
    let alice_id = value(&a, &["identity", "show"], None).await["peer_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let bob_id = value(&b, &["identity", "show"], None).await["peer_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let offer = value(&a, &["peers", "offer"], None).await;
    assert_eq!(offer["fingerprint"], alice_id);
    assert!(
        !cli(
            &b,
            &["peers", "join", "--fingerprint", &bob_id],
            offer["offer"].as_str(),
        )
        .await
        .status
        .success()
    );
    assert_eq!(value(&b, &["peers", "list"], None).await, json!([]));
    let joined = value(
        &b,
        &[
            "peers",
            "join",
            "--fingerprint",
            &alice_id,
            "--alias",
            "alice",
        ],
        offer["offer"].as_str(),
    )
    .await;
    assert_eq!(joined["fingerprint"], bob_id);
    assert_eq!(joined["peer"]["peer_id"], alice_id);
    assert_eq!(joined["mutual_pairing_complete"], false);
    assert!(joined["peer"]["grants"].as_array().unwrap().is_empty());
    assert_eq!(value(&a, &["peers", "list"], None).await, json!([]));
    let response = joined["response"].as_str().unwrap();
    assert!(
        !cli(
            &a,
            &["peers", "finish", "--fingerprint", &alice_id],
            Some(response),
        )
        .await
        .status
        .success()
    );
    alice.abort();
    let _ = alice.await;
    support::wait_for_shutdown(&a).await;
    let alice = start(&a);
    let finished = value(
        &a,
        &[
            "peers",
            "finish",
            "--fingerprint",
            &bob_id,
            "--alias",
            "bob",
        ],
        Some(response),
    )
    .await;
    assert_eq!(finished["local_pairing_complete"], true);
    assert_eq!(finished["peer"]["peer_id"], bob_id);
    assert!(finished["peer"]["grants"].as_array().unwrap().is_empty());
    for state in [&a, &b] {
        assert_eq!(
            value(state, &["status"], None).await["network"]["enabled"],
            false
        );
        assert_eq!(value(state, &["list", "--remote"], None).await, json!([]));
        assert!(
            value(state, &["peers", "list"], None).await[0]["grants"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    value(&a, &["peers", "revoke", "bob"], None).await;
    assert!(
        !cli(
            &a,
            &["peers", "finish", "--fingerprint", &bob_id],
            Some(response),
        )
        .await
        .status
        .success()
    );
    assert_eq!(
        value(&a, &["peers", "list"], None).await[0]["revoked"],
        true
    );
    alice.abort();
    bob.abort();
    let _ = alice.await;
    let _ = bob.await;
}

#[tokio::test]
async fn cancellation_and_invalid_input_cannot_create_issuer_trust() {
    let root = tempfile::Builder::new()
        .prefix("ab-pair-cancel-")
        .tempdir_in("/tmp")
        .unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    let alice = start(&a);
    let bob = start(&b);
    assert!(
        !cli(&a, &["peers", "offer", "--seconds", "601"], None)
            .await
            .status
            .success()
    );
    assert!(
        !cli(&b, &["peers", "join", "--fingerprint", "wrong"], Some(""))
            .await
            .status
            .success()
    );
    let offer = value(&a, &["peers", "offer", "--seconds", "120"], None).await;
    let fingerprint = offer["fingerprint"].as_str().unwrap();
    let joined = value(
        &b,
        &["peers", "join", "--fingerprint", fingerprint],
        offer["offer"].as_str(),
    )
    .await;
    value(
        &a,
        &["peers", "cancel-offer", offer["nonce"].as_str().unwrap()],
        None,
    )
    .await;
    assert!(
        !cli(
            &a,
            &[
                "peers",
                "finish",
                "--fingerprint",
                joined["fingerprint"].as_str().unwrap(),
            ],
            joined["response"].as_str(),
        )
        .await
        .status
        .success()
    );
    assert_eq!(value(&a, &["peers", "list"], None).await, json!([]));
    alice.abort();
    bob.abort();
    let _ = alice.await;
    let _ = bob.await;
}

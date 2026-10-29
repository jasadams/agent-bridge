use agent_bridge::{
    board::{Edit, Snapshot},
    broker::{self, Broker},
    protocol::Request,
};
use serde_json::{Value, json};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn cli(state: &Path, args: &[&str]) -> std::process::Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .arg("--state-dir")
        .arg(state)
        .args(args)
        .output()
        .await
        .unwrap()
}

async fn success(state: &Path, args: &[&str]) -> String {
    let output = cli(state, args).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[tokio::test]
async fn noninteractive_create_list_and_raw_are_isolated_and_explicit() {
    let root = tempfile::Builder::new()
        .prefix("ab-board-cli-")
        .tempdir_in("/tmp")
        .unwrap();
    let state = root.path().join("state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    let server = tokio::spawn(broker::serve(bound));
    let initial = success(&state, &["board", "--list"]).await;
    assert!(initial.starts_with("main\tabsent\t"));
    assert_eq!(success(&state, &["board", "--raw"]).await, "");
    assert!(!state.join("boards/main.md").exists());
    for name in ["zeta", "alpha"] {
        assert_eq!(
            success(&state, &["board", "--create", name]).await,
            format!("Created board {name}\n")
        );
        let board: agent_bridge::board::Name = name.parse().unwrap();
        let snapshot: Snapshot = serde_json::from_value(
            broker::rpc(&state, &Request::board_read(board.clone()))
                .await
                .unwrap(),
        )
        .unwrap();
        broker::rpc(
            &state,
            &Request::board_update(
                board,
                snapshot.revision,
                vec![Edit::Set {
                    markdown: format!("{name} content\n"),
                }],
                None,
            ),
        )
        .await
        .unwrap();
    }
    assert_eq!(
        success(&state, &["board", "alpha", "--raw"]).await,
        "alpha content\n"
    );
    assert_eq!(
        success(&state, &["board", "zeta", "--raw"]).await,
        "zeta content\n"
    );
    let listing = success(&state, &["board", "--list"]).await;
    assert_eq!(
        listing
            .lines()
            .map(|line| line.split('\t').next().unwrap())
            .collect::<Vec<_>>(),
        vec!["alpha", "main", "zeta"]
    );
    assert!(!listing.contains("content"));
    for args in [
        vec!["board", "--create", "alpha"],
        vec!["board", "missing", "--raw"],
        vec!["board", "--create", "../escape"],
    ] {
        assert!(!cli(&state, &args).await.status.success());
    }
    assert_eq!(
        success(&state, &["board", "alpha", "--raw"]).await,
        "alpha content\n"
    );
    assert!(!state.join("boards/missing.md").exists());
    assert!(!state.join("boards/main.md").exists());
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn legacy_main_cli_works_and_named_requests_fail_safely_against_old_broker() {
    let root = tempfile::Builder::new()
        .prefix("ab-old-board-")
        .tempdir_in("/tmp")
        .unwrap();
    let listener = tokio::net::UnixListener::bind(root.path().join("bridge.sock")).unwrap();
    let stub = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..5 {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            BufReader::new(read).read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let response = if request == json!({"op":"board_read"}) {
                json!({"ok":true,"result":{"name":"main","markdown":"legacy main\n","revision":"legacy","path":"/private/boards/main.md","exists":true}})
            } else {
                json!({"ok":false,"error":"Invalid request: unknown variant; upgrade the broker for named boards"})
            };
            requests.push(request);
            write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        }
        requests
    });
    assert_eq!(
        success(root.path(), &["board", "--raw"]).await,
        "legacy main\n"
    );
    assert_eq!(
        success(root.path(), &["board", "main", "--raw"]).await,
        "legacy main\n"
    );
    for args in [
        vec!["board", "project", "--raw"],
        vec!["board", "--create", "project"],
        vec!["board", "--list"],
    ] {
        let output = cli(root.path(), &args).await;
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("upgrade the broker"));
    }
    let requests = stub.await.unwrap();
    assert_eq!(requests[2]["op"], "named_board_read");
    assert_eq!(requests[2]["board"], "project");
    assert_eq!(requests[3]["op"], "board_create");
    assert_eq!(requests[4]["op"], "board_list");
}

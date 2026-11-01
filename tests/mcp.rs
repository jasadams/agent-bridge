//! Black-box MCP stdio checks against the real broker. No native model, global
//! client configuration or existing bridge state is touched.
mod support;

use agent_bridge::{
    broker::{self, Broker},
    db::Db,
    network::wire::RemoteAgent,
    protocol::{Agent, MAX_FRAME, Provider, Request},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap, os::unix::fs::PermissionsExt, path::Path, process::Stdio, time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::{Child, ChildStdin, ChildStdout},
    time::timeout,
};

struct Running {
    broker: Broker,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}
impl Running {
    fn start(state: &Path) -> Self {
        let bound = Broker::bind(state.into(), "/missing/codex".into()).unwrap();
        Self {
            broker: bound.broker.clone(),
            task: tokio::spawn(broker::serve(bound)),
        }
    }
    fn request(&self, request: Request) -> Value {
        self.broker.request(request).unwrap()
    }
    fn inbox(&self, name: &str) {
        self.request(Request::Register {
            agent: Agent {
                name: name.into(),
                provider: Provider::Inbox,
                session: format!("inbox-{name}"),
                endpoint: String::new(),
                enabled: true,
            },
            replace: false,
        });
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Native {
    name: String,
    session: String,
    received: tokio::sync::mpsc::Receiver<Value>,
    task: tokio::task::JoinHandle<()>,
}
impl Native {
    fn start(running: &Running, directory: &Path, name: &str) -> Self {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.join("native.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        running.request(Request::Register {
            agent: Agent {
                name: name.into(),
                provider: Provider::Claude,
                session: session.clone(),
                endpoint: socket.to_string_lossy().into(),
                enabled: true,
            },
            replace: false,
        });
        let (captured, received) = tokio::sync::mpsc::channel(32);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let mut bytes = Vec::new();
                timeout(
                    Duration::from_secs(5),
                    stream.take(MAX_FRAME as u64).read_to_end(&mut bytes),
                )
                .await
                .unwrap()
                .unwrap();
                if bytes.is_empty() {
                    continue; // Payload-free liveness connection, no native submission.
                }
                let frame: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(frame["type"], "user");
                let _ = captured.try_send(frame);
            }
        });
        Self {
            name: name.into(),
            session,
            received,
            task,
        }
    }
}
impl Drop for Native {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Mcp {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    next_id: u64,
}
impl Mcp {
    fn start(state: &Path) -> Self {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
            .args(["--state-dir", state.to_str().unwrap(), "mcp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        Self {
            input: child.stdin.take(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
            next_id: 1,
        }
    }
    async fn write(&mut self, value: &Value) {
        let mut frame = serde_json::to_vec(value).unwrap();
        frame.push(b'\n');
        self.input
            .as_mut()
            .unwrap()
            .write_all(&frame)
            .await
            .unwrap();
    }
    async fn read(&mut self) -> Value {
        let mut line = String::new();
        let read = timeout(Duration::from_secs(10), self.output.read_line(&mut line))
            .await
            .expect("MCP response timed out")
            .unwrap();
        assert_ne!(read, 0, "MCP closed stdout before responding");
        let value: Value = serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("non-JSON protocol output: {error}: {line:?}"));
        assert_eq!(value["jsonrpc"], "2.0");
        value
    }
    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.write(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await;
        loop {
            let response = self.read().await;
            if response["id"] == id {
                return response;
            }
            // SDK notifications and an uncorrelated parse error for a rejected
            // oversized frame still have to be valid JSON-RPC, never log text.
            assert!(response.get("id").is_none() || response["id"].is_null());
        }
    }
    async fn initialize(&mut self) {
        let response = self
            .request(
                "initialize",
                json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"agent-bridge-integration","version":"1"}}),
            )
            .await;
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(response["result"]["protocolVersion"], "2025-11-25");
        self.write(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await;
    }
    async fn call(&mut self, name: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name":name,"arguments":arguments}))
            .await
    }
    async fn listening(&mut self) -> Value {
        timeout(Duration::from_secs(8), async {
            loop {
                let status = self.call("connection_status", json!({})).await;
                let status = &structured(&status)["result"]["status"];
                if status["network"]["listening"]
                    .as_array()
                    .is_some_and(|addresses| !addresses.is_empty())
                {
                    break status.clone();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap()
    }
    async fn pairing_state(&mut self, id: &str, phase: &str) -> Value {
        timeout(Duration::from_secs(12), async {
            loop {
                let status = self.call("pairing_status", json!({})).await;
                if let Some(view) = structured(&status)["result"]["requests"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|view| view["id"] == id && view["state"] == phase)
                {
                    break view.clone();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("pairing did not reach {phase}"))
    }
    async fn close(mut self) {
        drop(self.input.take());
        let status = timeout(Duration::from_secs(5), self.child.wait())
            .await
            .expect("MCP did not stop after stdin closed")
            .unwrap();
        assert!(status.success(), "MCP failed on clean EOF: {status}");
    }
}

fn failed(response: &Value) -> bool {
    response.get("error").is_some() || response["result"]["isError"] == true
}

fn structured(response: &Value) -> &Value {
    assert!(!failed(response), "{response}");
    &response["result"]["structuredContent"]
}

fn root() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ab-mcp-")
        .tempdir_in("/tmp")
        .unwrap()
}

#[tokio::test]
async fn topic_tools_discover_and_publish_durable_history_without_native_input() {
    let root = root();
    let state = root.path().join("state");
    let running = Running::start(&state);
    let mut native = Native::start(&running, &root.path().join("native"), "friendly-author");
    running.inbox("reader");
    let mut mcp = Mcp::start(&state);
    mcp.initialize().await;
    let tools = mcp.request("tools/list", json!({})).await;
    let tools = tools["result"]["tools"].as_array().unwrap();
    for name in [
        "create_topic",
        "list_topics",
        "get_topic",
        "rename_topic",
        "archive_topic",
        "restore_topic",
        "publish_topic_post",
        "read_topic_posts",
        "list_topic_agents",
    ] {
        assert!(tools.iter().any(|tool| tool["name"] == name), "{name}");
    }
    let publish = tools
        .iter()
        .find(|tool| tool["name"] == "publish_topic_post")
        .unwrap();
    assert!(
        publish["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field == "id")
    );
    assert!(publish["description"].as_str().unwrap().contains("durable"));
    let created = mcp
        .call("create_topic", json!({"name":"Engineering"}))
        .await;
    let reference = structured(&created)["result"]["reference"].clone();
    let topic = json!({"topic_id":reference["topic_id"]});
    let catalog = mcp.call("list_topic_agents", json!({})).await;
    let author = json!({"agent_id":format!("claude-{}", native.session)});
    let agents = structured(&catalog)["result"]["agents"].as_array().unwrap();
    assert!(
        agents
            .iter()
            .any(|agent| agent["reference"]["agent_id"] == author["agent_id"]
                && agent["reference"]["bridge_id"] == reference["bridge_id"]
                && agent["display_name"].is_string())
    );
    let id = uuid::Uuid::new_v4().to_string();
    let args = json!({"topic":topic,"id":id,"author":author,"body":"Exact body\n  ✓","mentions":[{"agent_id":"inbox-inbox-reader"},{"agent_id":"inbox-inbox-reader"}]});
    let accepted = mcp.call("publish_topic_post", args.clone()).await;
    let post = structured(&accepted)["result"]["post"].clone();
    assert_eq!(structured(&accepted)["result"]["duplicate"], false);
    assert_eq!(post["topic"], reference);
    assert_eq!(post["body"], "Exact body\n  ✓");
    assert_eq!(post["mentions"].as_array().unwrap().len(), 1);
    assert!(post["author_display_name"].is_string());
    assert!(post.get("delivered").is_none());
    let mut explicit = args.clone();
    explicit["topic"] = reference.clone();
    explicit["author"]["bridge_id"] = reference["bridge_id"].clone();
    let retried = mcp.call("publish_topic_post", explicit).await;
    assert_eq!(structured(&retried)["result"]["duplicate"], true);
    assert_eq!(structured(&retried)["result"]["post"], post);
    let mut conflicting = args.clone();
    conflicting["body"] = json!("different");
    let conflict = mcp.call("publish_topic_post", conflicting).await;
    assert!(failed(&conflict));
    assert!(conflict.to_string().contains("topic_conflict"));
    assert_eq!(conflict["result"]["structuredContent"]["request_id"], id);
    let reply_id = uuid::Uuid::new_v4().to_string();
    let reply = mcp.call("publish_topic_post", json!({"topic":reference,"id":reply_id,"author":{"agent_id":"inbox-inbox-reader"},"body":"Reply","parent":id})).await;
    assert_eq!(structured(&reply)["result"]["post"]["parent"], id);
    let read = mcp.call("read_topic_posts", json!({"topic":topic})).await;
    assert_eq!(
        structured(&read)["result"]["posts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let conn = rusqlite::Connection::open(state.join("messages.sqlite3")).unwrap();
    let stored: String = conn
        .query_row("SELECT body FROM topic_posts WHERE id=?", [&id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(stored, "Exact body\n  ✓");
    let dm_count: i64 = conn
        .query_row("SELECT count(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(dm_count, 0);
    running.request(Request::Unregister {
        name: native.name.clone(),
        session: native.session.clone(),
    });
    let disabled_author = mcp.call("publish_topic_post", json!({"topic":topic,"id":uuid::Uuid::new_v4().to_string(),"author":author,"body":"disabled author"})).await;
    assert!(failed(&disabled_author));
    assert!(disabled_author.to_string().contains("topic_agent_unknown"));
    let stale = mcp.call("list_topic_agents", json!({})).await;
    assert!(
        structured(&stale)["result"]["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|agent| agent["reference"]["agent_id"] == author["agent_id"])
    );
    let renamed = mcp
        .call("rename_topic", json!({"topic":topic,"name":"Delivery"}))
        .await;
    assert_eq!(structured(&renamed)["result"]["reference"], reference);
    let archived = mcp.call("archive_topic", json!({"topic":topic})).await;
    assert_eq!(structured(&archived)["result"]["archived"], true);
    let retry = mcp.call("publish_topic_post", args).await;
    assert_eq!(structured(&retry)["result"]["post"], post);
    let denied = mcp.call("publish_topic_post", json!({"topic":topic,"id":uuid::Uuid::new_v4().to_string(),"author":{"agent_id":"inbox-inbox-reader"},"body":"archived"})).await;
    assert!(failed(&denied));
    assert!(denied.to_string().contains("topic_archived"));
    let active = mcp.call("list_topics", json!({})).await;
    assert_eq!(structured(&active)["result"]["topics"], json!([]));
    let all = mcp
        .call("list_topics", json!({"include_archived":true}))
        .await;
    assert_eq!(
        structured(&all)["result"]["topics"][0]["reference"],
        reference
    );
    let restored = mcp.call("restore_topic", json!({"topic":topic})).await;
    assert_eq!(structured(&restored)["result"]["archived"], false);
    let after_restore = mcp.call("publish_topic_post", json!({"topic":topic,"id":uuid::Uuid::new_v4().to_string(),"author":{"agent_id":"inbox-inbox-reader"},"body":"Restored","mentions":[author]})).await;
    assert_eq!(structured(&after_restore)["result"]["post"]["sequence"], 3);
    mcp.close().await;
    drop(running);
    support::wait_for_shutdown(&state).await;
    let _restarted = Running::start(&state);
    let mut mcp = Mcp::start(&state);
    mcp.initialize().await;
    let read = mcp.call("read_topic_posts", json!({"topic":topic})).await;
    assert_eq!(structured(&read)["result"]["posts"][0], post);
    assert!(
        timeout(Duration::from_millis(350), native.received.recv())
            .await
            .is_err(),
        "topics must not submit native input"
    );
    mcp.close().await;
}

#[tokio::test]
async fn topic_reads_keep_default_latest_forward_before_and_all_page_flags() {
    let root = root();
    let state = root.path().join("state");
    let running = Running::start(&state);
    running.inbox("writer");
    let mut mcp = Mcp::start(&state);
    mcp.initialize().await;
    let created = mcp.call("create_topic", json!({"name":"Pages"})).await;
    let topic = structured(&created)["result"]["reference"].clone();
    for index in 1..=52 {
        let result = mcp.call("publish_topic_post", json!({"topic":topic,"id":uuid::Uuid::new_v4().to_string(),"author":{"agent_id":"inbox-inbox-writer"},"body":format!("post {index}")})).await;
        assert_eq!(structured(&result)["result"]["post"]["sequence"], index);
    }
    let latest = mcp.call("read_topic_posts", json!({"topic":topic})).await;
    let page = &structured(&latest)["result"];
    assert_eq!(page["posts"].as_array().unwrap().len(), 50);
    assert_eq!(page["posts"][0]["sequence"], 3);
    assert_eq!(page["high_water"], 52);
    assert_eq!(page["has_older"], true);
    assert_eq!(page["has_more"], false);
    let older = mcp
        .call(
            "read_topic_posts",
            json!({"topic":topic,"before":page["previous_cursor"],"limit":2}),
        )
        .await;
    let older = &structured(&older)["result"];
    assert_eq!(older["posts"][0]["sequence"], 1);
    assert_eq!(older["posts"][1]["sequence"], 2);
    assert_eq!(older["has_older"], false);
    assert_eq!(older["has_more"], true);
    let forward = mcp
        .call(
            "read_topic_posts",
            json!({"topic":topic,"cursor":older["next_cursor"],"limit":1}),
        )
        .await;
    assert_eq!(structured(&forward)["result"]["posts"][0]["sequence"], 3);
    let full = mcp
        .call(
            "read_topic_posts",
            json!({"topic":topic,"cursor":"","limit":100}),
        )
        .await;
    assert_eq!(
        structured(&full)["result"]["posts"]
            .as_array()
            .unwrap()
            .len(),
        52
    );
    let empty = mcp
        .call(
            "read_topic_posts",
            json!({"topic":topic,"cursor":page["next_cursor"]}),
        )
        .await;
    let empty = &structured(&empty)["result"];
    assert_eq!(empty["posts"], json!([]));
    assert_eq!(empty["next_cursor"], page["next_cursor"]);
    assert_eq!(empty["has_more"], false);
    for args in [
        json!({"topic":topic,"limit":0}),
        json!({"topic":topic,"limit":101}),
        json!({"topic":topic,"cursor":"bad"}),
        json!({"topic":topic,"before":""}),
        json!({"topic":topic,"cursor":page["next_cursor"],"before":page["previous_cursor"]}),
    ] {
        let result = mcp.call("read_topic_posts", args).await;
        assert!(failed(&result), "{result}");
    }
    let other = mcp.call("create_topic", json!({"name":"Other"})).await;
    let wrong = mcp
        .call(
            "read_topic_posts",
            json!({"topic":structured(&other)["result"]["reference"],"cursor":page["next_cursor"]}),
        )
        .await;
    assert!(failed(&wrong));
    mcp.close().await;
}

#[tokio::test]
async fn topic_tools_reject_aliases_foreign_refs_and_malformed_publications() {
    let root = root();
    let state = root.path().join("state");
    let running = Running::start(&state);
    running.inbox("writer");
    let mut mcp = Mcp::start(&state);
    mcp.initialize().await;
    let created = mcp.call("create_topic", json!({"name":"Same name"})).await;
    let topic = structured(&created)["result"]["reference"].clone();
    let second = mcp.call("create_topic", json!({"name":"Same name"})).await;
    assert_ne!(structured(&second)["result"]["reference"], topic);
    let foreign = libp2p_identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id()
        .to_string();
    for selector in [
        json!("Same name"),
        json!({"topic_id":"Same name"}),
        json!({"topic_id":topic["topic_id"],"bridge_id":foreign}),
        json!({"topic_id":topic["topic_id"],"name":"Same name"}),
    ] {
        let rejected = mcp.call("get_topic", json!({"topic":selector})).await;
        assert!(failed(&rejected), "{rejected}");
    }
    let shorthand = mcp
        .call(
            "get_topic",
            json!({"topic":{"topic_id":topic["topic_id"],"bridge_id":null}}),
        )
        .await;
    assert_eq!(structured(&shorthand)["result"]["reference"], topic);
    let base = json!({"topic":topic,"id":uuid::Uuid::new_v4().to_string(),"author":{"agent_id":"inbox-inbox-writer"},"body":"body"});
    let mut missing_id = base.clone();
    missing_id.as_object_mut().unwrap().remove("id");
    assert!(failed(&mcp.call("publish_topic_post", missing_id).await));
    for (key, value) in [
        ("id", json!("bad")),
        ("author", json!({"agent_id":"writer"})),
        (
            "author",
            json!({"agent_id":"inbox-inbox-writer","bridge_id":foreign}),
        ),
        ("body", json!(" ")),
        ("body", json!("x".repeat(65537))),
        ("mentions", json!([{"agent_id":"writer"}])),
        ("mentions", Value::Null),
        ("parent", json!(uuid::Uuid::new_v4().to_string())),
        ("unknown", json!(true)),
    ] {
        let mut args = base.clone();
        args[key] = value;
        let rejected = mcp.call("publish_topic_post", args).await;
        assert!(failed(&rejected), "{rejected}");
    }
    let read = mcp.call("read_topic_posts", json!({"topic":topic})).await;
    assert_eq!(structured(&read)["result"]["posts"], json!([]));
    mcp.close().await;
}

#[tokio::test]
async fn topic_capacity_and_oversized_pages_fail_explicitly_without_truncation() {
    let root = root();
    let state = root.path().join("state");
    let running = Running::start(&state);
    running.inbox("writer");
    let mut mcp = Mcp::start(&state);
    mcp.initialize().await;
    let created = mcp.call("create_topic", json!({"name":"Large"})).await;
    let topic = structured(&created)["result"]["reference"].clone();
    for _ in 0..17 {
        let post = mcp.call("publish_topic_post", json!({"topic":topic,"id":uuid::Uuid::new_v4().to_string(),"author":{"agent_id":"inbox-inbox-writer"},"body":"x".repeat(65536)})).await;
        assert!(!failed(&post), "{post}");
    }
    let oversized = mcp
        .call("read_topic_posts", json!({"topic":topic,"limit":100}))
        .await;
    assert!(failed(&oversized));
    assert_eq!(
        oversized["result"]["structuredContent"]["code"],
        "result_too_large"
    );
    let smaller = mcp
        .call(
            "read_topic_posts",
            json!({"topic":topic,"cursor":"","limit":1}),
        )
        .await;
    assert_eq!(structured(&smaller)["result"]["posts"][0]["sequence"], 1);
    assert_eq!(structured(&smaller)["result"]["has_more"], true);
    let local = topic["bridge_id"].as_str().unwrap();
    let mut db = Db::open(&state.join("messages.sqlite3")).unwrap();
    // Seed real isolated storage at the production topic bound, without a new
    // owner setting or a mock broker that bypasses the actual capacity check.
    for index in 1..1000 {
        db.topic_create(local, &format!("seed {index}"), &Default::default())
            .unwrap();
    }
    let rejected = mcp
        .call("create_topic", json!({"name":"Over capacity"}))
        .await;
    assert!(failed(&rejected));
    assert!(rejected.to_string().contains("topic_capacity"));
    let listed = mcp.call("list_topics", json!({})).await;
    assert_eq!(
        structured(&listed)["result"]["topics"]
            .as_array()
            .unwrap()
            .len(),
        1000
    );
    mcp.close().await;
}

#[tokio::test]
async fn rejected_opencode_send_does_not_return_a_phantom_message_id() {
    let root = root();
    let state = root.path().join("state");
    let running = Running::start(&state);
    running.inbox("alice");
    running.inbox("bob");
    let conn = rusqlite::Connection::open(state.join("messages.sqlite3")).unwrap();
    conn.execute(
        "INSERT INTO agents(name,provider,session,endpoint,enabled) VALUES(?1,'opencode',?2,?3,1)",
        rusqlite::params![
            "opencode-ses_abcdefghijklmnopqrstuvwx12",
            "ses_abcdefghijklmnopqrstuvwx12",
            "/tmp/unused.sock"
        ],
    )
    .unwrap();
    let mut mcp = Mcp::start(&state);
    mcp.initialize().await;
    let rejected = mcp.call("send_message", json!({"from":"alice","to":"opencode-ses_abcdefghijklmnopqrstuvwx12","body":"rejected"})).await;
    assert!(failed(&rejected), "{rejected}");
    let error = &rejected["result"]["structuredContent"];
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("OpenCode live lease required"),
        "{error}"
    );
    assert!(
        error.get("id").is_none(),
        "a rejected send must not claim a message id: {error}"
    );
    assert!(
        error["request_id"].is_string(),
        "retain a safe retry correlation id: {error}"
    );
    assert_eq!(error["persisted"], false);
    let missing = mcp
        .call("get_message", json!({"id":error["request_id"]}))
        .await;
    assert!(failed(&missing));
    let retried = mcp.call("send_message", json!({"from":"alice","to":"opencode-ses_abcdefghijklmnopqrstuvwx12","body":"rejected","id":error["request_id"]})).await;
    assert!(failed(&retried));
    let retry_error = &retried["result"]["structuredContent"];
    assert_eq!(retry_error["request_id"], error["request_id"]);
    assert_eq!(retry_error["persisted"], false);
    let mut second = Mcp::start(&state);
    second.initialize().await;
    let args =
        json!({"from":"alice","to":"opencode-ses_abcdefghijklmnopqrstuvwx12","body":"concurrent"});
    let (left, right) = tokio::join!(
        mcp.call("send_message", args.clone()),
        second.call("send_message", args)
    );
    for response in [&left, &right] {
        assert!(failed(response));
        let error = &response["result"]["structuredContent"];
        assert_eq!(error["persisted"], false);
        assert!(error.get("id").is_none());
        assert!(error["request_id"].is_string());
    }
    assert_ne!(
        left["result"]["structuredContent"]["request_id"],
        right["result"]["structuredContent"]["request_id"]
    );
    second.close().await;
    let accepted = mcp
        .call(
            "send_message",
            json!({"from":"alice","to":"bob","body":"accepted"}),
        )
        .await;
    let id = structured(&accepted)["id"].as_str().unwrap();
    let readback = mcp.call("get_message", json!({"id":id})).await;
    assert_eq!(structured(&readback)["result"]["body"], "accepted");
    let conflict = mcp
        .call(
            "send_message",
            json!({"from":"alice","to":"bob","body":"different","id":id}),
        )
        .await;
    assert!(failed(&conflict));
    let conflict_error = &conflict["result"]["structuredContent"];
    assert_eq!(conflict_error["request_id"], id);
    assert_eq!(conflict_error["persistence"], "unknown");
    assert!(conflict_error.get("persisted").is_none());
    mcp.close().await;
}

#[tokio::test]
async fn lost_broker_response_keeps_send_persistence_unknown() {
    let root = root();
    let state = root.path().join("state");
    std::fs::create_dir(&state).unwrap();
    let listener = UnixListener::bind(state.join("bridge.sock")).unwrap();
    let broker = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        reader.read_line(&mut request).await.unwrap();
        assert!(request.contains("\"send\""));
        // Simulate a response lost after the broker received the request.
    });
    let mut mcp = Mcp::start(&state);
    mcp.initialize().await;
    let result = mcp
        .call(
            "send_message",
            json!({"from":"alice","to":"bob","body":"uncertain"}),
        )
        .await;
    assert!(failed(&result));
    let error = &result["result"]["structuredContent"];
    assert!(error.get("id").is_none(), "{error}");
    assert!(error["request_id"].is_string(), "{error}");
    assert_eq!(error["persistence"], "unknown");
    assert!(error.get("persisted").is_none(), "{error}");
    broker.await.unwrap();
    mcp.close().await;
}

#[tokio::test]
async fn unclassified_broker_rejection_does_not_claim_no_persistence() {
    let root = root();
    let state = root.path().join("state");
    std::fs::create_dir(&state).unwrap();
    let listener = UnixListener::bind(state.join("bridge.sock")).unwrap();
    let broker = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        reader.read_line(&mut request).await.unwrap();
        reader
            .get_mut()
            .write_all(b"{\"ok\":false,\"error\":\"unknown failure\"}\n")
            .await
            .unwrap();
    });
    let mut mcp = Mcp::start(&state);
    mcp.initialize().await;
    let result = mcp
        .call(
            "send_message",
            json!({"from":"alice","to":"bob","body":"uncertain"}),
        )
        .await;
    assert!(failed(&result));
    let error = &result["result"]["structuredContent"];
    assert_eq!(error["persistence"], "unknown");
    assert!(error.get("persisted").is_none());
    assert!(error.get("id").is_none());
    assert!(error["request_id"].is_string());
    broker.await.unwrap();
    mcp.close().await;
}

#[tokio::test]
async fn normal_send_continues_without_recipient_calls_or_status_exchange() {
    let root = root();
    let state = root.path().join("state");
    let running = Running::start(&state);
    running.inbox("alice");
    running.inbox("bob");
    let mut sender = Mcp::start(&state);
    sender.initialize().await;
    let sent = sender
        .call(
            "send_message",
            json!({"from":"alice","to":"bob","body":"handoff"}),
        )
        .await;
    assert!(!failed(&sent));
    let id = structured(&sent)["result"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // The sender proceeds with unrelated work; no recipient MCP process or
    // status request participates in accepting the message.
    assert!(!failed(&sender.call("list_agents", json!({})).await));
    let stored = rusqlite::Connection::open_with_flags(
        state.join("messages.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let count: i64 = stored
        .query_row(
            "SELECT count(*) FROM messages WHERE id=? AND sender='alice' AND recipient='bob' AND body='handoff'",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    sender.close().await;
}

#[tokio::test]
async fn one_stdio_process_discovers_tools_and_routes_messages_with_safe_retries() {
    let root = root();
    let state = root.path().join("state");
    let running = Running::start(&state);
    running.inbox("alice");
    running.inbox("bob");
    let mut mcp = Mcp::start(&state);
    let pid = mcp.child.id();
    mcp.initialize().await;
    let tools = mcp.request("tools/list", json!({})).await;
    let tools = tools["result"]["tools"].as_array().unwrap();
    for name in [
        "connection_status",
        "send_message",
        "connect_peer",
        "confirm_pairing",
        "share_session",
    ] {
        assert!(tools.iter().any(|tool| tool["name"] == name), "{name}");
    }
    assert!(!tools.iter().any(|tool| tool["name"] == "rpc"));
    assert!(
        !tools
            .iter()
            .any(|tool| tool["name"] == "acknowledge_message")
    );
    let tool = tools
        .iter()
        .find(|tool| tool["name"] == "send_message")
        .unwrap();
    let description = tool["description"].as_str().unwrap();
    assert!(description.contains("connection_status"));
    assert!(description.contains("then continue your work"));
    assert!(!description.to_lowercase().contains("acknowledg"));
    assert!(!description.to_lowercase().contains("poll"));
    assert!(!description.contains("get_message"));
    assert!(description.contains("retry the same request"));
    let diagnostic = tools
        .iter()
        .find(|tool| tool["name"] == "get_message")
        .unwrap()["description"]
        .as_str()
        .unwrap();
    assert!(diagnostic.contains("Diagnose a failed send or nonresponse"));
    for (field, source) in [("to", "Reply to"), ("parent", "Message reference")] {
        assert!(description.contains(source));
        assert!(
            tool["inputSchema"]["properties"][field]["description"]
                .as_str()
                .unwrap()
                .contains(source),
            "send_message.{field} must explain its incoming routing field"
        );
    }
    assert_eq!(
        tools
            .iter()
            .find(|tool| tool["name"] == "connection_status")
            .unwrap()["annotations"]["readOnlyHint"],
        true
    );
    assert_ne!(
        tools
            .iter()
            .find(|tool| tool["name"] == "send_message")
            .unwrap()["annotations"]["readOnlyHint"],
        true
    );
    assert!(!failed(&mcp.call("connection_status", json!({})).await));
    let stale = mcp.call("list_agents", json!({"include_stale":true})).await;
    assert!(!failed(&stale));
    let agents = mcp.call("list_agents", json!({})).await;
    assert!(
        structured(&agents)["result"]
            .as_array()
            .unwrap()
            .iter()
            .any(|agent| agent["name"] == "alice")
    );
    let body = "MCP test: quotes ' \" and newline\n$(this-is-data)";
    let sent = mcp
        .call(
            "send_message",
            json!({"from":"alice","to":"bob","body":body}),
        )
        .await;
    let id = structured(&sent)["id"].as_str().unwrap().to_owned();
    uuid::Uuid::parse_str(&id).unwrap();
    assert_eq!(structured(&sent)["result"]["id"], id);
    let repeated = mcp
        .call(
            "send_message",
            json!({"from":"alice","to":"bob","body":body,"id":id}),
        )
        .await;
    assert_eq!(structured(&repeated)["result"]["id"], id);
    let history = mcp
        .call("list_messages", json!({"agent":"bob","limit":10}))
        .await;
    let rows = structured(&history)["result"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["body"], body);
    assert_eq!(mcp.child.id(), pid);
    assert_eq!(running.request(Request::PeersList), json!([]));
    running.request(Request::Unregister {
        name: "alice".into(),
        session: "inbox-alice".into(),
    });
    let available = mcp.call("list_agents", json!({})).await;
    assert!(
        !structured(&available)["result"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == "alice")
    );
    let all = mcp.call("list_agents", json!({"include_stale":true})).await;
    let data = structured(&all);
    let row = data["result"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "alice")
        .unwrap();
    assert_eq!(row["stale_reason"], "disabled");
    assert_eq!(row["enabled"], false);
    mcp.close().await;
}

#[tokio::test]
async fn strict_tools_reject_unknown_fields_and_cannot_supply_owner_approval() {
    let root = root();
    let state = root.path().join("state");
    let running = Running::start(&state);
    running.inbox("alice");
    let mut mcp = Mcp::start(&state);
    mcp.initialize().await;
    let bad = mcp
        .call("list_agents", json!({"remote":false,"op":"peer_pair"}))
        .await;
    assert!(failed(&bad), "{bad}");
    let unknown = mcp.request("agent_bridge/raw_rpc", json!({})).await;
    assert!(failed(&unknown), "{unknown}");
    let false_confirmation = mcp
        .call(
            "confirm_pairing",
            json!({"id":uuid::Uuid::new_v4().to_string(),"code":"1234-5678-9012","owner_confirmed":false}),
        )
        .await;
    assert!(failed(&false_confirmation), "{false_confirmation}");
    let absent_confirmation = mcp
        .call(
            "confirm_pairing",
            json!({"id":uuid::Uuid::new_v4().to_string(),"code":"1234-5678-9012"}),
        )
        .await;
    assert!(failed(&absent_confirmation), "{absent_confirmation}");
    running.request(Request::NetworkConfigure {
        enabled: true,
        listen: Some("/ip4/127.0.0.1/udp/0/quic-v1".into()),
    });
    let inbox_watch = mcp
        .call("open_pairing", json!({"agent":"alice","seconds":120}))
        .await;
    assert!(failed(&inbox_watch), "{inbox_watch}");
    assert_eq!(running.request(Request::LanPairingList)["open"], false);
    assert_eq!(running.request(Request::PeersList), json!([]));
    // A malformed tool call does not kill the persistent server.
    assert!(!failed(&mcp.call("connection_status", json!({})).await));
    mcp.close().await;
}

#[tokio::test]
async fn list_agents_defaults_to_local_and_requires_one_explicit_trusted_remote_peer() {
    let root = root();
    let state = root.path().join("state");
    let running = Running::start(&state);
    running.inbox("local-agent");
    let mut peers = Vec::new();
    for alias in ["workstation", "other-machine"] {
        let peer = libp2p_identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id()
            .to_string();
        running.request(Request::PeerPair {
            peer_id: peer.clone(),
            fingerprint: peer.clone(),
            alias: Some(alias.into()),
        });
        // Seed the permitted directory cache, as the network worker does after
        // fetching a paired peer's explicitly exported sessions.
        Db::open(&state.join("messages.sqlite3"))
            .unwrap()
            .replace_remote_agents(
                &peer,
                &[RemoteAgent {
                    name: format!("{alias}-agent"),
                    provider: Provider::Inbox,
                    session: format!("{alias}-session"),
                }],
            )
            .unwrap();
        peers.push(peer);
    }
    let mut mcp = Mcp::start(&state);
    mcp.initialize().await;
    let tools = mcp.request("tools/list", json!({})).await;
    let list = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "list_agents")
        .unwrap();
    assert!(list["inputSchema"]["properties"].get("peer").is_some());
    assert!(
        list["inputSchema"]["properties"]
            .get("include_stale")
            .is_some()
    );
    assert!(failed(
        &mcp.call(
            "list_agents",
            json!({"peer":"workstation", "include_stale":true})
        )
        .await
    ));
    assert!(list["inputSchema"]["properties"].get("remote").is_none());
    for arguments in [json!({}), json!({"peer":null})] {
        let response = mcp.call("list_agents", arguments).await;
        let agents = structured(&response)["result"].as_array().unwrap();
        assert!(agents.iter().any(|agent| agent["name"] == "local-agent"));
        assert!(agents.iter().all(|agent| agent["peer_alias"].is_null()));
        assert!(
            !agents
                .iter()
                .any(|agent| agent["name"] == "workstation-agent")
        );
    }
    for peer in ["workstation", peers[0].as_str()] {
        let response = mcp.call("list_agents", json!({"peer":peer})).await;
        let agents = structured(&response)["result"].as_array().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["name"], "workstation-agent");
        assert_eq!(agents[0]["peer_id"], peers[0]);
        assert_eq!(agents[0]["online"], false);
    }
    let other = mcp
        .call("list_agents", json!({"peer":"other-machine"}))
        .await;
    assert_eq!(
        structured(&other)["result"][0]["name"],
        "other-machine-agent"
    );
    for arguments in [
        json!({"remote":true}),
        json!({"remote":false}),
        json!({"peer":""}),
        json!({"peer":"  "}),
        json!({"peer":"*"}),
        json!({"peer":"missing-peer"}),
        json!({"peer":42}),
    ] {
        let response = mcp.call("list_agents", arguments).await;
        assert!(failed(&response), "{response}");
        assert!(!response.to_string().contains("workstation-agent"));
        assert!(!response.to_string().contains("other-machine-agent"));
    }
    running.request(Request::PeerRevoke {
        peer: "workstation".into(),
    });
    for peer in ["workstation", peers[0].as_str()] {
        let response = mcp.call("list_agents", json!({"peer":peer})).await;
        assert!(failed(&response), "{response}");
        assert!(!response.to_string().contains("workstation-agent"));
    }
    // A rejected or revoked selection cannot poison another peer's cache.
    let other = mcp
        .call("list_agents", json!({"peer":"other-machine"}))
        .await;
    assert_eq!(
        structured(&other)["result"][0]["name"],
        "other-machine-agent"
    );
    mcp.close().await;
}

#[tokio::test]
async fn failed_send_returns_a_stable_retry_id_without_leaking_message_in_errors() {
    let root = root();
    let state = root.path().join("no-broker");
    let mut mcp = Mcp::start(&state);
    mcp.initialize().await;
    let body = "PRIVATE_MCP_ERROR_CANARY";
    let result = mcp
        .call(
            "send_message",
            json!({"from":"alice","to":"bob","body":body}),
        )
        .await;
    assert!(failed(&result));
    let id = result["result"]["structuredContent"]["request_id"]
        .as_str()
        .unwrap();
    uuid::Uuid::parse_str(id).unwrap();
    assert_eq!(
        result["result"]["structuredContent"]["persistence"],
        "unknown"
    );
    assert!(!result.to_string().contains(body));
    let retry = mcp
        .call(
            "send_message",
            json!({"from":"alice","to":"bob","body":body,"id":id}),
        )
        .await;
    assert_eq!(retry["result"]["structuredContent"]["request_id"], id);
    assert!(failed(&retry));
    mcp.close().await;
}

#[tokio::test]
async fn pipelined_calls_keep_request_ids_and_only_emit_json_rpc() {
    let root = root();
    let state = root.path().join("state");
    let _running = Running::start(&state);
    let mut mcp = Mcp::start(&state);
    mcp.initialize().await;
    for id in [101, 102] {
        mcp.write(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"connection_status","arguments":{}}}))
            .await;
    }
    let mut results = BTreeMap::new();
    for _ in 0..2 {
        let response = mcp.read().await;
        assert!(!failed(&response), "{response}");
        results.insert(response["id"].as_u64().unwrap(), response);
    }
    assert_eq!(results.keys().copied().collect::<Vec<_>>(), [101, 102]);
    mcp.close().await;
}

#[tokio::test]
async fn oversized_input_is_discarded_and_clean_eof_stops_the_server() {
    let root = root();
    let state = root.path().join("state");
    let _running = Running::start(&state);
    let mut mcp = Mcp::start(&state);
    mcp.initialize().await;
    // The SDK must reject the complete line before dispatch, then recover its
    // newline framing so the next bounded request still works.
    mcp.write(&json!({"jsonrpc":"2.0","id":999,"method":"tools/call","params":{"name":"send_message","arguments":{"from":"missing","to":"user","body":"x".repeat(MAX_FRAME + 1024)}}}))
        .await;
    assert!(!failed(&mcp.call("connection_status", json!({})).await));
    mcp.close().await;
}

#[tokio::test]
async fn modern_discovery_and_per_request_metadata_work_without_initialize() {
    let root = root();
    let state = root.path().join("state");
    let _running = Running::start(&state);
    let mut mcp = Mcp::start(&state);
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientInfo":{"name":"agent-bridge-modern-test","version":"1"},
        "io.modelcontextprotocol/clientCapabilities":{}
    });
    let discovered = mcp.request("server/discover", json!({"_meta":meta})).await;
    assert!(!failed(&discovered), "{discovered}");
    assert!(
        discovered["result"]["supportedVersions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|version| version == "2026-07-28")
    );
    let tools = mcp.request("tools/list", json!({"_meta":meta})).await;
    assert!(tools["result"]["tools"].as_array().is_some());
    let status = mcp
        .request(
            "tools/call",
            json!({"name":"connection_status","arguments":{},"_meta":meta}),
        )
        .await;
    assert!(!failed(&status), "{status}");
    mcp.close().await;
}

#[tokio::test]
async fn two_mcp_clients_pair_real_brokers_only_after_both_owners_approve_then_share_one_session() {
    let root = root();
    let a_state = root.path().join("a");
    let b_state = root.path().join("b");
    let a = Running::start(&a_state);
    let b = Running::start(&b_state);
    let a_native = Native::start(&a, &root.path().join("a-native"), "a-owner");
    let b_native = Native::start(&b, &root.path().join("b-native"), "b-owner");
    a.inbox("private-existing-session");
    let mut a_mcp = Mcp::start(&a_state);
    let mut b_mcp = Mcp::start(&b_state);
    a_mcp.initialize().await;
    b_mcp.initialize().await;
    for client in [&mut a_mcp, &mut b_mcp] {
        let configured = client
            .call(
                "configure_network",
                json!({"enabled":true,"lan":false,"listen":"/ip4/127.0.0.1/udp/0/quic-v1"}),
            )
            .await;
        assert!(!failed(&configured), "{configured}");
    }
    let a_status = a_mcp.listening().await;
    let a_peer = a_status["machine"]["peer_id"].as_str().unwrap().to_owned();
    let b_status = b_mcp.listening().await;
    let b_peer = b_status["machine"]["peer_id"].as_str().unwrap().to_owned();
    let b_address = b_status["network"]["listening"][0].as_str().unwrap();
    let opened = b_mcp
        .call("open_pairing", json!({"agent":b_native.name,"seconds":120}))
        .await;
    assert!(!failed(&opened), "{opened}");
    let connected = a_mcp
        .call(
            "connect_peer",
            json!({"peer":b_peer,"address":b_address,"agent":a_native.name,"seconds":120}),
        )
        .await;
    let id = structured(&connected)["result"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let a_view = a_mcp.pairing_state(&id, "compare_code").await;
    let b_view = b_mcp.pairing_state(&id, "compare_code").await;
    assert_eq!(a_view["verification_code"], b_view["verification_code"]);
    let code = a_view["verification_code"].as_str().unwrap();
    let denied = a_mcp
        .call(
            "confirm_pairing",
            json!({"id":id,"code":code,"owner_confirmed":false}),
        )
        .await;
    assert!(failed(&denied), "{denied}");
    assert_eq!(a.request(Request::PeersList), json!([]));
    assert_eq!(b.request(Request::PeersList), json!([]));
    let a_confirmed = a_mcp
        .call(
            "confirm_pairing",
            json!({"id":id,"code":code,"owner_confirmed":true}),
        )
        .await;
    assert!(!failed(&a_confirmed), "{a_confirmed}");
    a_mcp.pairing_state(&id, "waiting_peer_confirmation").await;
    assert_eq!(a.request(Request::PeersList), json!([]));
    assert_eq!(b.request(Request::PeersList), json!([]));
    let b_confirmed = b_mcp
        .call(
            "confirm_pairing",
            json!({"id":id,"code":code,"owner_confirmed":true}),
        )
        .await;
    assert!(!failed(&b_confirmed), "{b_confirmed}");
    a_mcp.pairing_state(&id, "paired").await;
    b_mcp.pairing_state(&id, "paired").await;
    assert_eq!(a.request(Request::PeersList)[0]["grants"], json!([]));
    assert_eq!(b.request(Request::PeersList)[0]["grants"], json!([]));
    let private = b_mcp.call("list_agents", json!({"peer":a_peer})).await;
    assert_eq!(structured(&private)["result"], json!([]));
    let shared = a_mcp
        .call(
            "share_session",
            json!({"peer":b_peer,"agent":a_native.name}),
        )
        .await;
    assert!(!failed(&shared), "{shared}");
    let a_peers = a.request(Request::PeersList);
    let grants = a_peers[0]["grants"].as_array().unwrap();
    assert_eq!(grants.len(), 3);
    assert!(
        grants
            .iter()
            .all(|grant| grant["session"] == a_native.session)
    );
    assert_eq!(b.request(Request::PeersList)[0]["grants"], json!([]));
    a.inbox("private-future-session");
    let exported = Db::open(&a_state.join("messages.sqlite3"))
        .unwrap()
        .export_agents(&b_peer)
        .unwrap();
    assert_eq!(exported.len(), 1);
    assert_eq!(exported[0].name, a_native.name);
    assert_eq!(exported[0].session, a_native.session);
    timeout(Duration::from_secs(30), async {
        loop {
            let response = b_mcp.call("list_agents", json!({"peer":a_peer})).await;
            let agents = structured(&response)["result"].as_array().unwrap();
            if !agents.is_empty() {
                assert_eq!(agents.len(), 1, "other sessions must remain private");
                assert_eq!(agents[0]["name"], a_native.name);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("shared session never appeared in the permitted remote directory");
    let unshared = a_mcp
        .call(
            "unshare_session",
            json!({"peer":b_peer,"agent":a_native.name}),
        )
        .await;
    assert!(!failed(&unshared), "{unshared}");
    assert_eq!(a.request(Request::PeersList)[0]["grants"], json!([]));
    assert!(
        Db::open(&a_state.join("messages.sqlite3"))
            .unwrap()
            .export_agents(&b_peer)
            .unwrap()
            .is_empty()
    );
    timeout(Duration::from_secs(30), async {
        loop {
            let response = b_mcp.call("list_agents", json!({"peer":a_peer})).await;
            if structured(&response)["result"] == json!([]) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("unshared session remained in the permitted remote directory");
    a_mcp.close().await;
    b_mcp.close().await;
}

#[tokio::test]
async fn event_subscription_compact_results_keep_effective_set_and_atomic_snapshot() {
    let root = root();
    let state = root.path().join("state");
    let running = Running::start(&state);
    let coordinator = Native::start(&running, &root.path().join("coordinator"), "coordinator");
    let worker = Native::start(&running, &root.path().join("worker"), "worker");
    let mut mcp = Mcp::start(&state);
    mcp.initialize().await;
    let events = json!([{"event":"agent.opened"},{"event":"agent.closed"}]);
    let response = mcp
        .call(
            "subscribe",
            json!({"agent":coordinator.name,"events":events}),
        )
        .await;
    let receipt = &structured(&response)["result"];
    assert_eq!(receipt["events"], events);
    assert_eq!(receipt["agent"], coordinator.name);
    assert_eq!(receipt["persistent"], false);
    assert_eq!(
        receipt["recovery"],
        json!({"total":0,"truncated":false,"items":[]})
    );
    let available = receipt["agents"].as_array().unwrap();
    let known = available
        .iter()
        .find(|agent| agent["session"] == worker.session)
        .unwrap();
    assert_eq!(known["provider"], "claude");
    assert_eq!(known["route"], worker.name);
    assert!(known["display_name"].is_string());
    assert!(
        available
            .iter()
            .all(|agent| agent.get("endpoint").is_none())
    );
    let added = mcp
        .call(
            "subscribe",
            json!({"agent":coordinator.name,"events":[{"event":"board.changed","board":"main"}]}),
        )
        .await;
    assert_eq!(
        structured(&added)["result"]["events"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    // Retain the snapshot even when adding a board selector to an existing lifecycle watch.
    assert!(
        structured(&added)["result"]["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|agent| agent["session"] == worker.session)
    );
    let removed = mcp
        .call(
            "unsubscribe",
            json!({"agent":coordinator.name,"events":[{"event":"board.changed","board":"main"}]}),
        )
        .await;
    assert_eq!(structured(&removed)["result"]["events"], events);
    let empty = mcp
        .call("unsubscribe", json!({"agent":coordinator.name,"events":[]}))
        .await;
    assert!(failed(&empty));
    let cleared = mcp
        .call("unsubscribe", json!({"agent":coordinator.name}))
        .await;
    assert_eq!(structured(&cleared)["result"]["events"], json!([]));
    assert_eq!(
        structured(&cleared)["result"]["recovery"],
        json!({"total":0,"truncated":false,"items":[]})
    );
    mcp.close().await;
}

#[tokio::test]
async fn board_subscriptions_deliver_debounced_native_notices_only_when_opted_in() {
    let root = root();
    let state = root.path().join("state");
    let running = Running::start(&state);
    let mut native = Native::start(&running, &root.path().join("native"), "board-owner");
    running.inbox("not-native");
    let mut mcp = Mcp::start(&state);
    mcp.initialize().await;
    let tools = mcp.request("tools/list", json!({})).await;
    for name in ["subscribe", "unsubscribe"] {
        assert!(
            tools["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == name)
        );
    }
    for old in ["subscribe_board", "unsubscribe_board"] {
        assert!(
            !tools["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == old)
        );
        let response = mcp.call(old, json!({"agent":native.name})).await;
        assert!(failed(&response), "old tool accepted: {response}");
    }
    for arguments in [
        json!({}),
        json!({"agent":native.name}),
        json!({"agent":"missing","events":[{"event":"agent.opened"}]}),
        json!({"agent":native.name,"events":[]}),
        json!({"agent":native.name,"events":[{"event":"unknown"}]}),
        json!({"agent":native.name,"events":[{"event":"board.changed"}]}),
        json!({"agent":native.name,"events":[{"event":"board.changed","board":"missing"}]}),
        json!({"agent":native.name,"events":[{"event":"agent.opened","board":"main"}]}),
        json!({"agent":native.name,"events":[{"event":"board.changed","board":"main","extra":true}]}),
        json!({"agent":native.name,"peer":"remote","events":[{"event":"agent.opened"}]}),
    ] {
        let response = mcp.call("subscribe", arguments).await;
        assert!(failed(&response), "{response}");
    }
    let snapshot = mcp.call("read_board", json!({})).await;
    let revision = structured(&snapshot)["result"]["revision"].clone();
    let initialized = mcp
        .call(
            "update_board",
            json!({"expected_revision":revision,"edits":[{"op":"set","markdown":"# Board\n"}]}),
        )
        .await;
    assert!(!failed(&initialized), "{initialized}");
    let revision = structured(&initialized)["result"]["revision"].clone();
    for _ in 0..2 {
        let subscribed = mcp
            .call(
                "subscribe",
                json!({"agent":native.name,"events":[{"event":"board.changed","board":"main"}]}),
            )
            .await;
        assert!(!failed(&subscribed), "{subscribed}");
    }
    // No historical notification on subscribe and no wakeup from this session's
    // own attributed edits. This also checks that the polling worker does not
    // reinterpret a known own-write as an external file edit.
    let own = mcp.call("update_board", json!({"agent":native.name,"expected_revision":revision,"edits":[{"op":"append","text":"Own update\n"}]})).await;
    assert!(!failed(&own), "{own}");
    assert!(
        timeout(Duration::from_millis(3700), native.received.recv())
            .await
            .is_err()
    );
    let revision = structured(&own)["result"]["revision"].clone();
    // Invalid attribution must not apply an otherwise valid edit.
    let invalid = mcp.call("update_board", json!({"agent":"missing","expected_revision":revision,"edits":[{"op":"append","text":"must not apply"}]})).await;
    assert!(failed(&invalid), "{invalid}");
    let unchanged = mcp.call("read_board", json!({})).await;
    assert_eq!(structured(&unchanged)["result"]["revision"], revision);
    let first = mcp.call("update_board", json!({"expected_revision":revision,"edits":[{"op":"append","text":"Unattributed first\n"}]})).await;
    assert!(!failed(&first), "{first}");
    let revision = structured(&first)["result"]["revision"].clone();
    let second = mcp.call("update_board", json!({"expected_revision":revision,"edits":[{"op":"append","text":"PRIVATE_MARKDOWN_NEVER_IN_NOTICE\n"}]})).await;
    assert!(!failed(&second), "{second}");
    let revision = structured(&second)["result"]["revision"].clone();
    // A subsequent own-write must neither erase pending external work nor
    // extend its debounce. The notice points at the newest readable revision.
    let own = mcp.call("update_board", json!({"agent":native.name,"expected_revision":revision,"edits":[{"op":"append","text":"Own follow-up\n"}]})).await;
    assert!(!failed(&own), "{own}");
    let revision = structured(&own)["result"]["revision"].as_str().unwrap();
    assert!(
        timeout(Duration::from_millis(2200), native.received.recv())
            .await
            .is_err()
    );
    let notice = timeout(Duration::from_secs(4), native.received.recv())
        .await
        .unwrap()
        .unwrap();
    let content = notice["message"]["content"].as_str().unwrap();
    assert!(content.contains("board"), "{notice}");
    assert!(content.contains(revision), "{notice}");
    assert!(content.contains("not user approval"), "{notice}");
    assert!(!content.contains("PRIVATE_MARKDOWN_NEVER_IN_NOTICE"));
    assert_eq!(notice["session_id"], native.session);
    assert_eq!(
        running.request(Request::History {
            agent: None,
            limit: 100
        }),
        json!([])
    );

    // An actual direct file change is detected even without a broker edit.
    let current = mcp.call("read_board", json!({})).await;
    let current = &structured(&current)["result"];
    std::fs::write(
        agent_bridge::board::read(&state).unwrap().path,
        format!("{}External editor\n", current["markdown"].as_str().unwrap()),
    )
    .unwrap();
    let external = timeout(Duration::from_secs(6), native.received.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        external["message"]["content"]
            .as_str()
            .unwrap()
            .contains("not user approval")
    );
    assert!(
        native.received.try_recv().is_err(),
        "one notice per coalesced burst"
    );

    let current = mcp.call("read_board", json!({})).await;
    let revision = structured(&current)["result"]["revision"].clone();
    let pending = mcp.call("update_board", json!({"expected_revision":revision,"edits":[{"op":"append","text":"Cancelled pending notification\n"}]})).await;
    assert!(!failed(&pending), "{pending}");
    let unsubscribed = mcp.call("unsubscribe", json!({"agent":native.name})).await;
    assert!(!failed(&unsubscribed), "{unsubscribed}");
    assert!(
        timeout(Duration::from_millis(3700), native.received.recv())
            .await
            .is_err()
    );

    let subscribed = mcp
        .call(
            "subscribe",
            json!({"agent":native.name,"events":[{"event":"board.changed","board":"main"}]}),
        )
        .await;
    assert!(!failed(&subscribed), "{subscribed}");
    drop(running);
    support::wait_for_shutdown(&state).await;
    let _restarted = Running::start(&state);
    let current = mcp.call("read_board", json!({})).await;
    let revision = structured(&current)["result"]["revision"].clone();
    let after_restart = mcp.call("update_board", json!({"expected_revision":revision,"edits":[{"op":"append","text":"Private after restart\n"}]})).await;
    assert!(!failed(&after_restart), "{after_restart}");
    assert!(
        timeout(Duration::from_millis(3700), native.received.recv())
            .await
            .is_err()
    );
    mcp.close().await;
}

#[tokio::test]
async fn shared_markdown_board_updates_atomically_and_preserves_concurrent_agent_work() {
    let root = root();
    let state = root.path().join("state");
    let _running = Running::start(&state);
    let mut first = Mcp::start(&state);
    let mut second = Mcp::start(&state);
    first.initialize().await;
    second.initialize().await;

    let empty = first.call("read_board", json!({})).await;
    let empty = &structured(&empty)["result"];
    assert_eq!(empty["markdown"], "");
    assert_eq!(empty["exists"], false);
    let markdown = "# Anything the user chooses\n\n| Writer | Work | Needs owner |\n| --- | --- | --- |\n| A | alpha | no |\n| B | beta | no |\n";
    let initial = first
        .call(
            "update_board",
            json!({
                "expected_revision":empty["revision"],
                "edits":[{"op":"set","markdown":markdown}]
            }),
        )
        .await;
    assert!(!failed(&initial), "{initial}");
    let revision = structured(&initial)["result"]["revision"].clone();
    let a = first.call("update_board", json!({
        "expected_revision":revision,
        "edits":[{"op":"replace","old":"| A | alpha | no |","new":"| A | complete ✓ | no |"}]
    })).await;
    assert!(!failed(&a), "{a}");

    // Both agents read the same revision. The second edit cannot overwrite the
    // first one's change; the conflict supplies a fresh snapshot for its retry.
    let b = second
        .call(
            "update_board",
            json!({
                "expected_revision":revision,
                "edits":[{"op":"replace","old":"| B | beta | no |","new":"| B | reviewing | yes |"}]
            }),
        )
        .await;
    assert!(failed(&b), "{b}");
    let conflict = &b["result"]["structuredContent"]["result"];
    assert_eq!(conflict["applied"], false);
    assert_eq!(conflict["conflict"], true);
    assert!(
        conflict["board"]["markdown"]
            .as_str()
            .unwrap()
            .contains("complete ✓")
    );
    let merged = second
        .call(
            "update_board",
            json!({
                "expected_revision":conflict["board"]["revision"],
                "verbose":true,
                "edits":[{"op":"replace","old":"| B | beta | no |","new":"| B | reviewing | yes |"}]
            }),
        )
        .await;
    assert!(!failed(&merged), "{merged}");
    let snapshot = &structured(&merged)["result"]["board"];
    let contents = snapshot["markdown"].as_str().unwrap();
    assert!(contents.contains("complete ✓"));
    assert!(contents.contains("reviewing | yes"));
    assert_eq!(
        std::fs::read_to_string(agent_bridge::board::read(&state).unwrap().path).unwrap(),
        contents
    );

    let invalid_batch = first
        .call(
            "update_board",
            json!({
                "expected_revision":snapshot["revision"],
                "edits":[{"op":"append","text":"must not appear"},
                    {"op":"replace","old":"missing anchor","new":"anything"}]
            }),
        )
        .await;
    assert!(failed(&invalid_batch));
    let unchanged = first.call("read_board", json!({})).await;
    assert_eq!(
        structured(&unchanged)["result"]["revision"],
        snapshot["revision"]
    );
    assert_eq!(structured(&unchanged)["result"]["markdown"], contents);
    assert!(failed(
        &first
            .call(
                "update_board",
                json!({
                    "expected_revision":snapshot["revision"],"edits":[],"path":"/unrelated/file"
                })
            )
            .await
    ));

    let raw = tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .arg("--state-dir")
        .arg(&state)
        .args(["board", "--raw"])
        .output()
        .await
        .unwrap();
    assert!(
        raw.status.success(),
        "{}",
        String::from_utf8_lossy(&raw.stderr)
    );
    assert_eq!(String::from_utf8(raw.stdout).unwrap(), contents);
    // Race two independent MCP processes against one revision. Exactly one
    // write commits, then the losing caller can merge against the fresh board.
    let (left, right) = tokio::join!(
        first.call(
            "update_board",
            json!({"expected_revision":snapshot["revision"],
            "edits":[{"op":"append","text":"\nleft writer\n"}]})
        ),
        second.call(
            "update_board",
            json!({"expected_revision":snapshot["revision"],
            "edits":[{"op":"append","text":"\nright writer\n"}]})
        )
    );
    assert_ne!(failed(&left), failed(&right));
    let (conflict, missing) = if failed(&left) {
        (&left, "\nleft writer\n")
    } else {
        (&right, "\nright writer\n")
    };
    let current = &conflict["result"]["structuredContent"]["result"]["board"];
    let retry = first
        .call(
            "update_board",
            json!({"expected_revision":current["revision"],"verbose":true,
        "edits":[{"op":"append","text":missing}]}),
        )
        .await;
    assert!(!failed(&retry), "{retry}");
    let merged = structured(&retry)["result"]["board"]["markdown"]
        .as_str()
        .unwrap();
    assert!(merged.contains("left writer"));
    assert!(merged.contains("right writer"));
    assert!(merged.contains("complete ✓"));
    assert!(merged.contains("reviewing | yes"));
    first.close().await;
    second.close().await;
}

#[tokio::test]
async fn compact_writes_stay_small_and_verbose_preserves_full_records() {
    let root = root();
    let state = root.path().join("state");
    let running = Running::start(&state);
    running.inbox("alice");
    running.inbox("bob");
    let mut mcp = Mcp::start(&state);
    mcp.initialize().await;
    let mut send_sizes = Vec::new();
    let mut board_sizes = Vec::new();
    let board = mcp.call("read_board", json!({})).await;
    let mut revision = structured(&board)["result"]["revision"].clone();
    for length in [100, 50_000] {
        let body = "x".repeat(length);
        let sent = mcp
            .call(
                "send_message",
                json!({"from":"alice","to":"bob","body":body}),
            )
            .await;
        let receipt = &structured(&sent)["result"];
        assert_eq!(receipt.as_object().unwrap().len(), 2);
        assert!(receipt["state"].is_string());
        send_sizes.push(sent["result"].to_string().len());
        assert!(sent.to_string().len() < 600);
        let id = receipt["id"].as_str().unwrap();
        let verbose = mcp
            .call(
                "send_message",
                json!({"from":"alice","to":"bob","body":body,"id":id,"verbose":true}),
            )
            .await;
        assert_eq!(structured(&verbose)["result"]["body"], body);
        // The compact receipt did not truncate or alter the persisted message.
        let stored = mcp.call("get_message", json!({"id":id})).await;
        assert_eq!(structured(&stored)["result"]["body"], body);
        let updated = mcp
            .call(
                "update_board",
                json!({"expected_revision":revision,"edits":[{"op":"set","markdown":body}]}),
            )
            .await;
        assert_eq!(structured(&updated)["result"].as_object().unwrap().len(), 5);
        assert_eq!(structured(&updated)["result"]["state"], "active");
        let timestamp = structured(&updated)["result"]["last_updated"].clone();
        assert!(timestamp.is_i64());
        assert_eq!(
            structured(&updated)["result"]["last_updated_source"],
            "file_mtime"
        );
        revision = structured(&updated)["result"]["revision"].clone();
        board_sizes.push(updated["result"].to_string().len());
        // Fixed timestamp and lifecycle fields extend the receipt, never its Markdown body.
        assert!(updated.to_string().len() < 600);
        let unchanged = mcp
            .call("read_board", json!({"if_revision":revision}))
            .await;
        assert_eq!(
            structured(&unchanged)["result"],
            json!({"name":"main","revision":revision,"unchanged":true,"last_updated":timestamp,"last_updated_source":"file_mtime","state":"active"})
        );
        let changed = mcp.call("read_board", json!({"if_revision":"stale"})).await;
        assert_eq!(structured(&changed)["result"]["markdown"], body);
        let verbose = mcp.call("update_board", json!({"expected_revision":revision,"verbose":true,"edits":[{"op":"set","markdown":body}]})).await;
        assert_eq!(structured(&verbose)["result"]["board"]["markdown"], body);
    }
    // State names vary during delivery, but growth is bounded independently of body size.
    assert!(send_sizes[0].abs_diff(send_sizes[1]) < 32);
    assert_eq!(board_sizes[0], board_sizes[1]);
    let compact = mcp.call("list_agents", json!({})).await;
    for agent in structured(&compact)["result"].as_array().unwrap() {
        assert!(agent.get("endpoint").is_none());
        assert!(agent.get("session").is_none());
        assert!(agent.get("machine_name").is_none());
        assert_eq!(agent["enabled"], true);
    }
    let verbose = mcp.call("list_agents", json!({"verbose":true})).await;
    assert!(structured(&verbose)["result"][0].get("session").is_some());
    assert!(
        structured(&verbose)["result"][0]
            .get("machine_name")
            .is_some()
    );
    let error = mcp.call("update_board", json!({"expected_revision":revision,"edits":[{"op":"replace","old":"PRIVATE_MISSING_ANCHOR","new":"PRIVATE_REPLACEMENT"}]})).await;
    assert!(failed(&error));
    assert_eq!(
        error["result"]["structuredContent"]["code"],
        "operation_failed"
    );
    assert!(!error.to_string().contains("PRIVATE_MISSING_ANCHOR"));
    assert!(!error.to_string().contains("PRIVATE_REPLACEMENT"));
    mcp.close().await;
}

#[tokio::test]
async fn incremental_message_reads_reuse_cursors_without_repeating_bodies() {
    let root = root();
    let state = root.path().join("state");
    let running = Running::start(&state);
    running.inbox("alice");
    running.inbox("bob");
    let mut mcp = Mcp::start(&state);
    mcp.initialize().await;
    let initial = mcp
        .call(
            "list_messages",
            json!({"agent":"bob","cursor":"","limit":1}),
        )
        .await;
    assert_eq!(structured(&initial)["result"]["messages"], json!([]));
    let mut cursor = structured(&initial)["result"]["cursor"].clone();
    let mut ids = Vec::new();
    for body in ["first", "second"] {
        let sent = mcp
            .call(
                "send_message",
                json!({"from":"alice","to":"bob","body":body}),
            )
            .await;
        ids.push(structured(&sent)["result"]["id"].clone());
    }
    for (index, id) in ids.iter().enumerate() {
        let page = mcp
            .call(
                "list_messages",
                json!({"agent":"bob","cursor":cursor,"limit":1}),
            )
            .await;
        let page = &structured(&page)["result"];
        assert_eq!(page["messages"].as_array().unwrap().len(), 1);
        assert_eq!(&page["messages"][0]["id"], id);
        assert_eq!(page["has_more"], index == 0);
        cursor = page["cursor"].clone();
    }
    let empty = mcp
        .call("list_messages", json!({"agent":"bob","cursor":cursor}))
        .await;
    assert_eq!(
        structured(&empty)["result"],
        json!({"messages":[],"cursor":cursor,"has_more":false})
    );
    for arguments in [
        json!({"agent":"alice","cursor":cursor}),
        json!({"agent":"bob","cursor":"invalid"}),
    ] {
        assert!(failed(&mcp.call("list_messages", arguments).await));
    }
    // Existing callers still get recent-history arrays.
    let legacy = mcp.call("list_messages", json!({"agent":"bob"})).await;
    assert_eq!(structured(&legacy)["result"].as_array().unwrap().len(), 2);
    mcp.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn named_boards_keep_mcp_revisions_results_and_writes_isolated() {
    let root = root();
    let state = root.path().join("state");
    let _running = Running::start(&state);
    let mut mcp = Mcp::start(&state);
    mcp.initialize().await;
    let initial = mcp.call("list_boards", json!({})).await;
    assert_eq!(structured(&initial)["result"].as_array().unwrap().len(), 1);
    assert_eq!(structured(&initial)["result"][0]["name"], "main");
    assert_eq!(structured(&initial)["result"][0]["exists"], false);
    let mut revisions = Vec::new();
    for name in ["alpha", "beta"] {
        let created = mcp.call("create_board", json!({"board":name})).await;
        let snapshot = &structured(&created)["result"];
        assert_eq!(snapshot["name"], name);
        assert_eq!(snapshot["exists"], true);
        let revision = snapshot["revision"].clone();
        let updated = mcp.call("update_board", json!({"board":name,"expected_revision":revision,"edits":[{"op":"set","markdown":"same content"}]})).await;
        let receipt = &structured(&updated)["result"];
        assert_eq!(receipt["board"], name);
        assert!(receipt.get("markdown").is_none());
        revisions.push(receipt["revision"].clone());
    }
    let native = Native::start(&_running, &root.path().join("native"), "named-owner");
    for name in ["alpha", "beta"] {
        let subscribed = mcp
            .call(
                "subscribe",
                json!({"events":[{"event":"board.changed","board":name}],"agent":native.name,"board_revisions":{(name):revisions[if name=="alpha" {0} else {1}]}}),
            )
            .await;
        let events = structured(&subscribed)["result"]["events"]
            .as_array()
            .unwrap();
        assert!(events.contains(&json!({"event":"board.changed","board":name})));
    }
    let unsubscribed = mcp
        .call(
            "unsubscribe",
            json!({"events":[{"event":"board.changed","board":"alpha"}],"agent":native.name,"board_revisions":{"alpha":revisions[0]}}),
        )
        .await;
    assert_eq!(
        structured(&unsubscribed)["result"]["events"],
        json!([{"event":"board.changed","board":"beta"}])
    );
    assert_ne!(revisions[0], revisions[1]);
    let wrong_cache = mcp
        .call(
            "read_board",
            json!({"board":"beta","if_revision":revisions[0]}),
        )
        .await;
    assert_eq!(
        structured(&wrong_cache)["result"]["markdown"],
        "same content"
    );
    let unchanged = mcp
        .call(
            "read_board",
            json!({"board":"beta","if_revision":revisions[1]}),
        )
        .await;
    assert_eq!(
        structured(&unchanged)["result"],
        json!({"name":"beta","revision":revisions[1],"unchanged":true,
            "last_updated":structured(&wrong_cache)["result"]["last_updated"],
            "last_updated_source":structured(&wrong_cache)["result"]["last_updated_source"],
            "incarnation":structured(&wrong_cache)["result"]["incarnation"],
            "lifecycle_generation":structured(&wrong_cache)["result"]["lifecycle_generation"],"state":"active"})
    );
    let conflict = mcp.call("update_board", json!({"board":"beta","expected_revision":revisions[0],"edits":[{"op":"set","markdown":"wrong board"}]})).await;
    assert_eq!(conflict["result"]["isError"], true);
    assert_eq!(
        conflict["result"]["structuredContent"]["result"]["board"]["name"],
        "beta"
    );
    assert_eq!(
        conflict["result"]["structuredContent"]["result"]["board"]["markdown"],
        "same content"
    );
    for arguments in [
        json!({}),
        json!({"board":"../main"}),
        json!({"board":"Alpha"}),
        json!({"board":"alpha"}),
    ] {
        let rejected = mcp.call("create_board", arguments).await;
        assert!(
            rejected.get("error").is_some() || rejected["result"]["isError"] == true,
            "{rejected}"
        );
    }
    let missing = mcp.call("update_board", json!({"board":"missing","expected_revision":revisions[0],"edits":[{"op":"set","markdown":"must not create"}]})).await;
    assert_eq!(missing["result"]["isError"], true);
    let list = mcp.call("list_boards", json!({})).await;
    let names: Vec<_> = structured(&list)["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            assert!(row.get("markdown").is_none());
            row["name"].as_str().unwrap()
        })
        .collect();
    assert_eq!(names, ["alpha", "beta", "main"]);
    let main = mcp.call("read_board", json!({})).await;
    assert_eq!(structured(&main)["result"]["exists"], false);
    assert_eq!(structured(&main)["result"]["markdown"], "");
}

#[tokio::test]
async fn archive_tools_preserve_fixed_deadlines_contents_and_lifecycle_fences() {
    let root = root();
    let state = root.path().join("state");
    let running = Running::start(&state);
    let mut mcp = Mcp::start(&state);
    mcp.initialize().await;
    let created = mcp
        .call("create_board", json!({"board":"archive-test"}))
        .await;
    let original = "# Exact\n\n雪\tcontent\n";
    let updated=mcp.call("update_board",json!({"board":"archive-test","expected_revision":structured(&created)["result"]["revision"],"edits":[{"op":"set","markdown":original}]})).await;
    assert_ne!(updated["result"]["isError"], true);
    let read = mcp
        .call("read_board", json!({"board":"archive-test"}))
        .await;
    let before = structured(&read)["result"].clone();
    let main = mcp.call("read_board", json!({})).await;
    assert_eq!(
        mcp.call(
            "archive_board",
            json!({"board":"main","expected_revision":structured(&main)["result"]["revision"]})
        )
        .await["result"]["isError"],
        true
    );
    let archived = mcp
        .call(
            "archive_board",
            json!({"board":"archive-test","expected_revision":before["revision"]}),
        )
        .await;
    assert_ne!(archived["result"]["isError"], true);
    let saved = structured(&archived)["result"].clone();
    assert_eq!(saved["state"], "archived");
    assert_eq!(saved["markdown"], original);
    assert_eq!(
        saved["expires_at"].as_i64().unwrap() - saved["archived_at"].as_i64().unwrap(),
        30 * 24 * 60 * 60 * 1000
    );
    let repeat = mcp
        .call(
            "archive_board",
            json!({"board":"archive-test","expected_revision":before["revision"]}),
        )
        .await;
    assert_eq!(
        structured(&repeat)["result"]["expires_at"],
        saved["expires_at"]
    );
    let active = mcp.call("list_boards", json!({})).await;
    assert_eq!(structured(&active)["result"].as_array().unwrap().len(), 1);
    assert_eq!(structured(&active)["result"][0]["name"], "main");
    let listed = mcp.call("list_archived_boards", json!({})).await;
    assert_eq!(structured(&listed)["result"][0]["name"], "archive-test");
    assert!(structured(&listed)["result"][0].get("markdown").is_none());
    assert_eq!(
        mcp.call("read_board", json!({"board":"archive-test"}))
            .await["result"]["isError"],
        true
    );
    let inspected = mcp
        .call("inspect_board", json!({"board":"archive-test"}))
        .await;
    assert_eq!(structured(&inspected)["result"]["markdown"], original);
    let rejected=mcp.call("update_board",json!({"board":"archive-test","expected_revision":before["revision"],"edits":[{"op":"append","text":"forbidden"}]})).await;
    assert_eq!(rejected["result"]["isError"], true);
    let restored = mcp
        .call(
            "restore_board",
            json!({"board":"archive-test","expected_revision":saved["revision"]}),
        )
        .await;
    assert_ne!(restored["result"]["isError"], true);
    let restored = structured(&restored)["result"].clone();
    assert_eq!(restored["state"], "active");
    assert_eq!(restored["markdown"], original);
    assert_eq!(restored["last_updated"], before["last_updated"]);
    assert_eq!(restored["incarnation"], before["incarnation"]);
    assert_ne!(
        restored["lifecycle_generation"],
        before["lifecycle_generation"]
    );
    let stale=mcp.call("update_board",json!({"board":"archive-test","expected_revision":before["revision"],"edits":[{"op":"append","text":"stale"}]})).await;
    assert_eq!(stale["result"]["isError"], true);
    let injected=mcp.call("archive_board",json!({"board":"archive-test","expected_revision":restored["revision"],"archived_at":0})).await;
    assert!(injected.get("error").is_some() || injected["result"]["isError"] == true);
    let active = running.request(Request::NamedBoardRead {
        board: "archive-test".parse().unwrap(),
    });
    assert_eq!(active["markdown"], original);
    assert_eq!(active["state"], "active");
    mcp.close().await;
}

#[tokio::test]
async fn cli_archive_restore_and_explicit_inspection_use_the_same_broker_contract() {
    async fn cli(state: &Path, home: &Path, args: &[&str]) -> std::process::Output {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-bridge"));
        command
            .env_clear()
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .current_dir(home)
            .args(["--state-dir", state.to_str().unwrap()])
            .args(args)
            .kill_on_drop(true);
        timeout(Duration::from_secs(8), command.output())
            .await
            .expect("CLI request timed out")
            .unwrap()
    }
    let root = root();
    let state = root.path().join("state");
    let running = Running::start(&state);
    let created = cli(&state, root.path(), &["board", "--create", "cli-archive"]).await;
    assert!(created.status.success());
    let name = "cli-archive".parse().unwrap();
    let current = running.request(Request::board_read(name));
    running.request(Request::board_update(
        "cli-archive".parse().unwrap(),
        current["revision"].as_str().unwrap().into(),
        vec![agent_bridge::board::Edit::Set {
            markdown: "exact CLI bytes\n雪\n".into(),
        }],
        None,
    ));
    let metadata = cli(&state, root.path(), &["board", "cli-archive", "--metadata"]).await;
    assert!(metadata.status.success());
    let before: Value = serde_json::from_slice(&metadata.stdout).unwrap();
    assert!(before.get("markdown").is_none());
    let missing = cli(&state, root.path(), &["board", "--archive", "cli-archive"]).await;
    assert!(!missing.status.success());
    let archived = cli(
        &state,
        root.path(),
        &[
            "board",
            "--archive",
            "cli-archive",
            "--expected-revision",
            before["revision"].as_str().unwrap(),
        ],
    )
    .await;
    assert!(archived.status.success());
    let saved: Value = serde_json::from_slice(&archived.stdout).unwrap();
    assert_eq!(saved["state"], "archived");
    assert!(saved.get("markdown").is_none());
    let listed = cli(&state, root.path(), &["board", "--list", "--archived"]).await;
    assert!(listed.status.success());
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed[0]["name"], "cli-archive");
    assert_eq!(listed[0]["expires_at"], saved["expires_at"]);
    let ordinary = cli(&state, root.path(), &["board", "cli-archive", "--raw"]).await;
    assert!(!ordinary.status.success());
    let exact = cli(
        &state,
        root.path(),
        &["board", "cli-archive", "--archived", "--raw"],
    )
    .await;
    assert!(exact.status.success());
    assert_eq!(exact.stdout, "exact CLI bytes\n雪\n".as_bytes());
    let restored = cli(
        &state,
        root.path(),
        &[
            "board",
            "--restore",
            "cli-archive",
            "--expected-revision",
            saved["revision"].as_str().unwrap(),
        ],
    )
    .await;
    assert!(restored.status.success());
    let restored: Value = serde_json::from_slice(&restored.stdout).unwrap();
    assert_eq!(restored["state"], "active");
    assert_eq!(restored["last_updated"], before["last_updated"]);
    let raw = cli(&state, root.path(), &["board", "cli-archive", "--raw"]).await;
    assert_eq!(raw.stdout, exact.stdout);
}

fn assert_logical_board_response(value: &Value, private_state: &Path) {
    let encoded = serde_json::to_string(value).unwrap();
    assert!(
        !encoded.contains(private_state.to_str().unwrap()),
        "{encoded}"
    );
    fn assert_no_storage_key(value: &Value) {
        match value {
            Value::Object(object) => {
                assert!(
                    !object.contains_key("path"),
                    "Service-generated backing path escaped: {value}"
                );
                for child in object.values() {
                    assert_no_storage_key(child);
                }
            }
            Value::Array(array) => {
                for child in array {
                    assert_no_storage_key(child);
                }
            }
            _ => (),
        }
    }
    assert_no_storage_key(value);
}

async fn logical_board_cli(state: &Path, args: &[&str]) -> String {
    let output = timeout(
        Duration::from_secs(8),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
            .args(["--state-dir", state.to_str().unwrap()])
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains(state.to_str().unwrap()), "{stdout}");
    assert!(!String::from_utf8_lossy(&output.stderr).contains(state.to_str().unwrap()));
    stdout
}

#[tokio::test]
async fn logical_board_projections_cover_mcp_rpc_cli_and_lifecycle_without_backing_paths() {
    use agent_bridge::board::{Edit, Name, Snapshot};
    let root = root();
    let state = root
        .path()
        .join("distinctive-private-board-storage-never-in-responses");
    let running = Running::start(&state);
    let mut mcp = Mcp::start(&state);
    mcp.initialize().await;
    let main = mcp.call("read_board", json!({})).await;
    assert_logical_board_response(&main, &state);
    let absent: Snapshot = serde_json::from_value(structured(&main)["result"].clone()).unwrap();
    assert!(absent.path.as_os_str().is_empty());
    assert_eq!(absent.name, "main");
    assert!(!absent.exists);
    let created = mcp.call("create_board", json!({"board":"logical"})).await;
    assert_logical_board_response(&created, &state);
    let revision = structured(&created)["result"]["revision"].clone();
    let authored = "# Logical board\nKeep the authored path /home/author/work/report.md exactly.\n```json\n{\"path\":\"/home/author/work/example.md\"}\n```\n";
    let compact = mcp.call("update_board", json!({"board":"logical","expected_revision":revision,"edits":[{"op":"append","text":authored}]})).await;
    assert_logical_board_response(&compact, &state);
    assert!(structured(&compact)["result"]["revision"].is_string());
    let read = mcp.call("read_board", json!({"board":"logical"})).await;
    assert_logical_board_response(&read, &state);
    let before = structured(&read)["result"].clone();
    assert_eq!(before["markdown"], authored);
    let unchanged = mcp
        .call(
            "read_board",
            json!({"board":"logical","if_revision":before["revision"]}),
        )
        .await;
    assert_logical_board_response(&unchanged, &state);
    assert_eq!(structured(&unchanged)["result"]["unchanged"], true);
    let verbose = mcp.call("update_board", json!({"board":"logical","expected_revision":before["revision"],"verbose":true,"edits":[{"op":"append","text":"Other writer\n"}]})).await;
    assert_logical_board_response(&verbose, &state);
    assert!(
        structured(&verbose)["result"]["board"]["markdown"]
            .as_str()
            .unwrap()
            .contains(authored)
    );
    for verbose in [false, true] {
        let conflict = mcp.call("update_board", json!({"board":"logical","expected_revision":before["revision"],"verbose":verbose,"edits":[{"op":"append","text":"Intended edit\n"}]})).await;
        assert_logical_board_response(&conflict, &state);
        assert!(failed(&conflict));
        let current = &conflict["result"]["structuredContent"]["result"];
        assert_eq!(current["conflict"], true);
        assert!(
            current["board"]["markdown"]
                .as_str()
                .unwrap()
                .contains(authored)
        );
    }
    let latest = mcp.call("read_board", json!({"board":"logical"})).await;
    let retry = mcp.call("update_board", json!({"board":"logical","expected_revision":structured(&latest)["result"]["revision"],"verbose":true,"edits":[{"op":"append","text":"Intended edit\n"}]})).await;
    assert_logical_board_response(&retry, &state);
    let content = structured(&retry)["result"]["board"]["markdown"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(content, format!("{authored}Other writer\nIntended edit\n"));
    let name: Name = "logical".parse().unwrap();
    let rpc_read = running.request(Request::board_read(name.clone()));
    assert_logical_board_response(&rpc_read, &state);
    let snapshot: Snapshot = serde_json::from_value(rpc_read.clone()).unwrap();
    assert!(snapshot.path.as_os_str().is_empty());
    let rpc_conflict = running.request(Request::board_update(
        name.clone(),
        before["revision"].as_str().unwrap().into(),
        vec![Edit::Append {
            text: "do not overwrite".into(),
        }],
        None,
    ));
    assert_logical_board_response(&rpc_conflict, &state);
    assert_eq!(rpc_conflict["conflict"], true);
    let listed = mcp.call("list_boards", json!({})).await;
    assert_logical_board_response(&listed, &state);
    let cli = timeout(
        Duration::from_secs(8),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
            .args([
                "--state-dir",
                state.to_str().unwrap(),
                "board",
                "logical",
                "--metadata",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        cli.status.success(),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let metadata: Value = serde_json::from_slice(&cli.stdout).unwrap();
    assert_logical_board_response(&metadata, &state);
    assert_eq!(metadata["name"], "logical");
    let access = running
        .broker
        .request_async(Request::MonitorEnable { port: 0 })
        .await
        .unwrap();
    let address = access["url"]
        .as_str()
        .unwrap()
        .strip_prefix("http://")
        .unwrap();
    for route in [
        "/api/boards".to_owned(),
        "/api/boards/logical".to_owned(),
        format!(
            "/api/boards/logical?if_revision={}",
            rpc_read["revision"].as_str().unwrap()
        ),
    ] {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream.write_all(format!("GET {route} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n", access["access_token"].as_str().unwrap()).as_bytes()).await.unwrap();
        let mut bytes = Vec::new();
        timeout(Duration::from_secs(5), stream.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        let response = String::from_utf8(bytes).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(!response.contains(state.to_str().unwrap()), "{response}");
        let body = response.split_once("\r\n\r\n").unwrap().1;
        let body: Value = serde_json::from_str(body).unwrap();
        assert_logical_board_response(&body, &state);
        if route == "/api/boards/logical" {
            assert_eq!(body["markdown"], content);
        }
    }
    assert_eq!(
        logical_board_cli(&state, &["board", "logical", "--raw"]).await,
        content
    );
    logical_board_cli(&state, &["board", "--create", "cli-logical"]).await;
    logical_board_cli(&state, &["board", "--list"]).await;
    let archived = mcp
        .call(
            "archive_board",
            json!({"board":"logical","expected_revision":rpc_read["revision"]}),
        )
        .await;
    assert_logical_board_response(&archived, &state);
    assert_eq!(structured(&archived)["result"]["markdown"], content);
    let inspected = mcp.call("inspect_board", json!({"board":"logical"})).await;
    assert_logical_board_response(&inspected, &state);
    let archives = mcp.call("list_archived_boards", json!({})).await;
    assert_logical_board_response(&archives, &state);
    let archive_metadata =
        logical_board_cli(&state, &["board", "logical", "--archived", "--metadata"]).await;
    assert_logical_board_response(&serde_json::from_str(&archive_metadata).unwrap(), &state);
    let archive_list = logical_board_cli(&state, &["board", "--list", "--archived"]).await;
    assert_logical_board_response(&serde_json::from_str(&archive_list).unwrap(), &state);
    assert_eq!(
        logical_board_cli(&state, &["board", "logical", "--archived", "--raw"]).await,
        content
    );
    let restored = mcp.call("restore_board", json!({"board":"logical","expected_revision":structured(&inspected)["result"]["revision"]})).await;
    assert_logical_board_response(&restored, &state);
    assert_eq!(structured(&restored)["result"]["markdown"], content);
    let internal = agent_bridge::board::read_named(&state, &name).unwrap();
    assert!(!internal.path.as_os_str().is_empty());
    assert_eq!(internal.markdown, content);
    assert_logical_board_response(&serde_json::to_value(&internal).unwrap(), &state);
    mcp.close().await;
}

#[tokio::test]
async fn current_clients_project_legacy_broker_board_snapshots_before_mcp_and_cli_output() {
    let root = root();
    let listener = UnixListener::bind(root.path().join("bridge.sock")).unwrap();
    let backing = root
        .path()
        .join("distinctive-private-legacy-storage/boards/logical.md");
    let authored = "Keep authored /work/report.md and {\"path\":\"/work/example.md\"} exactly.";
    let legacy = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut line = String::new();
            BufReader::new(reader).read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let snapshot = json!({"name":request["board"].as_str().unwrap_or("main"),"markdown":authored,"revision":"current","exists":true,"path":backing});
            let result = match request["op"].as_str().unwrap() {
                "board_list" | "board_archived_list" => json!([snapshot]),
                "board_update" | "named_board_update" | "add_board_message_input" => {
                    if request["expected_revision"] == "stale" {
                        json!({"applied":false,"conflict":true,"board":snapshot})
                    } else {
                        json!({"applied":true,"board":snapshot})
                    }
                }
                "status" => json!({"state_dir":"/unrelated/service/configuration"}),
                _ => snapshot,
            };
            writer
                .write_all(format!("{}\n", json!({"ok":true,"result":result})).as_bytes())
                .await
                .unwrap();
        }
    });
    let mut mcp = Mcp::start(root.path());
    mcp.initialize().await;
    for (tool, arguments) in [
        ("read_board", json!({})),
        ("read_board", json!({"board":"logical"})),
        (
            "read_board",
            json!({"board":"logical","if_revision":"current"}),
        ),
        ("create_board", json!({"board":"logical"})),
        ("list_boards", json!({})),
        ("list_archived_boards", json!({})),
        ("inspect_board", json!({"board":"logical"})),
        (
            "archive_board",
            json!({"board":"logical","expected_revision":"current"}),
        ),
        (
            "restore_board",
            json!({"board":"logical","expected_revision":"current"}),
        ),
        (
            "update_board",
            json!({"expected_revision":"current","edits":[],"verbose":false}),
        ),
        (
            "update_board",
            json!({"board":"logical","expected_revision":"current","edits":[],"verbose":true}),
        ),
        (
            "update_board",
            json!({"board":"logical","expected_revision":"stale","edits":[],"verbose":false}),
        ),
        (
            "update_board",
            json!({"board":"logical","expected_revision":"stale","edits":[],"verbose":true}),
        ),
        (
            "add_board_message_input",
            json!({"agent":"owner","board":"logical","expected_revision":"current","id":"compose","label":"Message owner"}),
        ),
        (
            "add_board_message_input",
            json!({"agent":"owner","board":"logical","expected_revision":"stale","id":"compose","label":"Message owner"}),
        ),
    ] {
        let conflict = arguments["expected_revision"] == "stale";
        let name = arguments["board"].as_str().unwrap_or("main");
        let response = mcp.call(tool, arguments.clone()).await;
        assert_logical_board_response(&response, root.path());
        assert!(response.get("error").is_none(), "{tool}: {response}");
        assert_eq!(
            failed(&response),
            tool == "update_board" && conflict,
            "{tool}: {response}"
        );
        let structured = &response["result"]["structuredContent"];
        let text: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(
            &text, structured,
            "MCP text and structured output must agree for {tool}"
        );
        let result = &structured["result"];
        let assert_snapshot = |snapshot: &Value, expected_name: &str| {
            assert_eq!(snapshot["name"], expected_name, "{tool}: {response}");
            assert_eq!(snapshot["revision"], "current", "{tool}: {response}");
            assert_eq!(snapshot["markdown"], authored, "{tool}: {response}");
            assert_eq!(snapshot["exists"], true, "{tool}: {response}");
        };
        match tool {
            "list_boards" | "list_archived_boards" => {
                let rows = result
                    .as_array()
                    .expect("Legacy catalog must remain an array");
                assert_eq!(rows.len(), 1);
                assert_snapshot(&rows[0], "main");
            }
            "read_board" if arguments["if_revision"] == "current" => {
                assert_eq!(result["name"], name);
                assert_eq!(result["revision"], "current");
                assert_eq!(result["unchanged"], true);
                assert!(result.get("markdown").is_none());
            }
            "update_board" if !conflict && arguments["verbose"] != true => {
                assert_eq!(result["board"], name);
                assert_eq!(result["revision"], "current");
                assert!(result.get("markdown").is_none());
            }
            "update_board" | "add_board_message_input" => {
                assert_eq!(result["applied"], !conflict);
                assert_eq!(result["conflict"] == true, conflict);
                assert_snapshot(&result["board"], name);
            }
            _ => assert_snapshot(result, name),
        }
    }
    mcp.close().await;
    for revision in ["current", "stale"] {
        let output = logical_board_cli(
            root.path(),
            &[
                "add-board-message-input",
                "owner",
                "--board",
                "logical",
                "--expected-revision",
                revision,
                "--id",
                "compose",
                "--label",
                "Message owner",
            ],
        )
        .await;
        let value: Value = serde_json::from_str(&output).unwrap();
        assert_logical_board_response(&value, root.path());
        assert_eq!(value["board"]["markdown"], authored);
        assert_eq!(value["applied"], revision == "current");
        assert_eq!(value["conflict"] == true, revision == "stale");
        assert_eq!(value["board"]["name"], "logical");
        assert_eq!(value["board"]["revision"], "current");
        assert_eq!(value["board"]["exists"], true);
    }
    let unrelated = broker::rpc(root.path(), &Request::Status).await.unwrap();
    assert_eq!(unrelated["state_dir"], "/unrelated/service/configuration");
    legacy.abort();
    assert!(legacy.await.unwrap_err().is_cancelled());
}

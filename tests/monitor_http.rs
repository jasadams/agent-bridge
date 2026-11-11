use agent_bridge::{
    broker::{self, Broker},
    protocol::Request,
};
use serde_json::Value;
use std::{path::Path, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

struct Running {
    broker: Broker,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}
impl Running {
    fn start(path: &Path) -> Self {
        let bound = Broker::bind(path.into(), "/missing/codex".into()).unwrap();
        let broker = bound.broker.clone();
        let task = tokio::spawn(broker::serve(bound));
        Self { broker, task }
    }
    async fn enable(&self, port: u16) -> Value {
        self.broker
            .request_async(Request::MonitorEnable { port })
            .await
            .unwrap()
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn board_snapshot(
    running: &Running,
    name: &agent_bridge::board::Name,
) -> agent_bridge::board::Snapshot {
    serde_json::from_value(
        running
            .broker
            .request(Request::board_read(name.clone()))
            .unwrap(),
    )
    .unwrap()
}
fn board_metadata(running: &Running, name: &agent_bridge::board::Name) -> Value {
    let board = board_snapshot(running, name);
    let mut value = serde_json::to_value(&board.lifecycle).unwrap();
    value["name"] = serde_json::json!(board.name);
    value["revision"] = serde_json::json!(board.revision);
    value["exists"] = serde_json::json!(board.exists);
    value["last_updated"] = serde_json::json!(board.last_updated);
    value["last_updated_source"] = serde_json::json!(board.last_updated_source);
    value["read_only"] = serde_json::json!(!board.writable());
    value["message_labels"] = serde_json::json!({});
    value
}

fn host(access: &Value) -> String {
    access["url"]
        .as_str()
        .unwrap()
        .strip_prefix("http://")
        .unwrap()
        .into()
}
fn key(access: &Value) -> &str {
    access["access_token"].as_str().unwrap()
}
async fn connect(access: &Value, path: &str, headers: &str) -> TcpStream {
    let target = host(access);
    let mut stream = TcpStream::connect(&target).await.unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: {target}\r\n{headers}\r\n").as_bytes())
        .await
        .unwrap();
    stream
}
async fn get(access: &Value, path: &str, headers: &str) -> String {
    let mut stream = connect(access, path, headers).await;
    let mut bytes = Vec::new();
    timeout(Duration::from_secs(3), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(bytes).unwrap()
}
fn auth(access: &Value) -> String {
    format!("Authorization: Bearer {}\r\n", key(access))
}

#[tokio::test]
async fn topic_catalog_requires_monitor_access_and_respects_browser_origin() {
    let root = tempfile::Builder::new()
        .prefix("ab-topic-http-")
        .tempdir_in("/tmp")
        .unwrap();
    let running = Running::start(root.path());
    let access = running.enable(0).await;
    assert!(
        get(&access, "/?topic=00000000-0000-4000-8000-000000000001", "")
            .await
            .starts_with("HTTP/1.1 200")
    );
    assert!(
        get(&access, "/?token=secret", "")
            .await
            .starts_with("HTTP/1.1 400")
    );
    assert!(
        get(&access, "/?topic=invalid-id", "")
            .await
            .starts_with("HTTP/1.1 200")
    );
    for query in [
        "?topic=",
        "?topic=a&topic=b",
        "?topic=a&token=secret",
        "?topic=%0A",
        &format!("?topic={}", "a".repeat(101)),
    ] {
        let response = get(&access, &format!("/{query}"), "").await;
        assert!(response.starts_with("HTTP/1.1 400"), "{query}: {response}");
    }
    assert!(
        get(&access, "/api/topics", "")
            .await
            .starts_with("HTTP/1.1 401")
    );
    assert!(
        get(
            &access,
            "/api/topics",
            &format!("{}Origin: https://evil.example\r\n", auth(&access))
        )
        .await
        .starts_with("HTTP/1.1 403")
    );
    let catalog = get(&access, "/api/topics", &auth(&access)).await;
    assert!(catalog.starts_with("HTTP/1.1 200"), "{catalog}");
    let payload = catalog.split_once("\r\n\r\n").unwrap().1;
    let value: Value = serde_json::from_str(payload).unwrap();
    assert_eq!(value["topics"], serde_json::json!([]));
}

#[tokio::test]
async fn agent_names_require_monitor_access_and_respect_browser_origin() {
    let root = tempfile::Builder::new()
        .prefix("ab-agent-names-")
        .tempdir_in("/tmp")
        .unwrap();
    let running = Running::start(root.path());
    let access = running.enable(0).await;
    assert!(
        get(&access, "/api/agents", "")
            .await
            .starts_with("HTTP/1.1 401")
    );
    assert!(
        get(
            &access,
            "/api/agents",
            &format!("{}Origin: https://evil.example\r\n", auth(&access))
        )
        .await
        .starts_with("HTTP/1.1 403")
    );
    assert!(
        get(&access, "/api/agents?agent=x", &auth(&access))
            .await
            .starts_with("HTTP/1.1 400")
    );
}

async fn first_event(stream: &mut TcpStream) -> String {
    timeout(Duration::from_secs(3), async {
        let mut bytes = Vec::new();
        let mut buf = [0; 1024];
        loop {
            let n = stream.read(&mut buf).await.unwrap();
            assert_ne!(n, 0, "SSE closed before first event");
            bytes.extend_from_slice(&buf[..n]);
            if bytes
                .windows(b"event: revision".len())
                .any(|b| b == b"event: revision")
            {
                break;
            }
        }
        String::from_utf8(bytes).unwrap()
    })
    .await
    .unwrap()
}
async fn board_event(stream: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0];
        assert_ne!(stream.read(&mut byte).await.unwrap(), 0, "Board SSE closed");
        bytes.push(byte[0]);
        if bytes.ends_with(b"\n\n")
            && bytes
                .windows(21)
                .any(|part| part == b"event: board-revision")
        {
            let event = String::from_utf8(bytes).unwrap();
            let data = event
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap();
            return serde_json::from_str(data).unwrap();
        }
    }
}

#[tokio::test]
async fn board_events_follow_only_selected_revision_including_external_edits_and_revoke() {
    use agent_bridge::board;
    let root = tempfile::tempdir().unwrap();
    let running = Running::start(root.path());
    for name in ["alpha", "beta"] {
        running
            .broker
            .request(Request::BoardCreate {
                board: name.parse().unwrap(),
            })
            .unwrap();
    }
    let alpha: board::Name = "alpha".parse().unwrap();
    let beta: board::Name = "beta".parse().unwrap();
    let access = running.enable(0).await;
    let path = "/api/boards/alpha/events";
    assert!(get(&access, path, "").await.starts_with("HTTP/1.1 401"));
    for invalid in [
        "/api/boards/ALPHA/events",
        "/api/boards/alpha/events?token=secret",
        "/api/boards/alpha/events/events",
    ] {
        assert!(
            get(&access, invalid, &auth(&access))
                .await
                .starts_with("HTTP/1.1 400")
        );
    }
    assert!(
        get(&access, "/api/boards/missing/events", &auth(&access))
            .await
            .starts_with("HTTP/1.1 404")
    );
    assert!(
        get(
            &access,
            path,
            &format!("{}Origin: https://evil.example\r\n", auth(&access))
        )
        .await
        .starts_with("HTTP/1.1 403")
    );
    let mut stream = connect(&access, path, &auth(&access)).await;
    let first = timeout(Duration::from_secs(3), board_event(&mut stream))
        .await
        .unwrap();
    assert_eq!(first, board_metadata(&running, &alpha));
    // Another board and unrelated broker activity must not invalidate alpha.
    let before = board_snapshot(&running, &beta);
    running
        .broker
        .request(Request::NamedBoardUpdate {
            board: beta,
            expected_revision: before.revision,
            edits: vec![board::Edit::Set {
                markdown: "# Other board secret\n".into(),
            }],
            actor: None,
        })
        .unwrap();
    assert!(
        timeout(Duration::from_millis(1100), board_event(&mut stream))
            .await
            .is_err()
    );
    // Direct file edits bypass broker notifications and still reach the stream.
    let current = board::read_named(root.path(), &alpha).unwrap();
    std::fs::write(&current.path, "# Private external content\n").unwrap();
    let changed = timeout(Duration::from_secs(3), board_event(&mut stream))
        .await
        .unwrap();
    assert_eq!(changed, board_metadata(&running, &alpha));
    assert_ne!(changed["revision"], first["revision"]);
    assert!(!changed.to_string().contains("Private external content"));
    assert!(!changed.to_string().contains(key(&access)));
    assert!(
        timeout(Duration::from_millis(1100), board_event(&mut stream))
            .await
            .is_err()
    );
    running.broker.request(Request::MonitorDisable).unwrap();
    let mut rest = Vec::new();
    timeout(Duration::from_secs(2), stream.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn board_events_refresh_metadata_when_virtual_main_is_created_with_same_revision() {
    let root = tempfile::tempdir().unwrap();
    let running = Running::start(root.path());
    let access = running.enable(0).await;
    let mut stream = connect(&access, "/api/boards/main/events", &auth(&access)).await;
    let initial = timeout(Duration::from_secs(3), board_event(&mut stream))
        .await
        .unwrap();
    assert_eq!(initial["exists"], false);
    assert_eq!(initial["last_updated"], Value::Null);
    assert_eq!(initial["last_updated_source"], "unknown");
    let created = running
        .broker
        .request(Request::BoardCreate {
            board: Default::default(),
        })
        .unwrap();
    assert_eq!(created["revision"], initial["revision"]);
    let changed = timeout(Duration::from_secs(3), board_event(&mut stream))
        .await
        .unwrap();
    assert_eq!(changed["name"], "main");
    assert_eq!(changed["revision"], initial["revision"]);
    assert_eq!(changed["exists"], true);
    assert_eq!(changed["last_updated"], created["last_updated"]);
    assert!(changed["last_updated"].is_i64());
    assert_eq!(changed["last_updated_source"], "file_mtime");
    assert!(changed.get("markdown").is_none());
    let conditional = json_body(
        &get(
            &access,
            &format!(
                "/api/boards/main?if_revision={}",
                initial["revision"].as_str().unwrap()
            ),
            &auth(&access),
        )
        .await,
    );
    assert_eq!(conditional["unchanged"], true);
    for field in [
        "name",
        "revision",
        "exists",
        "last_updated",
        "last_updated_source",
    ] {
        assert_eq!(conditional[field], changed[field]);
    }
    assert!(conditional.get("markdown").is_none());
    std::fs::File::options()
        .write(true)
        .open(root.path().join("boards/main.md"))
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000))
        .unwrap();
    assert!(
        timeout(Duration::from_millis(1100), board_event(&mut stream))
            .await
            .is_err()
    );
    let touched = json_body(
        &get(
            &access,
            &format!(
                "/api/boards/main?if_revision={}",
                initial["revision"].as_str().unwrap()
            ),
            &auth(&access),
        )
        .await,
    );
    assert_eq!(touched["unchanged"], true);
    for field in ["revision", "exists", "last_updated", "last_updated_source"] {
        assert_eq!(touched[field], changed[field]);
    }
}

#[tokio::test]
async fn board_and_message_events_share_the_live_connection_limit() {
    let root = tempfile::tempdir().unwrap();
    let running = Running::start(root.path());
    let access = running.enable(0).await;
    let mut board = connect(&access, "/api/boards/main/events", &auth(&access)).await;
    timeout(Duration::from_secs(3), board_event(&mut board))
        .await
        .unwrap();
    let mut streams = Vec::new();
    for _ in 0..7 {
        let mut stream = connect(&access, "/api/events", &auth(&access)).await;
        first_event(&mut stream).await;
        streams.push(stream);
    }
    for path in ["/api/events", "/api/boards/main/events"] {
        assert!(
            get(&access, path, &auth(&access))
                .await
                .starts_with("HTTP/1.1 429")
        );
    }
}
#[tokio::test]
async fn monitor_requires_bearer_and_enforces_browser_boundaries_on_assets_and_api() {
    let root = tempfile::Builder::new()
        .prefix("ab-http-")
        .tempdir_in("/tmp")
        .unwrap();
    let running = Running::start(root.path());
    let status = running.broker.request(Request::MonitorStatus).unwrap();
    assert_eq!(status["enabled"], false);
    assert!(status.get("access_token").is_none());
    let access = running.enable(0).await;
    assert!(key(&access).len() >= 43);
    assert!(
        !running.broker.request(Request::Status).unwrap()["network"]["enabled"]
            .as_bool()
            .unwrap()
    );
    let page = get(&access, "/", "").await;
    assert!(page.starts_with("HTTP/1.1 200"));
    assert!(page.contains("content-security-policy:"));
    assert!(page.contains("cache-control: no-store"));
    assert!(!page.contains(key(&access)));
    assert!(
        get(&access, "/api/messages", "")
            .await
            .starts_with("HTTP/1.1 401")
    );
    assert!(
        get(&access, "/api/status", &auth(&access))
            .await
            .starts_with("HTTP/1.1 200")
    );
    for extra in [
        "Origin: https://evil.example\r\n",
        "Origin: null\r\n",
        "Sec-Fetch-Site: cross-site\r\n",
        "Sec-Fetch-Site: same-site\r\n",
    ] {
        for path in ["/", "/app.js", "/api/messages"] {
            let result = get(&access, path, &format!("{}{extra}", auth(&access))).await;
            assert!(result.starts_with("HTTP/1.1 403"), "{result}");
            assert!(result.contains("cache-control: no-store"));
        }
    }
    let mut socket = TcpStream::connect(host(&access)).await.unwrap();
    socket
        .write_all(
            format!(
                "GET /api/status HTTP/1.1\r\nHost: evil.example\r\n{}\r\n",
                auth(&access)
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut bytes = Vec::new();
    socket.read_to_end(&mut bytes).await.unwrap();
    assert!(bytes.starts_with(b"HTTP/1.1 403"));
    let same_origin = format!(
        "{}Origin: {}\r\nSec-Fetch-Site: same-origin\r\n",
        auth(&access),
        access["url"].as_str().unwrap()
    );
    assert!(
        get(&access, "/api/status", &same_origin)
            .await
            .starts_with("HTTP/1.1 200")
    );
    assert!(
        get(
            &access,
            "/api/status",
            &format!("{}{}", auth(&access), auth(&access))
        )
        .await
        .starts_with("HTTP/1.1 401")
    );
    for path in [
        "/api/messages?access_token=secret",
        "/api/messages?limit=100000",
        "/api/messages?limit=0",
        "/api/messages?machine=a&machine=b",
        "/api/status?token=secret",
    ] {
        assert!(
            get(&access, path, &auth(&access))
                .await
                .starts_with("HTTP/1.1 400"),
            "{path}"
        );
    }
    assert!(!root.path().join("identity.key").exists());
}
#[tokio::test]
async fn disabling_monitor_closes_live_streams_and_rotates_credentials_independently_of_networking()
{
    let root = tempfile::Builder::new()
        .prefix("ab-http-live-")
        .tempdir_in("/tmp")
        .unwrap();
    let running = Running::start(root.path());
    let access = running.enable(0).await;
    let mut stream = connect(&access, "/api/events", &auth(&access)).await;
    let first = first_event(&mut stream).await;
    assert!(first.starts_with("HTTP/1.1 200"));
    assert!(!first.contains(key(&access)));
    let same = running.enable(0).await;
    assert_eq!(same["url"], access["url"]);
    assert_eq!(same["access_token"], access["access_token"]);
    running.broker.request(Request::MonitorDisable).unwrap();
    let mut rest = Vec::new();
    timeout(Duration::from_secs(2), stream.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap();
    let current = running.enable(0).await;
    assert_ne!(current["access_token"], access["access_token"]);
    assert!(
        get(&current, "/api/status", &auth(&access))
            .await
            .starts_with("HTTP/1.1 401")
    );
    assert!(
        get(&current, "/api/status", &auth(&current))
            .await
            .starts_with("HTTP/1.1 200")
    );
}
#[tokio::test]
async fn occupied_port_reports_failure_without_disrupting_the_broker() {
    let root = tempfile::Builder::new()
        .prefix("ab-http-busy-")
        .tempdir_in("/tmp")
        .unwrap();
    let running = Running::start(root.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(
        running
            .broker
            .request_async(Request::MonitorEnable { port })
            .await
            .is_err()
    );
    assert!(
        running
            .broker
            .request(Request::List {
                all: false,
                max_age_days: 7
            })
            .is_ok()
    );
    assert!(
        running
            .broker
            .request(Request::MonitorStatus)
            .unwrap()
            .get("access_token")
            .is_none()
    );
    running.broker.request(Request::MonitorDisable).unwrap();
    assert!(running.enable(0).await["url"].is_string());
}

fn json_body(response: &str) -> Value {
    serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap()
}
async fn post(access: &Value, path: &str, headers: &str, body: &str) -> String {
    let mut socket = TcpStream::connect(host(access)).await.unwrap();
    socket
        .write_all(
            format!(
                "POST {path} HTTP/1.1\r\nHost: {}\r\n{}Content-Length: {}\r\n{headers}\r\n{body}",
                host(access),
                auth(access),
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut bytes = Vec::new();
    timeout(Duration::from_secs(7), socket.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(bytes).unwrap()
}
fn post_headers(access: &Value) -> String {
    format!(
        "Origin: {}\r\nContent-Type: application/json\r\n",
        access["url"].as_str().unwrap()
    )
}

#[tokio::test]
async fn topic_http_pins_human_identity_and_preserves_retry_and_archive_history() {
    let root = tempfile::tempdir().unwrap();
    let running = Running::start(root.path());
    let access = running.enable(0).await;
    let headers = post_headers(&access);
    let create = r#"{"action":"create","name":"Work"}"#;
    let mut unauthenticated = TcpStream::connect(host(&access)).await.unwrap();
    unauthenticated
        .write_all(
            format!(
                "POST /api/topic-actions HTTP/1.1\r\nHost: {}\r\n{}Content-Length: {}\r\n\r\n{}",
                host(&access),
                headers,
                create.len(),
                create
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    timeout(
        Duration::from_secs(3),
        unauthenticated.read_to_end(&mut response),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(response.starts_with(b"HTTP/1.1 401"));
    assert!(
        post(
            &access,
            "/api/topic-actions",
            "Content-Type: application/json\r\n",
            create
        )
        .await
        .starts_with("HTTP/1.1 403")
    );
    for (headers, status) in [
        (
            "Origin: https://evil.example\r\nContent-Type: application/json\r\n".to_owned(),
            "403",
        ),
        (
            format!(
                "Origin: {}\r\nContent-Type: text/plain\r\n",
                access["url"].as_str().unwrap()
            ),
            "415",
        ),
        (format!("{}{}", post_headers(&access), auth(&access)), "401"),
    ] {
        let rejected = post(&access, "/api/topic-actions", &headers, create).await;
        assert!(
            rejected.starts_with(&format!("HTTP/1.1 {status}")),
            "{rejected}"
        );
    }
    let created = post(&access, "/api/topic-actions", &headers, create).await;
    assert!(created.starts_with("HTTP/1.1 200"), "{created}");
    let topic = json_body(&created)["reference"]["topic_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = uuid::Uuid::new_v4().to_string();
    let body = serde_json::json!({"action":"publish","topic_id":topic,"id":id,"body":"Hello <script>","parent":null,"mentions":[]});
    let mut spoof = body.clone();
    spoof["author"] = serde_json::json!({"agent_id":"inbox-other"});
    assert!(
        post(&access, "/api/topic-actions", &headers, &spoof.to_string())
            .await
            .starts_with("HTTP/1.1 400")
    );
    let published = post(&access, "/api/topic-actions", &headers, &body.to_string()).await;
    assert!(published.starts_with("HTTP/1.1 200"), "{published}");
    let published = json_body(&published);
    assert_eq!(published["post"]["author"]["agent_id"], "inbox-user");
    assert_eq!(published["duplicate"], false);
    let retried =
        json_body(&post(&access, "/api/topic-actions", &headers, &body.to_string()).await);
    assert_eq!(retried["duplicate"], true);
    assert_eq!(retried["post"]["id"], id);
    let archive = serde_json::json!({"action":"archive","topic_id":topic}).to_string();
    assert!(
        post(&access, "/api/topic-actions", &headers, &archive)
            .await
            .starts_with("HTTP/1.1 200")
    );
    let read = get(
        &access,
        &format!("/api/topics/{topic}/posts"),
        &auth(&access),
    )
    .await;
    assert!(read.starts_with("HTTP/1.1 200"), "{read}");
    assert_eq!(json_body(&read)["posts"].as_array().unwrap().len(), 1);
    for query in [
        "?cursor=garbage",
        "?limit=0",
        "?cursor=a&before=b",
        "?unexpected=x",
    ] {
        let result = get(
            &access,
            &format!("/api/topics/{topic}/posts{query}"),
            &auth(&access),
        )
        .await;
        assert!(result.starts_with("HTTP/1.1 400"), "{query}: {result}");
    }
    let restore = serde_json::json!({"action":"restore","topic_id":topic}).to_string();
    assert!(
        post(&access, "/api/topic-actions", &headers, &restore)
            .await
            .starts_with("HTTP/1.1 200")
    );
}
#[tokio::test]
async fn board_http_reads_controls_save_conditional_conflict_and_narrow_security() {
    use agent_bridge::{board, protocol::Request};
    let root = tempfile::tempdir().unwrap();
    let running = Running::start(root.path());
    let access = running.enable(0).await;
    let empty = json_body(&get(&access, "/api/boards/main", &auth(&access)).await);
    assert_eq!(empty["exists"], false);
    assert!(empty["last_updated"].is_null());
    assert_eq!(empty["last_updated_source"], "unknown");
    assert!(!root.path().join("boards").exists());
    for name in ["alpha", "beta"] {
        running
            .broker
            .request(Request::BoardCreate {
                board: name.parse().unwrap(),
            })
            .unwrap();
    }
    let alpha: board::Name = "alpha".parse().unwrap();
    let before = board_snapshot(&running, &alpha);
    running.broker.request(Request::NamedBoardUpdate { board: alpha.clone(), expected_revision: before.revision, edits: vec![board::Edit::Set { markdown: "# Title\n- [ ] task\n\n<!-- board-input: answer -->\n> old\n<!-- /board-input -->\nTail\n".into() }], actor: None }).unwrap();
    let listed = json_body(&get(&access, "/api/boards", &auth(&access)).await);
    assert_eq!(
        listed["boards"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["alpha", "beta", "main"]
    );
    let view = json_body(&get(&access, "/api/boards/alpha", &auth(&access)).await);
    assert_eq!(view["controls"].as_array().unwrap().len(), 2);
    assert!(view["last_updated"].is_i64());
    assert_eq!(view["last_updated_source"], "file_mtime");
    assert_eq!(listed["boards"][0]["last_updated"], view["last_updated"]);
    let beta_time = listed["boards"][1]["last_updated"].clone();
    let conditional = json_body(
        &get(
            &access,
            &format!(
                "/api/boards/alpha?if_revision={}",
                view["revision"].as_str().unwrap()
            ),
            &auth(&access),
        )
        .await,
    );
    assert_eq!(conditional["unchanged"], true);
    assert_eq!(conditional["last_updated"], view["last_updated"]);
    assert_eq!(
        conditional["last_updated_source"],
        view["last_updated_source"]
    );
    assert!(conditional.get("markdown").is_none());
    let dto = serde_json::json!({"board":"alpha","expected_revision":view["revision"],"control_id":view["controls"][0]["id"],"value":{"type":"checkbox","checked":true}}).to_string();
    for (headers, status) in [
        ("Content-Type: application/json\r\n".into(), 403),
        (
            "Origin: null\r\nContent-Type: application/json\r\n".into(),
            403,
        ),
        (
            format!(
                "Origin: {}\r\nContent-Type: text/plain\r\n",
                access["url"].as_str().unwrap()
            ),
            415,
        ),
        (
            format!("{}Content-Encoding: gzip\r\n", post_headers(&access)),
            415,
        ),
        (
            format!(
                "{}Origin: {}\r\n",
                post_headers(&access),
                access["url"].as_str().unwrap()
            ),
            403,
        ),
        (
            format!(
                "{}Content-Type: application/json\r\n",
                post_headers(&access)
            ),
            415,
        ),
    ] {
        assert!(
            post(&access, "/api/boards/alpha/responses", &headers, &dto)
                .await
                .starts_with(&format!("HTTP/1.1 {status}"))
        );
    }
    let valid = post(
        &access,
        "/api/boards/alpha/responses",
        &post_headers(&access),
        &dto,
    )
    .await;
    assert!(valid.starts_with("HTTP/1.1 200"), "{valid}");
    let saved = json_body(&valid)["board"].clone();
    assert!(saved["last_updated"].is_i64());
    let noop = serde_json::json!({"board":"alpha","expected_revision":saved["revision"],"control_id":saved["controls"][0]["id"],"value":{"type":"checkbox","checked":true}}).to_string();
    let noop = json_body(
        &post(
            &access,
            "/api/boards/alpha/responses",
            &post_headers(&access),
            &noop,
        )
        .await,
    );
    assert_eq!(noop["unchanged"], true);
    assert_eq!(noop["board"]["last_updated"], saved["last_updated"]);
    let beta = json_body(&get(&access, "/api/boards/beta", &auth(&access)).await);
    assert_eq!(beta["last_updated"], beta_time);
    assert!(
        board::read_named(root.path(), &alpha)
            .unwrap()
            .markdown
            .contains("[x] task")
    );
    let stale = post(
        &access,
        "/api/boards/alpha/responses",
        &post_headers(&access),
        &dto,
    )
    .await;
    assert!(stale.starts_with("HTTP/1.1 409"));
    assert_eq!(json_body(&stale)["board"]["name"], "alpha");
    assert_eq!(
        json_body(&stale)["board"]["last_updated"],
        saved["last_updated"]
    );
    assert!(
        post(&access, "/api/messages", &post_headers(&access), "{}")
            .await
            .starts_with("HTTP/1.1 405")
    );
    assert!(
        get(&access, "/api/boards/no-such-board", &auth(&access))
            .await
            .starts_with("HTTP/1.1 404")
    );
    assert!(
        get(&access, "/api/boards/ALPHA", &auth(&access))
            .await
            .starts_with("HTTP/1.1 400")
    );
    assert!(
        get(&access, "/api/boards/alpha?unknown=1", &auth(&access))
            .await
            .starts_with("HTTP/1.1 400")
    );
    assert!(
        get(&access, "/api/boards/alpha", "")
            .await
            .starts_with("HTTP/1.1 401")
    );
}

#[tokio::test]
async fn body_bounds_malformed_unknown_fields_and_revocation_never_change_board() {
    let root = tempfile::tempdir().unwrap();
    let running = Running::start(root.path());
    let access = running.enable(0).await;
    for body in ["{", "{\"board\":\"main\",\"extra\":true}"] {
        assert!(
            post(
                &access,
                "/api/boards/main/responses",
                &post_headers(&access),
                body
            )
            .await
            .starts_with("HTTP/1.1 400")
        );
    }
    let mut socket = TcpStream::connect(host(&access)).await.unwrap();
    socket.write_all(format!("POST /api/boards/main/responses HTTP/1.1\r\nHost: {}\r\n{}{}Content-Length: 524289\r\n\r\n", host(&access), auth(&access), post_headers(&access)).as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    timeout(Duration::from_secs(2), socket.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(bytes.starts_with(b"HTTP/1.1 413"));
    let mut body = TcpStream::connect(host(&access)).await.unwrap();
    body.write_all(format!("POST /api/boards/main/responses HTTP/1.1\r\nHost: {}\r\n{}{}Content-Length: 1000\r\n\r\n{{", host(&access), auth(&access), post_headers(&access)).as_bytes()).await.unwrap();
    running.broker.request(Request::MonitorDisable).unwrap();
    let _ = body.write_all(&[b' '; 999]).await;
    let mut rest = Vec::new();
    let _ = timeout(Duration::from_secs(2), body.read_to_end(&mut rest)).await;
    assert!(!root.path().join("boards").exists());
}

#[tokio::test]
async fn http_response_notifies_only_the_selected_boards_native_subscriber() {
    use agent_bridge::{board, protocol::Provider};
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::Builder::new()
        .prefix("ab-http-notice-")
        .tempdir_in("/tmp")
        .unwrap();
    let running = Running::start(root.path());
    let access = running.enable(0).await;
    let mut listeners = Vec::new();
    for name in ["alpha", "beta"] {
        let board: board::Name = name.parse().unwrap();
        running
            .broker
            .request(Request::BoardCreate {
                board: board.clone(),
            })
            .unwrap();
        let snapshot = board_snapshot(&running, &board);
        running
            .broker
            .request(Request::NamedBoardUpdate {
                board: board.clone(),
                expected_revision: snapshot.revision,
                edits: vec![board::Edit::Set {
                    markdown: "- [ ] native notice\n".into(),
                }],
                actor: None,
            })
            .unwrap();
        let path = root.path().join(format!("{name}.sock"));
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        running
            .broker
            .request(Request::Register {
                name: name.into(),
                provider: Provider::Claude,
                session: session.clone(),
                replace: false,
            })
            .unwrap();
        running
            .broker
            .request(Request::ClaudeBind {
                session,
                endpoint: path.to_string_lossy().into_owned(),
            })
            .unwrap();
        running
            .broker
            .request(Request::Subscribe {
                board_revisions: std::collections::BTreeMap::from([(
                    board.clone(),
                    board_snapshot(&running, &board).revision,
                )]),
                events: vec![agent_bridge::protocol::EventSelector::BoardChanged { board }],
                agent: name.into(),
            })
            .unwrap();
        listeners.push(listener);
    }
    let alpha = json_body(&get(&access, "/api/boards/alpha", &auth(&access)).await);
    // Reads and an already-desired checkbox state generate no native notice.
    let noop = serde_json::json!({"board":"alpha","expected_revision":alpha["revision"],"control_id":alpha["controls"][0]["id"],"value":{"type":"checkbox","checked":false}}).to_string();
    let response = post(
        &access,
        "/api/boards/alpha/responses",
        &post_headers(&access),
        &noop,
    )
    .await;
    assert_eq!(json_body(&response)["unchanged"], true);
    assert!(
        timeout(Duration::from_millis(500), listeners[0].accept())
            .await
            .is_err()
    );
    let save = noop.replace("\"checked\":false", "\"checked\":true");
    assert!(
        post(
            &access,
            "/api/boards/alpha/responses",
            &post_headers(&access),
            &save
        )
        .await
        .starts_with("HTTP/1.1 200")
    );
    let (mut socket, _) = timeout(Duration::from_secs(8), listeners[0].accept())
        .await
        .unwrap()
        .unwrap();
    let mut bytes = Vec::new();
    timeout(Duration::from_secs(2), socket.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let notice: Value = serde_json::from_slice(&bytes).unwrap();
    let text = notice["message"]["content"].as_str().unwrap();
    assert!(text.contains("local board changed, not user approval"));
    assert!(text.contains("\"board\":\"alpha\""));
    assert!(!text.contains("cross-session-message"));
    assert!(
        timeout(Duration::from_millis(300), listeners[1].accept())
            .await
            .is_err()
    );
    assert!(
        timeout(Duration::from_millis(300), listeners[0].accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn slow_bodies_have_a_deadline_and_share_a_bounded_write_budget() {
    let root = tempfile::tempdir().unwrap();
    let running = Running::start(root.path());
    let access = running.enable(0).await;
    let mut sockets = Vec::new();
    for _ in 0..5 {
        let mut socket = TcpStream::connect(host(&access)).await.unwrap();
        socket.write_all(format!("POST /api/boards/main/responses HTTP/1.1\r\nHost: {}\r\n{}{}Content-Length: 1000\r\n\r\n{{", host(&access), auth(&access), post_headers(&access)).as_bytes()).await.unwrap();
        sockets.push(socket);
    }
    // This read must remain responsive while response bodies are incomplete.
    assert!(
        get(&access, "/api/status", &auth(&access))
            .await
            .starts_with("HTTP/1.1 200")
    );
    let mut timed_out = 0;
    let mut denied = 0;
    for mut socket in sockets {
        let mut bytes = Vec::new();
        timeout(Duration::from_secs(7), socket.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        if bytes.starts_with(b"HTTP/1.1 408") {
            timed_out += 1;
        } else {
            assert!(bytes.starts_with(b"HTTP/1.1 429"));
            denied += 1;
        }
    }
    assert_eq!((timed_out, denied), (4, 1));
    assert!(!root.path().join("boards").exists());
}

#[tokio::test]
async fn archive_http_actions_share_security_and_emit_readonly_lifecycle_events() {
    use agent_bridge::{
        board::{Edit, Name},
        protocol::Request,
    };
    let root = tempfile::tempdir().unwrap();
    let running = Running::start(root.path());
    let name: Name = "archive-test".parse().unwrap();
    let created = running
        .broker
        .request(Request::BoardCreate {
            board: name.clone(),
        })
        .unwrap();
    let updated = running
        .broker
        .request(Request::board_update(
            name.clone(),
            created["revision"].as_str().unwrap().into(),
            vec![Edit::Set {
                markdown: "- [ ] Keep exact\n".into(),
            }],
            None,
        ))
        .unwrap();
    assert_eq!(updated["applied"], true);
    let before = board_snapshot(&running, &name);
    let access = running.enable(0).await;
    assert!(
        get(&access, "/api/archived-boards", "")
            .await
            .starts_with("HTTP/1.1 401")
    );
    let payload = serde_json::json!({"board":name,"expected_revision":before.revision}).to_string();
    let mut wrong = access.clone();
    wrong["access_token"] = Value::String("wrong-token".into());
    assert!(
        post(
            &wrong,
            "/api/boards/archive-test/archive",
            &post_headers(&access),
            &payload
        )
        .await
        .starts_with("HTTP/1.1 401")
    );
    assert!(
        post(
            &access,
            "/api/boards/archive-test/archive",
            "Content-Type: application/json\r\n",
            &payload
        )
        .await
        .starts_with("HTTP/1.1 403")
    );
    let mismatch =
        serde_json::json!({"board":"other","expected_revision":before.revision}).to_string();
    assert!(
        post(
            &access,
            "/api/boards/archive-test/archive",
            &post_headers(&access),
            &mismatch
        )
        .await
        .starts_with("HTTP/1.1 400")
    );
    let forged =
        serde_json::json!({"board":name,"expected_revision":before.revision,"archived_at":0})
            .to_string();
    assert!(
        post(
            &access,
            "/api/boards/archive-test/archive",
            &post_headers(&access),
            &forged
        )
        .await
        .starts_with("HTTP/1.1 400")
    );
    let main = board_snapshot(&running, &Name::default());
    let protected =
        serde_json::json!({"board":"main","expected_revision":main.revision}).to_string();
    assert!(
        post(
            &access,
            "/api/boards/main/archive",
            &post_headers(&access),
            &protected
        )
        .await
        .starts_with("HTTP/1.1 422")
    );
    let mut live = connect(&access, "/api/boards/archive-test/events", &auth(&access)).await;
    let initial = timeout(Duration::from_secs(3), board_event(&mut live))
        .await
        .unwrap();
    assert_eq!(initial["read_only"], false);
    let response = post(
        &access,
        "/api/boards/archive-test/archive",
        &post_headers(&access),
        &payload,
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"));
    let saved = json_body(&response)["board"].clone();
    assert_eq!(saved["state"], "archived");
    assert_eq!(saved["read_only"], true);
    let event = timeout(Duration::from_secs(3), board_event(&mut live))
        .await
        .unwrap();
    assert_eq!(event["read_only"], true);
    assert_eq!(event["state"], "archived");
    assert_eq!(event["revision"], saved["revision"]);
    assert_eq!(event["incarnation"], saved["incarnation"]);
    let active = json_body(&get(&access, "/api/boards", &auth(&access)).await);
    assert!(
        !active["boards"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == "archive-test")
    );
    let archived = json_body(&get(&access, "/api/archived-boards", &auth(&access)).await);
    assert_eq!(archived["boards"][0]["name"], "archive-test");
    assert!(archived["boards"][0].get("markdown").is_none());
    let conditional = json_body(
        &get(
            &access,
            &format!(
                "/api/boards/archive-test?if_revision={}",
                saved["revision"].as_str().unwrap()
            ),
            &auth(&access),
        )
        .await,
    );
    assert_eq!(conditional["unchanged"], true);
    assert_eq!(conditional["read_only"], true);
    assert_eq!(conditional["expires_at"], saved["expires_at"]);
    let restore =
        serde_json::json!({"board":name,"expected_revision":saved["revision"]}).to_string();
    let response = post(
        &access,
        "/api/boards/archive-test/restore",
        &post_headers(&access),
        &restore,
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"));
    let restored = json_body(&response)["board"].clone();
    assert_eq!(restored["state"], "active");
    assert_eq!(restored["markdown"], before.markdown);
    assert_eq!(
        restored["last_updated"],
        serde_json::json!(before.last_updated)
    );
    assert_ne!(
        restored["lifecycle_generation"],
        initial["lifecycle_generation"]
    );
    let event = timeout(Duration::from_secs(3), board_event(&mut live))
        .await
        .unwrap();
    assert_eq!(event["read_only"], false);
    assert_eq!(event["revision"], restored["revision"]);
    assert!(
        post(
            &access,
            "/api/boards/archive-test/archive",
            &post_headers(&access),
            &payload
        )
        .await
        .starts_with("HTTP/1.1 409")
    );
    drop(live);
}

#[tokio::test]
async fn decoded_text_byte_limit_is_enforced_without_mutation() {
    let root = tempfile::tempdir().unwrap();
    let running = Running::start(root.path());
    let access = running.enable(0).await;
    let oversized = serde_json::json!({"board":"main","expected_revision":"a".repeat(64),"control_id":"b".repeat(64),"value":{"type":"text","text":"界".repeat(21846)}}).to_string();
    assert!(
        post(
            &access,
            "/api/boards/main/responses",
            &post_headers(&access),
            &oversized
        )
        .await
        .starts_with("HTTP/1.1 413")
    );
    assert!(!root.path().join("boards").exists());
}

#[tokio::test]
async fn chunked_body_without_length_is_still_bounded() {
    let root = tempfile::tempdir().unwrap();
    let running = Running::start(root.path());
    let access = running.enable(0).await;
    let mut socket = TcpStream::connect(host(&access)).await.unwrap();
    socket.write_all(format!("POST /api/boards/main/responses HTTP/1.1\r\nHost: {}\r\n{}{}Transfer-Encoding: chunked\r\n\r\n", host(&access), auth(&access), post_headers(&access)).as_bytes()).await.unwrap();
    let chunk = vec![b' '; 65536];
    for _ in 0..8 {
        socket.write_all(b"10000\r\n").await.unwrap();
        socket.write_all(&chunk).await.unwrap();
        socket.write_all(b"\r\n").await.unwrap();
    }
    socket.write_all(b"1\r\nx\r\n0\r\n\r\n").await.unwrap();
    let mut bytes = Vec::new();
    let result = timeout(Duration::from_secs(3), socket.read_to_end(&mut bytes))
        .await
        .unwrap();
    if let Err(error) = result {
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
    }
    assert!(
        bytes.starts_with(b"HTTP/1.1 413"),
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    assert!(!root.path().join("boards").exists());
}

async fn get_bytes(access: &Value, path: &str, headers: &str) -> Vec<u8> {
    let mut stream = connect(access, path, headers).await;
    let mut bytes = Vec::new();
    timeout(Duration::from_secs(3), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    bytes
}
fn split_response(bytes: &[u8]) -> (String, &[u8]) {
    let at = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("complete response head");
    (
        String::from_utf8(bytes[..at].to_vec()).unwrap(),
        &bytes[at + 4..],
    )
}

#[tokio::test]
async fn web_assets_are_served_with_exact_types_and_a_tight_policy() {
    const HTML: &str = "text/html; charset=utf-8";
    const CSS: &str = "text/css; charset=utf-8";
    const JS: &str = "text/javascript; charset=utf-8";
    const WOFF2: &str = "font/woff2";
    // Fonts and images from this server only. Scripts and styles stay same-origin with no inline
    // allowance, which is what keeps injected agent-authored content inert.
    let policy = "content-security-policy: default-src 'none'; script-src 'self'; style-src 'self'; font-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
    let root = tempfile::Builder::new()
        .prefix("ab-assets-")
        .tempdir_in("/tmp")
        .unwrap();
    let running = Running::start(root.path());
    let access = running.enable(0).await;
    let assets = [
        ("/", HTML),
        ("/style.css", CSS),
        ("/theme-classic.css", CSS),
        ("/theme-sticker.css", CSS),
        ("/theme.js", JS),
        ("/util.js", JS),
        ("/markdown.js", JS),
        ("/boards.js", JS),
        ("/topics.js", JS),
        ("/app.js", JS),
        ("/favicon.svg", "image/svg+xml"),
        ("/fonts/bricolage-var.woff2", WOFF2),
        ("/fonts/space-mono-400.woff2", WOFF2),
        ("/fonts/space-mono-700.woff2", WOFF2),
    ];
    for (path, kind) in assets {
        let bytes = get_bytes(&access, path, "").await;
        let (head, body) = split_response(&bytes);
        assert!(head.starts_with("HTTP/1.1 200"), "{path}: {head}");
        assert!(
            head.contains(&format!("content-type: {kind}")),
            "{path}: {head}"
        );
        assert!(head.contains(policy), "{path}: {head}");
        assert!(head.contains("x-content-type-options: nosniff"), "{path}");
        assert!(head.contains("cache-control: no-store"), "{path}");
        assert!(!body.is_empty(), "{path} is empty");
        assert!(
            !String::from_utf8_lossy(body).contains(key(&access)),
            "{path} must never embed the access token"
        );
        if kind == WOFF2 {
            assert_eq!(&body[..4], b"wOF2", "{path} is not a WOFF2 file");
        }
        // Cross-site and cross-origin loads are refused for every asset, as for the API.
        for extra in [
            "Sec-Fetch-Site: cross-site\r\n",
            "Origin: https://evil.example\r\n",
        ] {
            let refused = get_bytes(&access, path, extra).await;
            assert!(refused.starts_with(b"HTTP/1.1 403"), "{path} {extra}");
        }
    }
    assert!(
        !policy.contains("unsafe-") && !policy.contains("http:") && !policy.contains("data:"),
        "the policy must not allow inline code or remote hosts"
    );
    // Only listed files exist: no theme or font name is interpreted as a path.
    for path in [
        "/fonts/missing.woff2",
        "/theme-missing.css",
        "/theme-../style.css",
        "/fonts/../app.js",
        "/assets/web/app.js",
        "/assets/monitor/app.js",
    ] {
        let bytes = get_bytes(&access, path, "").await;
        assert!(
            bytes.starts_with(b"HTTP/1.1 404") || bytes.starts_with(b"HTTP/1.1 400"),
            "{path}: {}",
            String::from_utf8_lossy(&bytes[..bytes.len().min(80)])
        );
    }
}

/// The policy forbids inline scripts and styles, so the interface must never depend on them. A page
/// or script that did would work in no browser, but only a test notices before a release does.
#[tokio::test]
async fn web_interface_never_relies_on_inline_code_or_styles() {
    let root = tempfile::Builder::new()
        .prefix("ab-inline-")
        .tempdir_in("/tmp")
        .unwrap();
    let running = Running::start(root.path());
    let access = running.enable(0).await;
    let page_bytes = get_bytes(&access, "/", "").await;
    let (_, page) = split_response(&page_bytes);
    let html = String::from_utf8(page.to_vec()).unwrap();
    assert!(
        !html.contains("<style"),
        "inline <style> is blocked by the policy"
    );
    assert!(
        !html.contains(" style=\""),
        "style attributes are blocked by the policy"
    );
    for handler in [
        " onclick=",
        " onload=",
        " onerror=",
        " oninput=",
        " onsubmit=",
        "javascript:",
    ] {
        assert!(!html.contains(handler), "inline handler {handler}");
    }
    assert_eq!(
        html.matches("<script").count(),
        html.matches("<script src=\"/").count(),
        "every script must be an external same-origin file"
    );
    for script in [
        "/theme.js",
        "/util.js",
        "/markdown.js",
        "/boards.js",
        "/topics.js",
        "/app.js",
    ] {
        let bytes = get_bytes(&access, script, "").await;
        let (_, body) = split_response(&bytes);
        let code = String::from_utf8(body.to_vec()).unwrap();
        for forbidden in [
            "innerHTML",
            "outerHTML",
            "insertAdjacentHTML",
            "document.write",
            "eval(",
            "new Function",
            "setAttribute(\"style\"",
            "setAttribute('style'",
        ] {
            assert!(
                !code.contains(forbidden),
                "{script} uses {forbidden}, which the policy or the untrusted-text rule forbids"
            );
        }
    }
}

#[tokio::test]
async fn topic_posts_carry_parsed_body_nodes_and_degrade_to_plain_text() {
    let root = tempfile::Builder::new()
        .prefix("ab-topic-md-")
        .tempdir_in("/tmp")
        .unwrap();
    let running = Running::start(root.path());
    let access = running.enable(0).await;
    let headers = post_headers(&access);
    let created = post(
        &access,
        "/api/topic-actions",
        &headers,
        r#"{"action":"create","name":"Docs"}"#,
    )
    .await;
    let topic = json_body(&created)["reference"]["topic_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let rich = "# Title\n\n- a\n- b\n\n```sh\nls\n```\n\n[ok](https://example.com) [bad](javascript:alert(1)) <script>alert(1)</script>\n";
    let complex = "a\n\n".repeat(5000);
    for body in [rich.to_owned(), complex.clone()] {
        let payload = serde_json::json!({
            "action": "publish", "topic_id": topic, "id": uuid::Uuid::new_v4().to_string(),
            "body": body, "parent": null, "mentions": []
        })
        .to_string();
        let published = post(&access, "/api/topic-actions", &headers, &payload).await;
        assert!(published.starts_with("HTTP/1.1 200"), "{published}");
    }
    let read = get(
        &access,
        &format!("/api/topics/{topic}/posts"),
        &auth(&access),
    )
    .await;
    assert!(read.starts_with("HTTP/1.1 200"), "{read}");
    let page = json_body(&read);
    let posts = page["posts"].as_array().unwrap();
    assert_eq!(posts.len(), 2);
    // The stored body is unchanged and nodes are additive.
    assert_eq!(posts[0]["body"], rich);
    let nodes = posts[0]["body_nodes"].to_string();
    for tag in ["h1", "ul", "li", "pre", "a"] {
        assert!(
            nodes.contains(&format!("\"tag\":\"{tag}\"")),
            "{tag}: {nodes}"
        );
    }
    assert!(nodes.contains("https://example.com"));
    assert!(
        !nodes.contains("javascript:"),
        "unsafe link survived: {nodes}"
    );
    assert!(!nodes.contains("\"tag\":\"script\""), "{nodes}");
    assert!(
        nodes.contains("\"text\":\"<script>\""),
        "raw HTML must be text: {nodes}"
    );
    // Too complex to render safely: plain text only, and the stored body is still delivered.
    assert!(posts[1]["body_nodes"].is_null());
    assert_eq!(posts[1]["body"], complex);
}

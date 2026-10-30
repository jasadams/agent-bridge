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
    assert!(running.broker.request(Request::List { all: false }).is_ok());
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
    use agent_bridge::{
        board,
        protocol::{Agent, Provider},
    };
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
        running
            .broker
            .request(Request::Register {
                agent: Agent {
                    name: name.into(),
                    provider: Provider::Claude,
                    session: uuid::Uuid::new_v4().to_string(),
                    endpoint: path.to_str().unwrap().into(),
                    enabled: true,
                },
                replace: false,
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

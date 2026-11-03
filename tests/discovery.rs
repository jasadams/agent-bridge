use agent_bridge::{
    broker::{BoundServer, Broker},
    clients::{ConnectorState, NativeState, StateError},
    discovery::{self, State},
    protocol::{Provider, Request},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{
    net::UnixListener,
    time::{sleep, timeout},
};
use tokio_tungstenite::tungstenite::Message;

fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
type NativeSocket = tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>;
async fn native_recv(socket: &mut NativeSocket) -> Value {
    let message = timeout(Duration::from_secs(3), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}
async fn native_send(socket: &mut NativeSocket, value: Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}
async fn wait_for(state: &State, check: impl Fn(&[NativeState]) -> bool) {
    timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(rows) = state.list(7)
                && check(&rows)
            {
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

struct Fixture {
    _dir: tempfile::TempDir,
    bound: BoundServer,
    state: State,
    observer: tokio::task::JoinHandle<anyhow::Result<()>>,
    socket: NativeSocket,
}
async fn fixture(wrong_home: bool) -> Fixture {
    let dir = tempfile::Builder::new()
        .prefix("abd-")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let path = dir.path().join("native.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let bound = Broker::bind(dir.path().join("state"), "/missing/codex".into()).unwrap();
    let state = State::default();
    let (broker, reader, native_home) = (bound.broker.clone(), state.clone(), home.clone());
    let observer = tokio::spawn(async move {
        discovery::observe_connection(&broker, &path, &native_home, &reader).await
    });
    let (stream, _) = listener.accept().await.unwrap();
    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
    let init = native_recv(&mut socket).await;
    assert_eq!(init["method"], "initialize");
    native_send(&mut socket, json!({"id":init["id"],"result":{"codexHome":if wrong_home { dir.path().to_path_buf() } else { home }}})).await;
    if !wrong_home {
        assert_eq!(native_recv(&mut socket).await["method"], "initialized");
    }
    Fixture {
        _dir: dir,
        bound,
        state,
        observer,
        socket,
    }
}
async fn read_reply(socket: &mut NativeSocket, request: &Value, name: &str, status: &str) {
    assert_eq!(request["method"], "thread/read");
    native_send(socket, json!({"id":request["id"],"result":{"thread":{"id":request["params"]["threadId"],"name":name,"cwd":"/native","status":{"type":status}}}})).await;
}

#[test]
fn disconnected_and_invalid_exact_ids_are_errors_not_empty_native_results() {
    let state = State::default();
    assert_eq!(state.provider(), Provider::Codex);
    assert!(matches!(state.list(7), Err(StateError::Unavailable(_))));
    assert!(matches!(state.get(&id()), Err(StateError::Unavailable(_))));
    assert!(matches!(
        state.get("codex:title"),
        Err(StateError::Incomplete(_))
    ));
}

#[tokio::test]
async fn observer_provides_native_state_without_registration_or_database_writes() {
    let mut f = fixture(false).await;
    let before = f.bound.broker.request(Request::List { all: true }).unwrap();
    let first = native_recv(&mut f.socket).await;
    assert_eq!(first["method"], "thread/loaded/list");
    assert!(matches!(f.state.list(7), Err(StateError::Incomplete(_))));
    let session = id();
    native_send(
        &mut f.socket,
        json!({"id":first["id"],"result":{"data":[session],"nextCursor":null}}),
    )
    .await;
    let read = native_recv(&mut f.socket).await;
    assert!(matches!(
        f.state.get(&session),
        Err(StateError::Incomplete(_))
    ));
    read_reply(&mut f.socket, &read, "same-name", "idle").await;
    wait_for(&f.state, |rows| rows.len() == 1).await;
    let row = f.state.get(&session).unwrap().unwrap();
    assert!(f.state.get(&id()).unwrap().is_none());
    assert_eq!(row.directory.as_deref(), Some("/native"));
    assert_eq!(row.status.as_deref(), Some("idle"));
    assert!(row.updated_ms.is_none() && row.transport_endpoint.is_none());
    native_send(
        &mut f.socket,
        json!({"method":"thread/name/updated","params":{"threadId":session}}),
    )
    .await;
    let read = native_recv(&mut f.socket).await;
    read_reply(&mut f.socket, &read, "renamed", "active").await;
    wait_for(&f.state, |rows| {
        rows[0].display_name.as_deref() == Some("renamed")
    })
    .await;
    assert_eq!(f.state.list(7).unwrap(), f.state.list(30).unwrap());
    assert_eq!(
        f.bound.broker.request(Request::List { all: true }).unwrap(),
        before
    );
    f.socket.close(None).await.unwrap();
    assert!(f.observer.await.unwrap().is_err());
    assert!(matches!(f.state.list(7), Err(StateError::Unavailable(_))));
}

#[tokio::test]
async fn complete_paginated_baseline_preserves_live_events_and_ignores_reads_preceding_closure() {
    let mut f = fixture(false).await;
    let (keep, joining, closed) = (id(), id(), id());
    let first = native_recv(&mut f.socket).await;
    native_send(
        &mut f.socket,
        json!({"id":first["id"],"result":{"data":[keep],"nextCursor":"next"}}),
    )
    .await;
    let read = native_recv(&mut f.socket).await;
    let second = native_recv(&mut f.socket).await;
    assert_eq!(second["params"]["cursor"], "next");
    read_reply(&mut f.socket, &read, "same", "idle").await;
    native_send(
        &mut f.socket,
        json!({"method":"thread/closed","params":{"threadId":closed}}),
    )
    .await;
    native_send(
        &mut f.socket,
        json!({"method":"thread/started","params":{"thread":{"id":joining}}}),
    )
    .await;
    let read = native_recv(&mut f.socket).await;
    read_reply(&mut f.socket, &read, "same", "active").await;
    assert!(matches!(f.state.list(7), Err(StateError::Incomplete(_))));
    native_send(
        &mut f.socket,
        json!({"id":second["id"],"result":{"data":[closed],"nextCursor":null}}),
    )
    .await;
    wait_for(&f.state, |rows| rows.len() == 2).await;
    assert!(f.state.get(&closed).unwrap().is_none());
    native_send(
        &mut f.socket,
        json!({"method":"thread/name/updated","params":{"threadId":keep}}),
    )
    .await;
    let stale = native_recv(&mut f.socket).await;
    native_send(
        &mut f.socket,
        json!({"method":"thread/closed","params":{"threadId":keep}}),
    )
    .await;
    wait_for(&f.state, |rows| rows.len() == 1).await;
    read_reply(&mut f.socket, &stale, "stale", "idle").await;
    native_send(
        &mut f.socket,
        json!({"method":"thread/name/updated","params":{"threadId":joining}}),
    )
    .await;
    let read = native_recv(&mut f.socket).await; // Barrier after ignored stale response.
    read_reply(&mut f.socket, &read, "survivor", "idle").await;
    wait_for(&f.state, |rows| {
        rows[0].display_name.as_deref() == Some("survivor")
    })
    .await;
    assert!(f.state.get(&keep).unwrap().is_none());
    f.observer.abort();
    let _ = f.observer.await;
    assert!(matches!(f.state.list(7), Err(StateError::Unavailable(_))));
}

#[tokio::test]
async fn not_loaded_and_read_errors_do_not_create_false_empty_success() {
    for read_error in [false, true] {
        let mut f = fixture(false).await;
        let session = id();
        let list = native_recv(&mut f.socket).await;
        native_send(
            &mut f.socket,
            json!({"id":list["id"],"result":{"data":[session],"nextCursor":null}}),
        )
        .await;
        let read = native_recv(&mut f.socket).await;
        if read_error {
            native_send(&mut f.socket, json!({"id":read["id"],"error":{"code":-1}})).await;
        } else {
            read_reply(&mut f.socket, &read, "saved", "notLoaded").await;
        }
        // Native ping/pong is a processing barrier without adding a pending read.
        f.socket
            .send(Message::Ping(Default::default()))
            .await
            .unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(3), f.socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            Message::Pong(_)
        ));
        if read_error {
            assert!(matches!(f.state.list(7), Err(StateError::Incomplete(_))));
        } else {
            assert!(f.state.list(7).unwrap().is_empty());
        }
        native_send(
            &mut f.socket,
            json!({"method":"thread/name/updated","params":{"threadId":session}}),
        )
        .await;
        let read = native_recv(&mut f.socket).await; // Previous response processed.
        assert!(matches!(f.state.list(7), Err(StateError::Incomplete(_))));
        read_reply(&mut f.socket, &read, "loaded", "idle").await;
        wait_for(&f.state, |rows| rows.len() == 1).await;
        native_send(&mut f.socket, json!({"method":"thread/status/changed","params":{"threadId":session,"status":{"type":"notLoaded"}}})).await;
        wait_for(&f.state, |rows| rows.is_empty()).await;
        assert!(f.state.get(&session).unwrap().is_none());
        f.observer.abort();
        let _ = f.observer.await;
    }
}

#[tokio::test]
async fn failed_malformed_and_repeated_cursor_pages_never_produce_a_complete_directory() {
    for kind in 0..5 {
        let mut f = fixture(false).await;
        let first = native_recv(&mut f.socket).await;
        native_send(
            &mut f.socket,
            json!({"id":first["id"],"result":{"data":[],"nextCursor":"next"}}),
        )
        .await;
        let second = native_recv(&mut f.socket).await;
        assert!(matches!(f.state.list(7), Err(StateError::Incomplete(_))));
        let response = match kind {
            0 => json!({"error":{"code":-1}}),
            1 => json!({"result":{"nextCursor":null}}),
            2 => json!({"result":{"data":[],"nextCursor":"next"}}),
            3 => json!({"result":{"data":["not-a-uuid"],"nextCursor":null}}),
            _ => json!({"result":{"data":[],"nextCursor":7}}),
        };
        let mut response = response;
        response["id"] = second["id"].clone();
        native_send(&mut f.socket, response).await;
        assert!(f.observer.await.unwrap().is_err());
        assert!(matches!(f.state.list(7), Err(StateError::Unavailable(_))));
    }
    let f = fixture(true).await;
    assert!(f.observer.await.unwrap().is_err());
    assert!(matches!(f.state.list(7), Err(StateError::Unavailable(_))));
}

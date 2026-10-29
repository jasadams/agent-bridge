use agent_bridge::{broker::Broker, db::Db, discovery, protocol::Request};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{
    net::UnixListener,
    time::{sleep, timeout},
};

fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[test]
fn native_aliases_follow_renames_but_messages_stay_pinned_and_duplicate_names_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open(&dir.path().join("db")).unwrap();
    let first = id();
    db.observe_codex(&first, Some("Native agent with spaces"), true)
        .unwrap();
    let mid = id();
    let sent = db
        .send(&mid, "user", "Native agent with spaces", "first", &None)
        .unwrap();
    assert_eq!(sent.recipient_session, first);
    db.observe_codex(&first, Some("Renamed agent"), true)
        .unwrap();
    assert!(db.resolve_agent("Native agent with spaces").is_err());
    assert_eq!(db.resolve_agent("Renamed agent").unwrap().session, first);
    assert_eq!(db.get(&mid).unwrap().recipient_session, first);
    let second = id();
    db.observe_codex(&second, Some("Renamed agent"), true)
        .unwrap();
    assert!(db.resolve_agent("Renamed agent").is_err());
    db.codex_offline(Some(&second)).unwrap();
    assert_eq!(db.resolve_agent("Renamed agent").unwrap().session, first);
    db.codex_offline(None).unwrap();
    assert!(db.resolve_agent("Renamed agent").is_err());
    assert_eq!(db.get(&mid).unwrap().state, "queued");
}

async fn wait_for(broker: &Broker, check: impl Fn(&Value) -> bool) {
    timeout(Duration::from_secs(3), async {
        loop {
            let agents = broker.request(Request::List { all: true }).unwrap();
            if check(&agents) {
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn observer_discovers_loaded_sessions_and_follows_native_events_without_resuming_or_starting_them()
 {
    let dir = tempfile::Builder::new()
        .prefix("abd-")
        .tempdir_in("/tmp")
        .unwrap();
    let home = dir.path().join("codex-home");
    std::fs::create_dir(&home).unwrap();
    let socket = dir.path().join("native.sock");
    let native = UnixListener::bind(&socket).unwrap();
    let bound = Broker::bind(dir.path().join("state"), "/missing/codex".into()).unwrap();
    let broker = bound.broker.clone();
    let observer =
        tokio::spawn(async move { discovery::observe_connection(&broker, &socket, &home).await });
    let (stream, _) = native.accept().await.unwrap();
    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
    async fn recv(ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>) -> Value {
        let m = timeout(Duration::from_secs(3), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(m.to_text().unwrap()).unwrap()
    }
    async fn send(ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>, v: Value) {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            v.to_string().into(),
        ))
        .await
        .unwrap();
    }
    let init = recv(&mut ws).await;
    assert_eq!(init["method"], "initialize");
    send(
        &mut ws,
        json!({"id":init["id"],"result":{"codexHome":dir.path().join("codex-home")}}),
    )
    .await;
    assert_eq!(recv(&mut ws).await["method"], "initialized");
    let list = recv(&mut ws).await;
    assert_eq!(list["method"], "thread/loaded/list");
    let session = id();
    send(
        &mut ws,
        json!({"id":list["id"],"result":{"data":[session],"nextCursor":null}}),
    )
    .await;
    let read = recv(&mut ws).await;
    assert_eq!(read["method"], "thread/read");
    assert_eq!(read["params"]["threadId"], session);
    send(&mut ws,json!({"id":read["id"],"result":{"thread":{"id":session,"name":"native-test","status":{"type":"idle"}}}})).await;
    wait_for(&bound.broker, |a| {
        a.as_array()
            .unwrap()
            .iter()
            .any(|x| x["display_name"] == "native-test")
    })
    .await;
    send(&mut ws,json!({"method":"thread/name/updated","params":{"threadId":session,"threadName":"renamed-test"}})).await;
    let read = recv(&mut ws).await;
    assert_eq!(read["method"], "thread/read");
    send(&mut ws,json!({"id":read["id"],"result":{"thread":{"id":session,"name":"renamed-test","status":{"type":"idle"}}}})).await;
    wait_for(&bound.broker, |a| {
        a.as_array()
            .unwrap()
            .iter()
            .any(|x| x["display_name"] == "renamed-test")
    })
    .await;
    let sent = bound
        .broker
        .request(Request::Send {
            id: id(),
            sender: "user".into(),
            recipient: "renamed-test".into(),
            body: "test".into(),
            parent: None,
        })
        .unwrap();
    assert_eq!(sent["recipient_session"], session);
    send(
        &mut ws,
        json!({"method":"thread/closed","params":{"threadId":session}}),
    )
    .await;
    wait_for(&bound.broker, |a| {
        a.as_array()
            .unwrap()
            .iter()
            .any(|x| x["session"] == session && x["enabled"] == false)
    })
    .await;
    observer.abort();
    let _ = observer.await;
}

#[tokio::test]
async fn default_list_hides_exited_sessions_and_all_preserves_history_and_restart_identity() {
    let dir = tempfile::Builder::new()
        .prefix("abl-")
        .tempdir_in("/tmp")
        .unwrap();
    let bound = Broker::bind(dir.path().join("state"), "/missing/codex".into()).unwrap();
    let session = id();
    let thread = json!({"id":session,"name":"returning-peer","status":{"type":"idle"}});
    bound.broker.observe_codex(&thread).unwrap();
    let mid = id();
    bound
        .broker
        .request(Request::Send {
            id: mid.clone(),
            sender: "user".into(),
            recipient: "returning-peer".into(),
            body: "retained".into(),
            parent: None,
        })
        .unwrap();
    bound.broker.codex_offline(Some(&session)).unwrap();
    // Existing clients omitting the new field also get the available-only default.
    let default: Request = serde_json::from_value(json!({"op":"list"})).unwrap();
    let listed = bound.broker.request(default).unwrap();
    assert!(
        !listed
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["session"] == session)
    );
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["enabled"] == true)
    );
    let all = bound.broker.request(Request::List { all: true }).unwrap();
    assert!(
        all.as_array()
            .unwrap()
            .iter()
            .any(|a| a["session"] == session && a["enabled"] == false)
    );
    let message = bound.broker.request(Request::Get { id: mid }).unwrap();
    assert_eq!(message["body"], "retained");
    assert_eq!(message["recipient_session"], session);
    bound.broker.observe_codex(&thread).unwrap();
    let listed = bound.broker.request(Request::List { all: false }).unwrap();
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["session"] == session && a["enabled"] == true)
    );
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
        .send(tokio_tungstenite::tungstenite::Message::Text(
            value.to_string().into(),
        ))
        .await
        .unwrap();
}
fn enabled(agents: &Value, session: &str) -> bool {
    agents
        .as_array()
        .unwrap()
        .iter()
        .any(|agent| agent["session"] == session && agent["enabled"] == true)
}

#[tokio::test]
async fn complete_paginated_baseline_preserves_live_events_and_ignores_reads_preceding_closure() {
    let dir = tempfile::Builder::new()
        .prefix("ab-scan-")
        .tempdir_in("/tmp")
        .unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let path = dir.path().join("native.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let bound = Broker::bind(dir.path().join("state"), "/missing/codex".into()).unwrap();
    let keep = id();
    let absent = id();
    let joining = id();
    for session in [&keep, &absent] {
        bound
            .broker
            .observe_codex(&json!({"id":session,"name":"persisted","status":{"type":"idle"}}))
            .unwrap();
    }
    let broker = bound.broker.clone();
    let native_home = home.clone();
    let observer =
        tokio::spawn(
            async move { discovery::observe_connection(&broker, &path, &native_home).await },
        );
    let (stream, _) = listener.accept().await.unwrap();
    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
    let initialize = native_recv(&mut socket).await;
    native_send(
        &mut socket,
        json!({"id":initialize["id"],"result":{"codexHome":home}}),
    )
    .await;
    assert_eq!(native_recv(&mut socket).await["method"], "initialized");
    let first = native_recv(&mut socket).await;
    let before = bound.broker.request(Request::List { all: true }).unwrap();
    assert!(enabled(&before, &keep) && enabled(&before, &absent));
    native_send(
        &mut socket,
        json!({"id":first["id"],"result":{"data":[keep],"nextCursor":"page-two"}}),
    )
    .await;
    let read = native_recv(&mut socket).await;
    assert_eq!(read["method"], "thread/read");
    let second = native_recv(&mut socket).await;
    assert_eq!(second["params"]["cursor"], "page-two");
    native_send(&mut socket,json!({"id":read["id"],"result":{"thread":{"id":keep,"name":"unchanged","status":{"type":"idle"}}}})).await;
    native_send(
        &mut socket,
        json!({"method":"thread/started","params":{"thread":{"id":joining}}}),
    )
    .await;
    let read = native_recv(&mut socket).await;
    native_send(&mut socket,json!({"id":read["id"],"result":{"thread":{"id":joining,"name":"joined during scan","status":{"type":"idle"}}}})).await;
    wait_for(&bound.broker, |agents| enabled(agents, &joining)).await;
    // A live session observed after the original scan must survive an empty
    // final page, even though it was absent from the initial native snapshot.
    native_send(
        &mut socket,
        json!({"id":second["id"],"result":{"data":[],"nextCursor":null}}),
    )
    .await;
    wait_for(&bound.broker, |agents| {
        enabled(agents, &keep) && enabled(agents, &joining) && !enabled(agents, &absent)
    })
    .await;
    native_send(
        &mut socket,
        json!({"method":"thread/name/updated","params":{"threadId":keep}}),
    )
    .await;
    let stale_read = native_recv(&mut socket).await;
    native_send(
        &mut socket,
        json!({"method":"thread/closed","params":{"threadId":keep}}),
    )
    .await;
    wait_for(&bound.broker, |agents| !enabled(agents, &keep)).await;
    native_send(&mut socket,json!({"id":stale_read["id"],"result":{"thread":{"id":keep,"name":"late stale read","status":{"type":"idle"}}}})).await;
    // A later request/response is a processing barrier for the ignored old read.
    native_send(
        &mut socket,
        json!({"method":"thread/name/updated","params":{"threadId":joining}}),
    )
    .await;
    let read = native_recv(&mut socket).await;
    assert_eq!(read["params"]["threadId"], joining);
    assert!(!enabled(
        &bound.broker.request(Request::List { all: true }).unwrap(),
        &keep
    ));
    native_send(
        &mut socket,
        json!({"method":"thread/started","params":{"thread":{"id":keep}}}),
    )
    .await;
    let reopened = native_recv(&mut socket).await;
    native_send(&mut socket,json!({"id":reopened["id"],"result":{"thread":{"id":keep,"name":"genuine reopen","status":{"type":"idle"}}}})).await;
    wait_for(&bound.broker, |agents| enabled(agents, &keep)).await;
    observer.abort();
    let _ = observer.await;
}

#[tokio::test]
async fn partial_failed_or_malformed_loaded_scans_never_retire_persisted_sessions() {
    for malformed in [false, true] {
        let dir = tempfile::Builder::new()
            .prefix("ab-scan-fail-")
            .tempdir_in("/tmp")
            .unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let path = dir.path().join("native.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let bound = Broker::bind(dir.path().join("state"), "/missing/codex".into()).unwrap();
        let persisted = id();
        bound
            .broker
            .observe_codex(&json!({"id":persisted,"status":{"type":"idle"}}))
            .unwrap();
        let broker = bound.broker.clone();
        let observer =
            tokio::spawn(async move { discovery::observe_connection(&broker, &path, &home).await });
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let initialize = native_recv(&mut socket).await;
        native_send(
            &mut socket,
            json!({"id":initialize["id"],"result":{"codexHome":dir.path().join("home")}}),
        )
        .await;
        native_recv(&mut socket).await;
        let first = native_recv(&mut socket).await;
        native_send(
            &mut socket,
            json!({"id":first["id"],"result":{"data":[],"nextCursor":"next"}}),
        )
        .await;
        let second = native_recv(&mut socket).await;
        assert!(enabled(
            &bound.broker.request(Request::List { all: true }).unwrap(),
            &persisted
        ));
        native_send(
            &mut socket,
            if malformed {
                json!({"id":second["id"],"result":{"nextCursor":null}})
            } else {
                json!({"id":second["id"],"error":{"code":-1,"message":"fixture page failure"}})
            },
        )
        .await;
        assert!(observer.await.unwrap().is_err());
        assert!(enabled(
            &bound.broker.request(Request::List { all: true }).unwrap(),
            &persisted
        ));
    }
}

use agent_bridge::{
    board::{self, Edit, Name, Snapshot},
    board_interaction::{self, Kind},
    board_messages::Submission,
    broker::{self, Broker},
    protocol::{Agent, Provider, Request},
};
use serde_json::Value;
use std::{os::unix::fs::PermissionsExt, time::Duration};

fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn register(broker: &Broker, name: &str) -> Agent {
    let agent = Agent {
        name: name.into(),
        provider: Provider::Inbox,
        session: id(),
        endpoint: String::new(),
        enabled: true,
    };
    broker
        .request(Request::Register {
            agent: agent.clone(),
            replace: false,
        })
        .unwrap();
    agent
}
fn read(broker: &Broker, name: &Name) -> Snapshot {
    serde_json::from_value(broker.request(Request::board_read(name.clone())).unwrap()).unwrap()
}
fn add(broker: &Broker, board: &Name, agent: &str, field: &str) -> Snapshot {
    let value = broker
        .request(Request::AddBoardMessageInput {
            agent: agent.into(),
            board: board.clone(),
            expected_revision: read(broker, board).revision,
            id: field.into(),
            label: format!("Message {agent}"),
        })
        .unwrap();
    assert_eq!(value["applied"], true);
    serde_json::from_value(value["board"].clone()).unwrap()
}
fn submission(snapshot: &Snapshot, field: &str, text: &str) -> Submission {
    let controls = board_interaction::controls(&snapshot.markdown);
    let control = controls
        .iter()
        .find(|control| matches!(&control.kind,Kind::Message {id,..} if id == field))
        .unwrap();
    Submission {
        board: snapshot.name.parse().unwrap(),
        expected_revision: snapshot.revision.clone(),
        control_id: board_interaction::control_id(snapshot, control),
        id: id(),
        text: text.into(),
    }
}
fn send(broker: &Broker, request: &Submission) -> Value {
    broker
        .request(Request::BoardMessageSend {
            request: request.clone(),
        })
        .unwrap()
}
fn set(broker: &Broker, snapshot: &Snapshot, markdown: String) {
    broker
        .request(Request::board_update(
            snapshot.name.parse().unwrap(),
            snapshot.revision.clone(),
            vec![Edit::Set { markdown }],
            None,
        ))
        .unwrap();
}
#[tokio::test]
async fn repeated_controls_named_boards_and_durable_exact_retry_never_cross_route() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    let broker = &bound.broker;
    register(broker, "one");
    register(broker, "two");
    let main = Name::default();
    let other: Name = "other".parse().unwrap();
    broker
        .request(Request::BoardCreate {
            board: other.clone(),
        })
        .unwrap();
    add(broker, &main, "one", "first");
    let main_snapshot = add(broker, &main, "two", "second");
    let other_snapshot = add(broker, &other, "one", "first");
    let exact = "  literal\r\n你好 $(never run)\n ";
    let request = submission(&main_snapshot, "first", exact);
    let receipt = send(broker, &request);
    assert_eq!(receipt["accepted"], true);
    assert_eq!(receipt["message"]["body"], exact);
    assert_eq!(receipt["message"]["sender"], "user");
    assert_eq!(receipt["message"]["recipient"], "one");
    assert_eq!(
        send(broker, &submission(&main_snapshot, "second", "second"))["message"]["recipient"],
        "two"
    );
    assert_eq!(
        send(broker, &submission(&other_snapshot, "first", "other"))["message"]["recipient"],
        "one"
    );
    assert_eq!(read(broker, &main).markdown, main_snapshot.markdown);
    assert_eq!(read(broker, &main).revision, main_snapshot.revision);
    set(broker, &main_snapshot, "removed".into());
    broker
        .request(Request::Unregister {
            name: "one".into(),
            session: receipt["message"]["recipient_session"]
                .as_str()
                .unwrap()
                .into(),
        })
        .unwrap();
    assert_eq!(send(broker, &request)["duplicate"], true);
    let mut collision = request.clone();
    collision.text.push('!');
    assert_eq!(send(broker, &collision)["code"], "conflict");
    drop(bound);
    let rebound = Broker::bind(state, "/missing/codex".into()).unwrap();
    assert_eq!(send(&rebound.broker, &request)["duplicate"], true);
    let history = rebound
        .broker
        .request(Request::History {
            agent: None,
            limit: 100,
        })
        .unwrap();
    assert_eq!(history.as_array().unwrap().len(), 3);
}
#[tokio::test]
async fn changed_removed_duplicate_malformed_and_replaced_owner_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let bound = Broker::bind(dir.path().join("state"), "/missing/codex".into()).unwrap();
    let broker = &bound.broker;
    let owner = register(broker, "owner");
    let main = Name::default();
    let initial = add(broker, &main, "owner", "compose");
    let old = submission(&initial, "compose", "retain draft");
    set(broker, &initial, format!("{}\nchanged", initial.markdown));
    assert_eq!(send(broker, &old)["code"], "conflict");
    let changed = read(broker, &main);
    let fresh = submission(&changed, "compose", "retain draft");
    broker
        .request(Request::Register {
            agent: Agent {
                session: id(),
                ..owner
            },
            replace: true,
        })
        .unwrap();
    assert_eq!(send(broker, &fresh)["code"], "unavailable");
    for markdown in [
        format!("{}\n{}", initial.markdown, initial.markdown),
        format!("{}\n<!-- board-message: {{broken}} -->\n", initial.markdown),
        "removed".into(),
    ] {
        let current = read(broker, &main);
        set(broker, &current, markdown);
        let current = read(broker, &main);
        let mut attempt = old.clone();
        attempt.expected_revision = current.revision;
        assert_eq!(send(broker, &attempt)["accepted"], false);
    }
}
#[tokio::test]
async fn full_queue_rejects_atomically_and_foreign_uuid_cannot_be_adopted() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    let broker = &bound.broker;
    register(broker, "owner");
    let snapshot = add(broker, &Name::default(), "owner", "compose");
    let request = submission(&snapshot, "compose", "body");
    broker
        .request(Request::Send {
            id: request.id.clone(),
            sender: "user".into(),
            recipient: "owner".into(),
            body: request.text.clone(),
            parent: None,
        })
        .unwrap();
    assert_eq!(send(broker, &request)["code"], "conflict");
    let session = register_session(broker);
    let db = rusqlite::Connection::open(state.join("messages.sqlite3")).unwrap();
    let tx = db.unchecked_transaction().unwrap();
    for _ in 0..999 {
        tx.execute(
            "INSERT INTO messages VALUES(?1,'user','owner',?2,'queued',NULL,0,'queued',NULL,0,0)",
            rusqlite::params![id(), session],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    let request = submission(&snapshot, "compose", "queue draft");
    assert_eq!(send(broker, &request)["code"], "rejected");
    let count: i64 = db
        .query_row("SELECT count(*) FROM board_message_receipts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
    db.execute(
        "UPDATE messages SET state='cancelled' WHERE created=0 AND state='queued'",
        [],
    )
    .unwrap();
    assert_eq!(send(broker, &request)["accepted"], true);
    assert_eq!(send(broker, &request)["duplicate"], true);
}
fn register_session(broker: &Broker) -> String {
    broker
        .request(Request::List { all: true })
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["name"] == "owner")
        .unwrap()["session"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn composer_uses_actual_native_claude_delivery_and_closed_owner_is_unavailable() {
    use tokio::{
        io::{AsyncBufReadExt, BufReader},
        net::UnixListener,
        time::timeout,
    };
    let dir = tempfile::Builder::new()
        .prefix("ab-")
        .tempdir_in("/tmp")
        .unwrap();
    let native = dir.path().join("native");
    std::fs::create_dir(&native).unwrap();
    std::fs::set_permissions(&native, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = native.join("inbox.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let state = dir.path().join("state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    let broker = bound.broker.clone();
    let owner = Agent {
        name: "coordinator".into(),
        provider: Provider::Claude,
        session: id(),
        endpoint: socket.to_string_lossy().into_owned(),
        enabled: true,
    };
    broker
        .request(Request::Register {
            agent: owner.clone(),
            replace: false,
        })
        .unwrap();
    let snapshot = add(&broker, &Name::default(), &owner.name, "compose");
    let request = submission(&snapshot, "compose", "Exact multiline\ninstruction 🦀");
    let receipt = send(&broker, &request);
    assert_eq!(receipt["accepted"], true);
    assert_eq!(receipt["state"], "queued");
    let server = tokio::spawn(broker::serve(bound));
    let frame = timeout(Duration::from_secs(5), async {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).await.unwrap();
            if !line.is_empty() {
                break serde_json::from_str::<Value>(&line).unwrap();
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(frame["msg_id"], request.id);
    assert_eq!(frame["session_id"], owner.session);
    assert_eq!(frame["from"], "user");
    assert!(
        frame["message"]["content"]
            .as_str()
            .unwrap()
            .contains(&request.text)
    );
    assert_eq!(board::read(&state).unwrap().markdown, snapshot.markdown);
    drop(listener);
    std::fs::remove_file(socket).unwrap();
    let fresh = submission(&snapshot, "compose", "keep draft");
    assert_eq!(send(&broker, &fresh)["code"], "unavailable");
    assert_eq!(send(&broker, &request)["duplicate"], true);
    server.abort();
    let _ = server.await;
}

#[test]
fn markers_exclude_examples_nested_comments_and_response_bodies_and_share_ids() {
    let owner = agent_bridge::board_messages::Owner {
        name: "coordinator".into(),
        provider: Provider::Inbox,
        session: "session".into(),
        endpoint: String::new(),
    };
    let marker = agent_bridge::board_messages::Marker {
        id: "compose".into(),
        instance: id(),
        label: "Message coordinator".into(),
        owner,
    };
    let line = format!(
        "<!-- board-message: {} -->",
        serde_json::to_string(&marker).unwrap()
    );
    assert_eq!(board_interaction::controls(&line).len(), 1);
    for source in [
        format!("```md\n{line}\n```"),
        format!("<!-- outer\n{line}\n-->"),
        format!("<!-- board-input: response -->\n> {line}\n<!-- /board-input -->"),
        format!("{line}\n{line}"),
        format!("{line}\n<!-- board-input: compose -->\n> response\n<!-- /board-input -->"),
        format!("{line}\n<!-- board-message: {{\"id\":\"compose\",\"invalid\":true}} -->"),
    ] {
        assert!(
            !board_interaction::controls(&source)
                .iter()
                .any(|control| matches!(control.kind, Kind::Message { .. })),
            "{source}"
        );
    }
    let parsed = board_interaction::controls(&line);
    assert!(parsed[0].replacement(&line, Some("text")).is_err());
    let key = parsed[0].message_key("main").unwrap();
    assert_ne!(key, parsed[0].message_key("other").unwrap());
}

#[tokio::test]
async fn authoring_rejects_duplicate_unsafe_alias_and_stale_inputs_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let bound = Broker::bind(dir.path().join("state"), "/missing/codex".into()).unwrap();
    let broker = &bound.broker;
    register(broker, "owner");
    let board = Name::default();
    let empty = read(broker, &board);
    let snapshot = add(broker, &board, "owner", "compose");
    for (agent, revision, field, label) in [
        ("owner", snapshot.revision.as_str(), "compose", "duplicate"),
        ("owner", snapshot.revision.as_str(), "another", "bad\nlabel"),
        (
            "owner",
            snapshot.revision.as_str(),
            "another",
            "bad --> label",
        ),
        (
            "unknown-alias",
            snapshot.revision.as_str(),
            "another",
            "Message",
        ),
    ] {
        assert!(
            broker
                .request(Request::AddBoardMessageInput {
                    agent: agent.into(),
                    board: board.clone(),
                    expected_revision: revision.into(),
                    id: field.into(),
                    label: label.into()
                })
                .is_err()
        );
        assert_eq!(read(broker, &board).markdown, snapshot.markdown);
    }
    let stale = broker
        .request(Request::AddBoardMessageInput {
            agent: "owner".into(),
            board: board.clone(),
            expected_revision: empty.revision,
            id: "another".into(),
            label: "Message".into(),
        })
        .unwrap();
    assert_eq!(stale["conflict"], true);
    assert_eq!(read(broker, &board).markdown, snapshot.markdown);
}

#[tokio::test]
async fn receipt_insert_failure_rolls_back_message_and_same_uuid_can_retry_once() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    let broker = &bound.broker;
    register(broker, "owner");
    let snapshot = add(broker, &Name::default(), "owner", "compose");
    let request = submission(&snapshot, "compose", "retain this exact draft");
    let db = rusqlite::Connection::open(state.join("messages.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER reject_board_receipt BEFORE INSERT ON board_message_receipts BEGIN SELECT RAISE(ABORT,'Injected receipt persistence failure'); END;").unwrap();
    let rejected = send(broker, &request);
    assert_eq!(rejected["accepted"], false);
    assert_eq!(rejected["code"], "uncertain");
    let counts: (i64, i64) = db.query_row(
        "SELECT (SELECT count(*) FROM messages WHERE id=?1),(SELECT count(*) FROM board_message_receipts WHERE id=?1)",
        [&request.id], |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert_eq!(counts, (0, 0), "Both inserts must roll back together");
    db.execute_batch("DROP TRIGGER reject_board_receipt;")
        .unwrap();
    assert_eq!(send(broker, &request)["accepted"], true);
    assert_eq!(send(broker, &request)["duplicate"], true);
    let counts: (i64, i64) = db.query_row(
        "SELECT (SELECT count(*) FROM messages WHERE id=?1),(SELECT count(*) FROM board_message_receipts WHERE id=?1)",
        [&request.id], |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert_eq!(counts, (1, 1));
    assert_eq!(read(broker, &Name::default()).markdown, snapshot.markdown);
}

#[tokio::test]
async fn reauthoring_identical_user_id_creates_new_identity_but_accepted_retry_survives() {
    let dir = tempfile::tempdir().unwrap();
    let bound = Broker::bind(dir.path().join("state"), "/missing/codex".into()).unwrap();
    let broker = &bound.broker;
    register(broker, "owner");
    let name = Name::default();
    let original = add(broker, &name, "owner", "compose");
    let accepted = submission(&original, "compose", "accepted before removal");
    let unsent = submission(&original, "compose", "old unsent draft");
    assert_eq!(send(broker, &accepted)["accepted"], true);
    set(broker, &original, String::new());
    let recreated = add(broker, &name, "owner", "compose");
    let before = board_interaction::controls(&original.markdown);
    let after = board_interaction::controls(&recreated.markdown);
    let Kind::Message {
        instance: before_instance,
        ..
    } = &before[0].kind
    else {
        panic!("Expected composer");
    };
    let Kind::Message {
        instance: after_instance,
        ..
    } = &after[0].kind
    else {
        panic!("Expected composer");
    };
    assert_ne!(before_instance, after_instance);
    assert_ne!(original.revision, recreated.revision);
    assert_ne!(
        board_interaction::control_id(&original, &before[0]),
        board_interaction::control_id(&recreated, &after[0])
    );
    assert_ne!(before[0].message_key("main"), after[0].message_key("main"));
    assert_eq!(send(broker, &unsent)["code"], "conflict");
    assert_eq!(send(broker, &accepted)["duplicate"], true);
    assert_eq!(
        send(
            broker,
            &submission(&recreated, "compose", "reviewed new composer")
        )["accepted"],
        true
    );
}

#[tokio::test]
async fn authoring_requires_the_new_instance_to_be_an_active_control() {
    let dir = tempfile::tempdir().unwrap();
    let bound = Broker::bind(dir.path().join("state"), "/missing/codex".into()).unwrap();
    let broker = &bound.broker;
    register(broker, "owner");
    let board = Name::default();
    let original = add(broker, &board, "owner", "compose");
    for tail in [
        "\n```md\n",
        "\n<!-- unclosed comment\n",
        "\n<!-- board-input: response -->\n> unclosed response\n",
    ] {
        let current = read(broker, &board);
        set(broker, &current, format!("{}{tail}", original.markdown));
        let before = read(broker, &board);
        for field in ["compose", "fresh"] {
            let result = broker.request(Request::AddBoardMessageInput {
                agent: "owner".into(),
                board: board.clone(),
                expected_revision: before.revision.clone(),
                id: field.into(),
                label: "Message owner".into(),
            });
            assert!(
                result.is_err(),
                "Hidden authored input {field} must not succeed in {tail:?}"
            );
            let after = read(broker, &board);
            assert_eq!(after.markdown, before.markdown);
            assert_eq!(after.revision, before.revision);
        }
    }
}

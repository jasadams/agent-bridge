//! Actual stale broker/monitor requests after purging and recreating identical bytes.
//! These tests never serve a broker or start discovery/native delivery.
use crate::{
    board::{Edit, Name, Snapshot},
    board_lifecycle,
    board_messages::Submission,
    broker::{BoundServer, Broker},
    monitor_board::{Desired, ResponseRequest},
    protocol::{Agent, EventSelector, Provider, Request},
};
use serde_json::Value;
use std::collections::BTreeMap;

struct Fixture {
    bound: BoundServer,
    _root: tempfile::TempDir,
    name: Name,
    original: Snapshot,
    owner: Agent,
}

fn read(broker: &Broker, name: &Name) -> Snapshot {
    serde_json::from_value(broker.request(Request::board_read(name.clone())).unwrap()).unwrap()
}

fn set(broker: &Broker, name: &Name, markdown: &str) -> Snapshot {
    let current = read(broker, name);
    let reply = broker
        .request(Request::board_update(
            name.clone(),
            current.revision,
            vec![Edit::Set {
                markdown: markdown.to_owned(),
            }],
            None,
        ))
        .unwrap();
    assert_eq!(reply["applied"], true);
    serde_json::from_value(reply["board"].clone()).unwrap()
}

fn fixture() -> Fixture {
    let root = tempfile::Builder::new()
        .prefix("agb25-reuse-")
        .tempdir_in("/var/tmp")
        .unwrap();
    let bound = Broker::bind(root.path().join("state"), "/missing/codex".into()).unwrap();
    let broker = &bound.broker;
    let name: Name = "same-name".parse().unwrap();
    broker
        .request(Request::BoardCreate {
            board: name.clone(),
        })
        .unwrap();
    let initial = set(
        broker,
        &name,
        "# Reusable board 🦀\r\n\r\n- [ ] Keep this choice\r\n\r\n<!-- board-input: answer -->\r\n> unchanged 世界\r\n<!-- /board-input -->\r\n",
    );
    let owner = Agent {
        name: "reuse-recipient".into(),
        provider: Provider::Inbox,
        session: uuid::Uuid::new_v4().to_string(),
        endpoint: String::new(),
        enabled: true,
    };
    broker
        .request(Request::Register {
            agent: owner.clone(),
            replace: false,
        })
        .unwrap();
    let added = broker
        .request(Request::AddBoardMessageInput {
            agent: owner.name.clone(),
            board: name.clone(),
            expected_revision: initial.revision,
            id: "compose".into(),
            label: "Message original recipient".into(),
        })
        .unwrap();
    assert_eq!(added["applied"], true);
    let original = serde_json::from_value(added["board"].clone()).unwrap();
    // Enable only the private monitor admission state. No HTTP listener is run.
    broker.monitor_control.lock().unwrap().enable(0);
    Fixture {
        bound,
        _root: root,
        name,
        original,
        owner,
    }
}

fn control_id(view: &Value, kind: &str) -> String {
    let controls: Vec<_> = view["controls"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|control| control["kind"] == kind)
        .collect();
    assert_eq!(controls.len(), 1, "Expected one {kind} fixture control");
    controls[0]["id"].as_str().unwrap().to_owned()
}

fn rebase_key(view: &Value, kind: &str) -> String {
    view["controls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|control| control["kind"] == kind)
        .unwrap()["rebase_key"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn purge_and_recreate(fixture: &Fixture, archived_at: i64) -> (Snapshot, Snapshot) {
    let broker = &fixture.bound.broker;
    let archived = {
        let db = broker.db.lock().unwrap();
        let archived = broker
            .archive_board_locked(&db, &fixture.name, &fixture.original.revision, archived_at)
            .unwrap();
        assert_eq!(
            archived.lifecycle.expires_at,
            Some(archived_at + board_lifecycle::RETENTION_MS)
        );
        assert!(
            board_lifecycle::cleanup(
                &db,
                &broker.state_dir,
                archived_at + board_lifecycle::RETENTION_MS,
            )
            .unwrap()
            .is_empty()
        );
        archived
    };
    assert!(
        !broker
            .state_dir
            .join("boards")
            .join(format!("{}.md", fixture.name))
            .exists(),
        "Purge must remove the managed file"
    );
    assert!(
        broker
            .request(Request::BoardArchivedList)
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty(),
        "Purge must remove the archived record before name reuse"
    );
    broker
        .request(Request::BoardCreate {
            board: fixture.name.clone(),
        })
        .unwrap();
    // Preserve the message marker instance and every byte, including mixed
    // CRLF/LF and Unicode. Only the broker lifecycle identity is new.
    let recreated = set(broker, &fixture.name, &fixture.original.markdown);
    assert_eq!(recreated.markdown, fixture.original.markdown);
    assert_eq!(
        std::fs::read(
            crate::board::read_named(&broker.state_dir, &fixture.name)
                .unwrap()
                .path
        )
        .unwrap()
        .as_slice(),
        fixture.original.markdown.as_bytes()
    );
    assert_ne!(
        recreated.lifecycle.incarnation,
        fixture.original.lifecycle.incarnation
    );
    assert_ne!(
        recreated.lifecycle.lifecycle_generation,
        fixture.original.lifecycle.lifecycle_generation
    );
    assert_ne!(recreated.revision, fixture.original.revision);
    assert_ne!(recreated.control_scope(), fixture.original.control_scope());
    (archived, recreated)
}

#[tokio::test]
async fn identical_name_and_bytes_do_not_reauthorize_old_controls_or_message_intent() {
    let fixture = fixture();
    let broker = &fixture.bound.broker;
    let before_view = broker.monitor_board(1, &fixture.name, None);
    assert_eq!(before_view.status, 200);
    let old_checkbox = control_id(&before_view.body, "checkbox");
    let old_text = control_id(&before_view.body, "text");
    let unknown_message = Submission {
        board: fixture.name.clone(),
        expected_revision: fixture.original.revision.clone(),
        control_id: control_id(&before_view.body, "message"),
        id: uuid::Uuid::new_v4().to_string(),
        text: "Original unsent intent must not reach a replacement board".into(),
    };
    // This UUID has never been admitted; the accepted-receipt exception must
    // not accidentally authorize it after purge and name reuse.
    assert!(
        broker
            .db
            .lock()
            .unwrap()
            .history(&None, 100)
            .unwrap()
            .is_empty()
    );
    let archived_at = crate::db::now();
    let (archived, recreated) = purge_and_recreate(&fixture, archived_at);
    let after_view = broker.monitor_board(1, &fixture.name, None);
    assert_eq!(after_view.status, 200);
    for kind in ["checkbox", "text", "message"] {
        assert_ne!(
            control_id(&before_view.body, kind),
            control_id(&after_view.body, kind)
        );
    }
    for kind in ["text", "message"] {
        assert_ne!(
            rebase_key(&before_view.body, kind),
            rebase_key(&after_view.body, kind)
        );
    }
    let old_write = broker
        .request(Request::board_update(
            fixture.name.clone(),
            fixture.original.revision.clone(),
            vec![Edit::Append {
                text: "stale ordinary write".into(),
            }],
            None,
        ))
        .unwrap();
    assert_eq!(old_write["applied"], false);
    assert_eq!(old_write["conflict"], true);

    for expected in [&fixture.original.revision, &recreated.revision] {
        // The second iteration refreshes ONLY the revision: old control IDs
        // must remain unusable even when name, Markdown and marker instance match.
        let checkbox = broker.monitor_board_response(
            1,
            &fixture.name,
            ResponseRequest {
                board: fixture.name.clone(),
                expected_revision: expected.clone(),
                control_id: old_checkbox.clone(),
                value: Desired::Checkbox { checked: true },
            },
        );
        assert_eq!(checkbox.status, 409);
        let text = broker.monitor_board_response(
            1,
            &fixture.name,
            ResponseRequest {
                board: fixture.name.clone(),
                expected_revision: expected.clone(),
                control_id: old_text.clone(),
                value: Desired::Text {
                    text: "stale replacement response".into(),
                },
            },
        );
        assert_eq!(text.status, 409);
        let mut attempt = unknown_message.clone();
        attempt.expected_revision = expected.clone();
        let message = broker.monitor_board_message(1, &fixture.name, attempt);
        assert_eq!(message.status, 409);
        assert_ne!(message.body["accepted"], true);
        assert_eq!(read(broker, &fixture.name).markdown, recreated.markdown);
        assert!(
            broker
                .db
                .lock()
                .unwrap()
                .history(&None, 100)
                .unwrap()
                .is_empty()
        );
    }
    assert!(
        broker
            .request(Request::BoardArchive {
                board: fixture.name.clone(),
                expected_revision: fixture.original.revision.clone(),
            })
            .is_err()
    );
    assert!(
        broker
            .request(Request::BoardRestore {
                board: fixture.name.clone(),
                expected_revision: archived.revision,
            })
            .is_err()
    );
    {
        let db = broker.db.lock().unwrap();
        assert!(
            board_lifecycle::cleanup(
                &db,
                &broker.state_dir,
                archived_at + 2 * board_lifecycle::RETENTION_MS,
            )
            .unwrap()
            .is_empty()
        );
    }
    let untouched = read(broker, &fixture.name);
    assert_eq!(untouched.revision, recreated.revision);
    assert_eq!(untouched.lifecycle, recreated.lifecycle);
    assert_eq!(untouched.markdown, recreated.markdown);
    assert!(
        crate::board::read_named(&broker.state_dir, &fixture.name)
            .unwrap()
            .path
            .exists(),
        "An old cleanup deadline cannot delete a new active board"
    );

    let fresh = Submission {
        board: fixture.name.clone(),
        expected_revision: recreated.revision.clone(),
        control_id: control_id(&after_view.body, "message"),
        id: uuid::Uuid::new_v4().to_string(),
        text: "Explicit fresh intent for the recreated board".into(),
    };
    let accepted = broker.monitor_board_message(1, &fixture.name, fresh.clone());
    assert_eq!(accepted.status, 200);
    assert_eq!(accepted.body["accepted"], true);
    let duplicate = broker.monitor_board_message(1, &fixture.name, fresh.clone());
    assert_eq!(duplicate.status, 200);
    assert_eq!(duplicate.body["duplicate"], true);
    let history = broker.db.lock().unwrap().history(&None, 100).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, fresh.id);
    assert_eq!(history[0].sender, "user");
    assert_eq!(history[0].recipient, fixture.owner.name);
    assert_eq!(history[0].recipient_session, fixture.owner.session);
    assert_eq!(history[0].body, fresh.text);
    assert_eq!(read(broker, &fixture.name).revision, recreated.revision);
}

#[tokio::test]
async fn old_subscription_fences_cannot_attach_to_or_remove_watches_for_a_recreated_name() {
    let fixture = fixture();
    let broker = &fixture.bound.broker;
    let watcher = Agent {
        name: "reuse-watcher".into(),
        provider: Provider::Codex,
        session: uuid::Uuid::new_v4().to_string(),
        endpoint: String::new(),
        enabled: true,
    };
    broker
        .request(Request::Register {
            agent: watcher.clone(),
            replace: false,
        })
        .unwrap();
    let named = EventSelector::BoardChanged {
        board: fixture.name.clone(),
    };
    let main = EventSelector::BoardChanged {
        board: Name::default(),
    };
    let subscribe = |revision: &str| Request::Subscribe {
        agent: watcher.name.clone(),
        events: vec![named.clone()],
        board_revisions: BTreeMap::from([(fixture.name.clone(), revision.to_owned())]),
    };
    let watched = || {
        broker
            .board_notifications
            .lock()
            .unwrap()
            .events(&watcher.name)
    };
    broker
        .request(Request::Subscribe {
            agent: watcher.name.clone(),
            events: vec![main.clone(), named.clone()],
            board_revisions: BTreeMap::from([(
                fixture.name.clone(),
                fixture.original.revision.clone(),
            )]),
        })
        .unwrap();
    assert_eq!(watched().len(), 2);
    assert!(watched().contains(&named));
    assert!(watched().contains(&main));
    let (_, recreated) = purge_and_recreate(&fixture, crate::db::now());
    assert_eq!(
        watched(),
        vec![main.clone()],
        "Archive removes only its named watch; recreate does not revive it"
    );
    assert!(
        broker
            .request(subscribe(&fixture.original.revision))
            .is_err()
    );
    assert_eq!(watched(), vec![main.clone()]);
    broker.request(subscribe(&recreated.revision)).unwrap();
    assert!(watched().contains(&named));
    assert!(
        broker
            .request(Request::Unsubscribe {
                agent: watcher.name.clone(),
                events: Some(vec![named.clone()]),
                board_revisions: BTreeMap::from([(
                    fixture.name.clone(),
                    fixture.original.revision.clone()
                )]),
            })
            .is_err()
    );
    assert_eq!(watched().len(), 2);
    assert!(
        watched().contains(&named),
        "A stale unsubscribe must preserve the fresh watch"
    );
    assert!(watched().contains(&main));
    broker
        .request(Request::Unsubscribe {
            agent: watcher.name.clone(),
            events: Some(vec![named]),
            board_revisions: BTreeMap::from([(fixture.name.clone(), recreated.revision.clone())]),
        })
        .unwrap();
    assert_eq!(watched(), vec![main]);
    assert_eq!(
        read(broker, &fixture.name).markdown,
        fixture.original.markdown
    );
    assert_eq!(read(broker, &fixture.name).revision, recreated.revision);
    assert!(
        broker
            .db
            .lock()
            .unwrap()
            .history(&None, 100)
            .unwrap()
            .is_empty()
    );
}

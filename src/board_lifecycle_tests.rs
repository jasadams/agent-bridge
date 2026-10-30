//! Broker/API linearization tests, always using disposable private state.
use crate::{
    board::{Edit, Name, Snapshot},
    board_lifecycle,
    broker::{BoundServer, Broker},
    protocol::{Agent, EventSelector, Provider, Request},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Barrier},
    time::Duration,
};

fn setup() -> (tempfile::TempDir, BoundServer, Name) {
    let root = tempfile::Builder::new()
        .prefix("agb25-")
        .tempdir_in("/var/tmp")
        .unwrap();
    let bound = Broker::bind(root.path().join("state"), "/missing/codex".into()).unwrap();
    let name: Name = "project".parse().unwrap();
    bound
        .broker
        .request(Request::BoardCreate {
            board: name.clone(),
        })
        .unwrap();
    (root, bound, name)
}
fn read(broker: &Broker, name: &Name) -> Snapshot {
    serde_json::from_value(broker.request(Request::board_read(name.clone())).unwrap()).unwrap()
}
fn set(broker: &Broker, name: &Name, text: &str) -> Snapshot {
    let before = read(broker, name);
    let result = broker
        .request(Request::board_update(
            name.clone(),
            before.revision,
            vec![Edit::Set {
                markdown: text.into(),
            }],
            None,
        ))
        .unwrap();
    assert_eq!(result["applied"], true);
    serde_json::from_value(result["board"].clone()).unwrap()
}
fn archive(broker: &Broker, name: &Name, expected: &str, now: i64) -> anyhow::Result<Snapshot> {
    let db = broker.db.lock().unwrap();
    broker.archive_board_locked(&db, name, expected, now)
}
fn restore(broker: &Broker, name: &Name, expected: &str, now: i64) -> anyhow::Result<Snapshot> {
    let db = broker.db.lock().unwrap();
    board_lifecycle::restore(&db, &broker.state_dir, name, expected, now)
}

#[tokio::test]
async fn stale_writes_and_subscription_requests_do_not_cross_restore_generation() {
    let (_root, bound, name) = setup();
    let broker = &bound.broker;
    let before = set(broker, &name, "- [ ] Choice\n");
    let agent = Agent {
        name: "watcher".into(),
        provider: Provider::Codex,
        session: uuid::Uuid::new_v4().to_string(),
        endpoint: String::new(),
        enabled: true,
    };
    broker
        .request(Request::Register {
            agent: agent.clone(),
            replace: false,
        })
        .unwrap();
    let events = vec![EventSelector::BoardChanged {
        board: name.clone(),
    }];
    let subscribe = |revision: &str| Request::Subscribe {
        agent: agent.name.clone(),
        events: events.clone(),
        board_revisions: BTreeMap::from([(name.clone(), revision.to_owned())]),
    };
    assert!(
        broker
            .request(Request::Subscribe {
                agent: agent.name.clone(),
                events: events.clone(),
                board_revisions: BTreeMap::new()
            })
            .is_err()
    );
    broker.request(subscribe(&before.revision)).unwrap();
    assert_eq!(
        broker
            .board_notifications
            .lock()
            .unwrap()
            .events(&agent.name)
            .len(),
        1
    );
    let at = crate::db::now();
    let saved = archive(broker, &name, &before.revision, at).unwrap();
    assert!(
        broker
            .board_notifications
            .lock()
            .unwrap()
            .events(&agent.name)
            .is_empty()
    );
    assert!(
        broker
            .request(Request::board_update(
                name.clone(),
                before.revision.clone(),
                vec![Edit::Append {
                    text: "stale".into()
                }],
                None
            ))
            .is_err()
    );
    let restored = restore(broker, &name, &saved.revision, at + 1).unwrap();
    assert_ne!(restored.revision, before.revision);
    assert!(
        broker
            .board_notifications
            .lock()
            .unwrap()
            .events(&agent.name)
            .is_empty()
    );
    let stale = broker
        .request(Request::board_update(
            name.clone(),
            before.revision.clone(),
            vec![Edit::Append {
                text: "stale".into(),
            }],
            None,
        ))
        .unwrap();
    assert_eq!(stale["conflict"], true);
    assert_eq!(read(broker, &name).markdown, before.markdown);
    assert!(broker.request(subscribe(&before.revision)).is_err());
    broker.request(subscribe(&restored.revision)).unwrap();
    assert!(
        broker
            .request(Request::Unsubscribe {
                agent: agent.name.clone(),
                events: Some(events.clone()),
                board_revisions: BTreeMap::from([(name.clone(), before.revision)])
            })
            .is_err()
    );
    assert_eq!(
        broker
            .board_notifications
            .lock()
            .unwrap()
            .events(&agent.name)
            .len(),
        1
    );
    broker
        .request(Request::Unsubscribe {
            agent: agent.name,
            events: Some(events),
            board_revisions: BTreeMap::from([(name, restored.revision)]),
        })
        .unwrap();
}

#[tokio::test]
async fn archive_and_concurrent_update_have_only_serialized_outcomes() {
    let (_root, bound, _) = setup();
    let broker = &bound.broker;
    for index in 0..8 {
        let name: Name = format!("race-{index}").parse().unwrap();
        broker
            .request(Request::BoardCreate {
                board: name.clone(),
            })
            .unwrap();
        let before = set(broker, &name, "original");
        let barrier = Arc::new(Barrier::new(2));
        let archive_broker = broker.clone();
        let archive_name = name.clone();
        let expected = before.revision.clone();
        let gate = barrier.clone();
        let archiver = std::thread::spawn(move || {
            gate.wait();
            archive(&archive_broker, &archive_name, &expected, crate::db::now())
        });
        let writer_broker = broker.clone();
        let writer_name = name.clone();
        let expected = before.revision.clone();
        let writer = std::thread::spawn(move || {
            barrier.wait();
            writer_broker.request(Request::board_update(
                writer_name,
                expected,
                vec![Edit::Append {
                    text: " written".into(),
                }],
                None,
            ))
        });
        let archived = archiver.join().unwrap();
        let written = writer.join().unwrap();
        match archived {
            Ok(saved) => {
                assert!(written.is_err());
                assert_eq!(saved.markdown, "original");
            }
            Err(error) => {
                assert!(error.downcast_ref::<crate::board::Conflict>().is_some());
                assert_eq!(written.unwrap()["applied"], true);
                let current = read(broker, &name);
                assert_eq!(current.markdown, "original written");
                assert_eq!(
                    archive(broker, &name, &current.revision, crate::db::now())
                        .unwrap()
                        .markdown,
                    current.markdown
                );
            }
        }
    }
}

#[tokio::test]
async fn restore_and_expiring_cleanup_have_one_outcome_without_deleting_a_restored_board() {
    let (_root, bound, _) = setup();
    let broker = &bound.broker;
    for index in 0..8 {
        let name: Name = format!("expiry-{index}").parse().unwrap();
        broker
            .request(Request::BoardCreate {
                board: name.clone(),
            })
            .unwrap();
        let before = set(broker, &name, "preserve");
        let at = crate::db::now();
        let saved = archive(broker, &name, &before.revision, at).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let restore_broker = broker.clone();
        let restore_name = name.clone();
        let gate = barrier.clone();
        let restorer = std::thread::spawn(move || {
            gate.wait();
            restore(
                &restore_broker,
                &restore_name,
                &saved.revision,
                at + board_lifecycle::RETENTION_MS - 1,
            )
        });
        let cleanup_broker = broker.clone();
        let cleaner = std::thread::spawn(move || {
            barrier.wait();
            let db = cleanup_broker.db.lock().unwrap();
            board_lifecycle::cleanup(
                &db,
                &cleanup_broker.state_dir,
                at + board_lifecycle::RETENTION_MS,
            )
        });
        let restored = restorer.join().unwrap();
        assert!(cleaner.join().unwrap().unwrap().is_empty());
        if restored.is_ok() {
            assert_eq!(read(broker, &name).markdown, "preserve");
        } else {
            assert!(broker.request(Request::board_read(name.clone())).is_err());
            assert!(
                !broker
                    .state_dir
                    .join("boards")
                    .join(format!("{name}.md"))
                    .exists()
            );
        }
    }
}

#[tokio::test]
async fn startup_and_periodic_cleanup_do_not_depend_on_a_viewer() {
    let (root, bound, name) = setup();
    let broker = &bound.broker;
    let before = set(broker, &name, "offline archive");
    archive(
        broker,
        &name,
        &before.revision,
        crate::db::now() - board_lifecycle::RETENTION_MS - 1,
    )
    .unwrap();
    assert!(before.path.exists());
    drop(bound);
    let rebound = Broker::bind(root.path().join("state"), "/missing/codex".into()).unwrap();
    assert!(!before.path.exists());
    assert!(
        rebound
            .broker
            .request(Request::BoardArchivedList)
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
    let broker = &rebound.broker;
    broker
        .request(Request::BoardCreate {
            board: name.clone(),
        })
        .unwrap();
    let current = set(broker, &name, "periodic archive");
    archive(
        broker,
        &name,
        &current.revision,
        crate::db::now() - board_lifecycle::RETENTION_MS - 1,
    )
    .unwrap();
    let worker = broker.clone();
    let task =
        tokio::spawn(async move { worker.board_cleanup_loop(Duration::from_millis(10)).await });
    let observed = tokio::time::timeout(Duration::from_secs(3), async {
        while current.path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(observed.is_ok());
    assert!(
        broker
            .request(Request::BoardArchivedList)
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn monitor_lifecycle_contract_freezes_controls_and_preserves_conditional_metadata() {
    use crate::monitor_board::{Desired, LifecycleRequest, ResponseRequest};
    let (_root, bound, name) = setup();
    let broker = &bound.broker;
    let before = set(
        broker,
        &name,
        "- [ ] Choice\n<!-- board-input: answer -->\n> original\n<!-- /board-input -->\n",
    );
    broker.monitor_control.lock().unwrap().enable(0);
    let before_view = broker.monitor_board(1, &name, None).body;
    let checkbox = crate::board_interaction::controls(&before.markdown).remove(0);
    let old_response = || ResponseRequest {
        board: name.clone(),
        expected_revision: before.revision.clone(),
        control_id: crate::board_interaction::control_id(&before, &checkbox),
        value: Desired::Checkbox { checked: true },
    };
    let wrong = broker.monitor_board_lifecycle(
        1,
        &name,
        LifecycleRequest {
            board: name.clone(),
            expected_revision: "f".repeat(64),
        },
        false,
    );
    assert_eq!(wrong.status, 409);
    assert!(read(broker, &name).writable());
    let main = Name::default();
    let main_snapshot = read(broker, &main);
    assert_eq!(
        broker
            .monitor_board_lifecycle(
                1,
                &main,
                LifecycleRequest {
                    board: main.clone(),
                    expected_revision: main_snapshot.revision
                },
                false
            )
            .status,
        422
    );
    let saved = broker.monitor_board_lifecycle(
        1,
        &name,
        LifecycleRequest {
            board: name.clone(),
            expected_revision: before.revision.clone(),
        },
        false,
    );
    assert_eq!(saved.status, 200);
    assert_eq!(saved.body["board"]["read_only"], true);
    let revision = saved.body["board"]["revision"].as_str().unwrap().to_owned();
    let conditional = broker.monitor_board(1, &name, Some(&revision));
    assert_eq!(conditional.body["unchanged"], true);
    assert_eq!(conditional.body["read_only"], true);
    assert_eq!(conditional.body["state"], "archived");
    assert!(conditional.body["incarnation"].is_string());
    assert!(conditional.body.get("markdown").is_none());
    assert_eq!(
        broker
            .monitor_board_response(1, &name, old_response())
            .status,
        409
    );
    let restored = broker.monitor_board_lifecycle(
        1,
        &name,
        LifecycleRequest {
            board: name.clone(),
            expected_revision: revision,
        },
        true,
    );
    assert_eq!(restored.status, 200);
    assert_eq!(restored.body["board"]["read_only"], false);
    assert_eq!(restored.body["board"]["markdown"], before.markdown);
    assert_ne!(restored.body["board"]["revision"], before.revision);
    let old_text_key = &before_view["controls"][1]["rebase_key"];
    assert_ne!(
        old_text_key,
        &restored.body["board"]["controls"][1]["rebase_key"]
    );
    assert_eq!(
        broker
            .monitor_board_response(1, &name, old_response())
            .status,
        409
    );
    broker.monitor_control.lock().unwrap().enable(1);
    assert_eq!(
        broker
            .monitor_board_lifecycle(
                1,
                &name,
                LifecycleRequest {
                    board: name.clone(),
                    expected_revision: read(broker, &name).revision
                },
                false
            )
            .status,
        401
    );
    assert!(read(broker, &name).writable());
    assert!(serde_json::from_value::<LifecycleRequest>(serde_json::json!({"board":"project","expected_revision":"a".repeat(64),"archived_at":0})).is_err());
}

#[tokio::test]
async fn live_identity_reports_expiry_even_when_archive_revision_is_unchanged() {
    let (_root, bound, name) = setup();
    let broker = &bound.broker;
    let before = set(broker, &name, "dated archive");
    broker.monitor_control.lock().unwrap().enable(0);
    let at = crate::db::now();
    let saved = archive(broker, &name, &before.revision, at).unwrap();
    let archived = broker
        .monitor_board_revision(1, &name)
        .unwrap_or_else(|reply| panic!("Unexpected status {}", reply.status));
    {
        let db = broker.db.lock().unwrap();
        board_lifecycle::inspect(
            &db,
            &broker.state_dir,
            &name,
            at + board_lifecycle::RETENTION_MS,
        )
        .unwrap();
    }
    // Wall time is now below the observed deadline; the persisted expiry wins.
    let expired = broker
        .monitor_board_revision(1, &name)
        .unwrap_or_else(|reply| panic!("Unexpected status {}", reply.status));
    assert_eq!(archived.revision, expired.revision);
    assert_ne!(archived, expired);
    assert!(expired.read_only);
    assert_eq!(
        expired.lifecycle.state,
        crate::board::LifecycleState::Purging
    );
    let conditional = broker.monitor_board(1, &name, Some(&saved.revision));
    assert_eq!(conditional.body["unchanged"], true);
    assert_eq!(conditional.body["state"], "purging");
    let result = broker.monitor_board_lifecycle(
        1,
        &name,
        crate::monitor_board::LifecycleRequest {
            board: name.clone(),
            expected_revision: saved.revision,
        },
        true,
    );
    assert_eq!(result.status, 410);
    assert_eq!(result.body["code"], "expired");
}

#[tokio::test]
async fn archived_message_receipt_readback_never_enqueues_an_unknown_old_control() {
    use crate::{
        board_interaction::{self, Kind},
        board_messages::Submission,
    };
    let (_root, bound, name) = setup();
    let broker = &bound.broker;
    let owner = Agent {
        name: "recipient".into(),
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
    let initial = read(broker, &name);
    let added = broker
        .request(Request::AddBoardMessageInput {
            agent: owner.name,
            board: name.clone(),
            expected_revision: initial.revision,
            id: "message".into(),
            label: "Message recipient".into(),
        })
        .unwrap();
    assert_eq!(added["applied"], true);
    let before: Snapshot = serde_json::from_value(added["board"].clone()).unwrap();
    let control = board_interaction::controls(&before.markdown)
        .into_iter()
        .find(|v| matches!(v.kind, Kind::Message { .. }))
        .unwrap();
    let attempt = Submission {
        board: name.clone(),
        expected_revision: before.revision.clone(),
        control_id: board_interaction::control_id(&before, &control),
        id: uuid::Uuid::new_v4().to_string(),
        text: "exact original attempt".into(),
    };
    broker.monitor_control.lock().unwrap().enable(0);
    let accepted = broker.monitor_board_message(1, &name, attempt.clone());
    assert_eq!(accepted.status, 200);
    assert_eq!(accepted.body["accepted"], true);
    let at = crate::db::now();
    let saved = archive(broker, &name, &before.revision, at).unwrap();
    let receipt = broker.monitor_board_message(1, &name, attempt.clone());
    assert_eq!(receipt.status, 200);
    assert_eq!(receipt.body["duplicate"], true);
    let mut unknown = attempt.clone();
    unknown.id = uuid::Uuid::new_v4().to_string();
    let rejected = broker.monitor_board_message(1, &name, unknown.clone());
    assert_eq!(rejected.status, 409);
    assert_eq!(rejected.body["code"], "archived");
    assert_eq!(rejected.body["board"]["read_only"], true);
    let restored = restore(broker, &name, &saved.revision, at + 1).unwrap();
    assert_ne!(
        control.message_key(&before.control_scope()),
        control.message_key(&restored.control_scope())
    );
    assert_eq!(broker.monitor_board_message(1, &name, unknown).status, 409);
    assert_eq!(
        broker.monitor_board_message(1, &name, attempt).body["duplicate"],
        true
    );
    assert_eq!(
        broker.db.lock().unwrap().history(&None, 100).unwrap().len(),
        1
    );
}

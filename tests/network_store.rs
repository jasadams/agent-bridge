use agent_bridge::{
    db::Db,
    network::{
        store::retryable,
        trust::Permission,
        wire::{Envelope, RemoteAgent, StatusEvent, WireResponse},
    },
    protocol::{Agent, Provider},
};

fn peer_id() -> String {
    libp2p_identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id()
        .to_string()
}
fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn user() -> RemoteAgent {
    RemoteAgent {
        name: "user".into(),
        provider: Provider::Inbox,
        session: "user".into(),
    }
}

struct Pair {
    _dir: tempfile::TempDir,
    a: Db,
    b: Db,
    aid: String,
    bid: String,
    bpath: std::path::PathBuf,
}
impl Pair {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut a = Db::open(&dir.path().join("a.sqlite")).unwrap();
        let bpath = dir.path().join("b.sqlite");
        let mut b = Db::open(&bpath).unwrap();
        let aid = peer_id();
        let bid = peer_id();
        a.pair_peer(&bid, &bid, Some("bob")).unwrap();
        b.pair_peer(&aid, &aid, Some("alice")).unwrap();
        for permission in [Permission::Discover, Permission::Send, Permission::Receive] {
            a.grant_peer(&bid, "user", permission, true).unwrap();
            b.grant_peer(&aid, "user", permission, true).unwrap();
        }
        a.replace_remote_agents(&bid, &[user()]).unwrap();
        b.replace_remote_agents(&aid, &[user()]).unwrap();
        Self {
            _dir: dir,
            a,
            b,
            aid,
            bid,
            bpath,
        }
    }
    fn send(&self, body: &str) -> Envelope {
        self.a
            .send_remote(&self.aid, &id(), "user", "bob/user", body, &None)
            .unwrap();
        self.a
            .pending_transfers(&self.bid, 100)
            .unwrap()
            .pop()
            .unwrap()
    }
}

#[test]
fn lost_receipt_duplicate_and_restart_never_replay_native_submission() {
    let mut p = Pair::new();
    let env = p.send("hello");
    p.a.mark_transfer_attempt(&env.id).unwrap();
    p.b.accept_remote(&p.bid, &p.aid, &env).unwrap();
    p.b.accept_remote(&p.bid, &p.aid, &env).unwrap();
    assert_eq!(p.b.history(&None, 100).unwrap().len(), 1);
    let (message, _) = p.b.claim().unwrap().unwrap();
    assert_eq!(message.sender, format!("{}/user", p.aid));
    p.b.finish(&message.id, "submitted", "native accepted")
        .unwrap();
    p.b.accept_remote(&p.bid, &p.aid, &env).unwrap();
    assert!(p.b.claim().unwrap().is_none());
    p.b = Db::open(&p.bpath).unwrap();
    p.b.accept_remote(&p.bid, &p.aid, &env).unwrap();
    assert!(p.b.claim().unwrap().is_none());
    let events = p.b.sync_events(&p.aid, 0).unwrap();
    p.a.apply_events(&p.bid, &events).unwrap();
    assert_eq!(p.a.get(&env.id).unwrap().state, "submitted");
    assert_eq!(
        p.a.transfer_info(&env.id).unwrap().unwrap()["state"],
        "accepted"
    );
    assert!(p.a.pending_transfers(&p.bid, 10).unwrap().is_empty());
    let cursor = p.a.sync_cursor(&p.bid).unwrap();
    p.a.apply_events(&p.bid, &events).unwrap();
    assert_eq!(p.a.sync_cursor(&p.bid).unwrap(), cursor);
    assert!(p.a.retry(&env.id, true).is_err());
}

#[test]
fn opencode_status_sync_preserves_legacy_unknown_without_local_phase_metadata() {
    for (state, phase, expected) in [
        ("queued", "queued", "queued"),
        ("sending", "queued", "sending"),
        ("sending", "dispatched", "sending"),
        ("queued", "reconcile", "uncertain"),
        ("sending", "reconcile", "uncertain"),
        ("queued", "uncertain", "uncertain"),
        ("sending", "uncertain", "uncertain"),
        ("queued", "legacy", "uncertain"),
        ("sending", "legacy", "uncertain"),
        ("queued", "inserted", "uncertain"),
        ("sending", "inserted", "uncertain"),
    ] {
        let mut p = Pair::new();
        let agent = Agent {
            name: "native".into(),
            provider: Provider::OpenCode,
            session: "ses_0123456789abcdefghijklmnop".into(),
            endpoint: format!("/tmp/{}.sock", id()),
            enabled: true,
        };
        p.b.register(&agent, false).unwrap();
        // Native lease/phase APIs are crate-private. This isolated fixture sets
        // only their durable metadata; envelope admission and status production,
        // application, acceptance and restart use the real public store APIs.
        let conn = rusqlite::Connection::open(&p.bpath).unwrap();
        conn.execute(
            "INSERT INTO opencode_leases(session,generation,instance,endpoint,expires,ready) VALUES(?1,?2,?3,?4,?5,1)",
            rusqlite::params![agent.session, id(), id(), agent.endpoint, i64::MAX],
        )
        .unwrap();
        p.b.share_peer_session(&p.aid, &agent.name, true).unwrap();
        p.a.replace_remote_agents(
            &p.bid,
            &[RemoteAgent {
                name: agent.name.clone(),
                provider: agent.provider.clone(),
                session: agent.session.clone(),
            }],
        )
        .unwrap();
        let remote_id = id();
        p.a.send_remote(
            &p.aid,
            &remote_id,
            "user",
            "bob/native",
            "native input",
            &None,
        )
        .unwrap();
        let envelope = p.a.pending_transfers(&p.bid, 1).unwrap().pop().unwrap();
        p.a.mark_transfer_attempt(&remote_id).unwrap();
        p.b.accept_remote(&p.bid, &p.aid, &envelope).unwrap();
        let local_id = p.b.history(&None, 1).unwrap()[0].id.clone();
        if phase == "inserted" {
            // The current delivery phase can already be terminal when old
            // reconciliation events are paged. Preserve their prior attempted
            // provenance in the immutable event history, not only d.phase.
            conn.execute(
                "UPDATE messages SET state='uncertain' WHERE id=?",
                [&local_id],
            )
            .unwrap();
        }
        conn.execute(
            "UPDATE opencode_deliveries SET phase=?1 WHERE id=?2",
            rusqlite::params![phase, local_id],
        )
        .unwrap();
        // Queued fixtures reuse the real acceptance event; sending fixtures
        // produce a transition event. Both must project the local phase when
        // sync_events emits the legacy wire state.
        conn.execute(
            "UPDATE messages SET state=?1,updated=updated+1 WHERE id=?2",
            rusqlite::params![state, local_id],
        )
        .unwrap();
        let events = p.b.sync_events(&p.aid, 0).unwrap();
        assert_eq!(events.last().unwrap().state, expected, "{state}/{phase}");
        assert!(
            events
                .iter()
                .all(|event| matches!(event.state.as_str(), "queued" | "sending" | "uncertain"))
        );
        p.a.apply_events(&p.bid, &events).unwrap();
        assert_eq!(p.a.get(&remote_id).unwrap().state, expected);
        let public = if expected == "uncertain" {
            "unknown"
        } else {
            "pending"
        };
        assert_eq!(p.a.counts().unwrap(), serde_json::json!({public: 1}));
        assert_eq!(
            p.a.transfer_info(&remote_id).unwrap().unwrap()["state"],
            "accepted"
        );
        assert!(p.a.pending_transfers(&p.bid, 10).unwrap().is_empty());
        let sender_conn = rusqlite::Connection::open(p._dir.path().join("a.sqlite")).unwrap();
        assert_eq!(
            sender_conn
                .query_row::<i64, _, _>(
                    "SELECT count(*) FROM opencode_deliveries WHERE id=?",
                    [&remote_id],
                    |row| row.get(0)
                )
                .unwrap(),
            0,
            "sender must rely on the compatible wire state, not receiver-local metadata"
        );
        if expected == "uncertain" {
            p.a = Db::open(&p._dir.path().join("a.sqlite")).unwrap();
            assert_eq!(p.a.get(&remote_id).unwrap().state, "uncertain");
            assert_eq!(p.a.counts().unwrap(), serde_json::json!({"unknown": 1}));
        }
    }
}

#[test]
fn duplicate_acceptance_precedes_changed_permissions_rate_limits_and_session_lookup() {
    let mut p = Pair::new();
    let first = p.send("first");
    p.b.accept_remote(&p.bid, &p.aid, &first).unwrap();
    for n in 1..30 {
        let mut env = first.clone();
        env.id = id();
        env.body = format!("message {n}");
        p.b.accept_remote(&p.bid, &p.aid, &env).unwrap();
    }
    let mut next = first.clone();
    next.id = id();
    let error = p.b.accept_remote(&p.bid, &p.aid, &next).unwrap_err();
    assert!(retryable(&error));
    p.b.accept_remote(&p.bid, &p.aid, &first).unwrap();
    p.b.grant_peer(&p.aid, "user", Permission::Receive, false)
        .unwrap();
    p.b.accept_remote(&p.bid, &p.aid, &first).unwrap();
    assert!(!retryable(
        &p.b.accept_remote(&p.bid, &p.aid, &next).unwrap_err()
    ));
    p.b.revoke_peer(&p.aid).unwrap();
    assert!(p.b.accept_remote(&p.bid, &p.aid, &first).is_err());
}

#[test]
fn envelope_identity_conflicts_and_unrelated_statuses_are_rejected() {
    let p = Pair::new();
    let env = p.send("first");
    p.b.accept_remote(&p.bid, &p.aid, &env).unwrap();
    let mut conflict = env.clone();
    conflict.body = "different".into();
    assert!(p.b.accept_remote(&p.bid, &p.aid, &conflict).is_err());
    let mut forged = env.clone();
    forged.id = id();
    forged.origin_peer = peer_id();
    assert!(p.b.accept_remote(&p.bid, &p.aid, &forged).is_err());
    let mut forwarded = env.clone();
    forwarded.destination_peer = peer_id();
    assert!(p.b.accept_remote(&p.bid, &p.aid, &forwarded).is_err());
    assert!(
        p.a.transfer_result(&p.bid, &env.id, &WireResponse::Accepted { id: id() })
            .is_err()
    );
    let event = StatusEvent {
        sequence: 1,
        id: id(),
        state: "submitted".into(),
        detail: None,
        updated: 1,
    };
    assert!(p.a.apply_events(&p.bid, &[event]).is_err());
    assert_eq!(p.a.sync_cursor(&p.bid).unwrap(), 0);
    assert_eq!(p.b.history(&None, 100).unwrap().len(), 1);
}

#[test]
fn reply_uses_original_parent_session_and_correlates_local_ids() {
    let p = Pair::new();
    let env = p.send("question");
    p.b.accept_remote(&p.bid, &p.aid, &env).unwrap();
    let local = p.b.history(&None, 1).unwrap().pop().unwrap();
    // A reply remains possible from a pinned parent even without directory data.
    p.b.replace_remote_agents(&p.aid, &[]).unwrap();
    let reply_id = id();
    p.b.send_remote(
        &p.bid,
        &reply_id,
        "user",
        "alice/user",
        "answer",
        &Some(local.id),
    )
    .unwrap();
    let reply = p.b.pending_transfers(&p.aid, 1).unwrap().pop().unwrap();
    assert_eq!(reply.hops, 1);
    p.a.accept_remote(&p.aid, &p.bid, &reply).unwrap();
    let answer = p.a.history(&None, 1).unwrap().pop().unwrap();
    assert_eq!(answer.parent, Some(env.id));
    assert_eq!(answer.body, "answer");
    let mut forged = reply;
    forged.id = id();
    forged.hops = 0;
    assert!(p.a.accept_remote(&p.aid, &p.bid, &forged).is_err());
}

#[test]
fn remote_reply_chain_continues_beyond_eight_hops() {
    let p = Pair::new();
    let original = p.send("start");
    p.b.accept_remote(&p.bid, &p.aid, &original).unwrap();
    let mut parent = p.b.history(&None, 1).unwrap()[0].id.clone();
    for hop in 1..=10 {
        let (sender, receiver, source_peer, destination_peer, destination) = if hop % 2 == 1 {
            (&p.b, &p.a, &p.bid, &p.aid, "alice/user")
        } else {
            (&p.a, &p.b, &p.aid, &p.bid, "bob/user")
        };
        let message_id = id();
        sender
            .send_remote(
                source_peer,
                &message_id,
                "user",
                destination,
                "reply",
                &Some(parent),
            )
            .unwrap();
        let envelope = sender
            .pending_transfers(destination_peer, 100)
            .unwrap()
            .into_iter()
            .find(|message| message.id == message_id)
            .unwrap();
        assert_eq!(envelope.hops, hop);
        receiver
            .accept_remote(destination_peer, source_peer, &envelope)
            .unwrap();
        parent = receiver.history(&None, 1).unwrap()[0].id.clone();
    }
}

#[test]
fn remote_reply_hop_boundaries_apply_on_send_and_receive() {
    for hop in [9, 100] {
        let p = Pair::new();
        let original = p.send("question");
        p.b.accept_remote(&p.bid, &p.aid, &original).unwrap();
        let parent = p.b.history(&None, 1).unwrap().pop().unwrap().id;
        // Model a validated prior exchange at the boundary without bypassing
        // the per-minute rate limits with 100 artificial network deliveries.
        let a = rusqlite::Connection::open(p._dir.path().join("a.sqlite")).unwrap();
        let b = rusqlite::Connection::open(&p.bpath).unwrap();
        a.execute(
            "UPDATE outbound_transfers SET envelope=json_set(envelope,'$.hops',?1) WHERE id=?2",
            rusqlite::params![hop - 1, original.id],
        )
        .unwrap();
        b.execute("UPDATE inbound_transfers SET envelope=json_set(envelope,'$.hops',?1) WHERE local_id=?2", rusqlite::params![hop - 1, parent]).unwrap();
        let reply_id = id();
        p.b.send_remote(
            &p.bid,
            &reply_id,
            "user",
            "alice/user",
            "answer",
            &Some(parent.clone()),
        )
        .unwrap();
        let reply = p.b.pending_transfers(&p.aid, 1).unwrap().pop().unwrap();
        assert_eq!(reply.hops, hop);
        p.a.accept_remote(&p.aid, &p.bid, &reply).unwrap();
        if hop == 100 {
            b.execute("UPDATE inbound_transfers SET envelope=json_set(envelope,'$.hops',100) WHERE local_id=?1", [&parent]).unwrap();
            let error =
                p.b.send_remote(
                    &p.bid,
                    &id(),
                    "user",
                    "alice/user",
                    "too far",
                    &Some(parent),
                )
                .unwrap_err();
            assert!(error.to_string().contains("100-hop limit"));
            let mut excessive = reply;
            excessive.id = id();
            excessive.hops = 101;
            let error = p.a.accept_remote(&p.aid, &p.bid, &excessive).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("Invalid message body or hop count")
            );
        }
    }
}

#[test]
fn revocation_blocks_queued_work_and_clears_cached_directory_even_while_paused() {
    let mut p = Pair::new();
    let env = p.send("pending");
    p.b.accept_remote(&p.bid, &p.aid, &env).unwrap();
    p.b.pause(true).unwrap();
    p.b.revoke_peer(&p.aid).unwrap();
    assert_eq!(p.b.history(&None, 1).unwrap()[0].state, "blocked");
    assert_eq!(p.b.remote_agents().unwrap(), serde_json::json!([]));
    p.b.pause(false).unwrap();
    assert!(p.b.claim().unwrap().is_none());
    p.a.mark_transfer_attempt(&env.id).unwrap();
    assert!(p.a.cancel(&env.id).is_err());
    p.a.revoke_peer(&p.bid).unwrap();
    assert_eq!(
        p.a.transfer_info(&env.id).unwrap().unwrap()["state"],
        "blocked"
    );
    assert!(p.a.cancel(&env.id).is_err());
}

#[test]
fn immutable_outbound_survives_directory_replacement_and_idempotent_retries() {
    let p = Pair::new();
    let env = p.send("original");
    p.a.replace_remote_agents(
        &p.bid,
        &[RemoteAgent {
            session: "replacement".into(),
            ..user()
        }],
    )
    .unwrap();
    p.a.send_remote(&p.aid, &env.id, "user", "bob/user", "original", &None)
        .unwrap();
    assert_eq!(p.a.pending_transfers(&p.bid, 1).unwrap()[0], env);
    assert!(
        p.a.send_remote(&p.aid, &env.id, "user", "bob/user", "conflict", &None)
            .is_err()
    );
    let next = p.send("new");
    assert_eq!(next.destination.session, "replacement");
    assert!(p.b.accept_remote(&p.bid, &p.aid, &next).is_err());
    assert!(p.a.cancel(&next.id).is_ok());
}

#[test]
fn configuration_addresses_and_exports_are_validated_and_persisted() {
    let mut p = Pair::new();
    assert!(!p.a.network_config().unwrap().enabled);
    assert!(
        p.a.set_network_config(true, Some("/ip4/0.0.0.0/udp/0/quic-v1"))
            .is_ok()
    );
    assert!(
        p.a.set_peer_address("bob", "/ip4/0.0.0.0/udp/0/quic-v1")
            .is_err()
    );
    p.a.set_peer_address("bob", "/ip4/127.0.0.1/udp/1234/quic-v1")
        .unwrap();
    assert_eq!(p.a.network_peers().unwrap().len(), 1);
    assert!(
        p.a.set_network_config(false, Some("/ip4/127.0.0.1/tcp/1234/ws"))
            .is_err()
    );
    assert!(p.a.network_config().unwrap().enabled);
    assert_eq!(p.a.export_agents(&p.bid).unwrap(), vec![user()]);
    p.a.grant_peer(&p.bid, "user", Permission::Discover, false)
        .unwrap();
    assert!(p.a.export_agents(&p.bid).unwrap().is_empty());
    assert!(
        p.a.replace_remote_agents(&p.bid, &[user(), user()])
            .is_err()
    );
    assert!(
        p.a.replace_remote_agents(
            &p.bid,
            &[RemoteAgent {
                name: "bad/name".into(),
                ..user()
            }]
        )
        .is_err()
    );
}

#[test]
fn interrupted_native_delivery_is_uncertain_and_unicode_details_are_bounded() {
    let mut p = Pair::new();
    let env = p.send("first");
    p.b.accept_remote(&p.bid, &p.aid, &env).unwrap();
    let (message, _) = p.b.claim().unwrap().unwrap();
    p.b = Db::open(&p.bpath).unwrap();
    assert_eq!(p.b.get(&message.id).unwrap().state, "uncertain");
    assert!(p.b.claim().unwrap().is_none());
    p.b.retry(&message.id, true).unwrap();
    let (message, _) = p.b.claim().unwrap().unwrap();
    p.b.finish(&message.id, "failed", &"🦀".repeat(1000))
        .unwrap();
    let events = p.b.sync_events(&p.aid, 0).unwrap();
    assert!(
        events
            .iter()
            .all(|e| e.detail.as_ref().is_none_or(|s| s.len() <= 2000))
    );
    p.a.apply_events(&p.bid, &events).unwrap();
    assert_eq!(p.a.get(&env.id).unwrap().state, "failed");
}

#[test]
fn destination_replacement_never_redirects_an_accepted_or_pending_message() {
    let mut p = Pair::new();
    p.b.register(
        &Agent {
            name: "sink".into(),
            provider: Provider::Inbox,
            session: "original".into(),
            endpoint: String::new(),
            enabled: true,
        },
        false,
    )
    .unwrap();
    p.b.grant_peer(&p.aid, "sink", Permission::Receive, true)
        .unwrap();
    let mut env = p.send("pinned");
    env.destination = RemoteAgent {
        name: "sink".into(),
        provider: Provider::Inbox,
        session: "original".into(),
    };
    p.b.accept_remote(&p.bid, &p.aid, &env).unwrap();
    p.b.register(
        &Agent {
            name: "sink".into(),
            provider: Provider::Inbox,
            session: "replacement".into(),
            endpoint: String::new(),
            enabled: true,
        },
        true,
    )
    .unwrap();
    p.b.accept_remote(&p.bid, &p.aid, &env).unwrap();
    assert!(p.b.claim().unwrap().is_none());
    let mut new = env;
    new.id = id();
    assert!(p.b.accept_remote(&p.bid, &p.aid, &new).is_err());
}

#[test]
fn serialized_status_pages_fit_even_when_details_expand_as_json_controls() {
    let mut p = Pair::new();
    let env = p.send("bounded status");
    p.b.accept_remote(&p.bid, &p.aid, &env).unwrap();
    for _ in 0..100 {
        let (message, _) = p.b.claim().unwrap().unwrap();
        p.b.finish(&message.id, "failed", &"\0".repeat(2000))
            .unwrap();
        p.b.retry(&message.id, false).unwrap();
    }
    let first = p.b.sync_events(&p.aid, 0).unwrap();
    assert!(first.len() < 100);
    assert!(!first.is_empty());
    let mut pages = 0;
    loop {
        let page =
            p.b.sync_events(&p.aid, p.a.sync_cursor(&p.bid).unwrap())
                .unwrap();
        if page.is_empty() {
            break;
        }
        let encoded = serde_json::to_vec(&WireResponse::Status {
            events: page.clone(),
        })
        .unwrap();
        assert!(encoded.len() <= agent_bridge::protocol::MAX_FRAME);
        p.a.apply_events(&p.bid, &page).unwrap();
        pages += 1;
    }
    assert!(pages > 1);
    assert_eq!(p.a.get(&env.id).unwrap().state, "queued");
}

#[test]
fn late_rejection_does_not_overwrite_an_existing_durable_acceptance() {
    let p = Pair::new();
    let env = p.send("accepted");
    p.a.mark_transfer_attempt(&env.id).unwrap();
    p.a.transfer_result(
        &p.bid,
        &env.id,
        &WireResponse::Accepted { id: env.id.clone() },
    )
    .unwrap();
    p.a.transfer_result(
        &p.bid,
        &env.id,
        &WireResponse::Rejected {
            reason: "late reply".into(),
            retryable: false,
        },
    )
    .unwrap();
    assert_eq!(
        p.a.transfer_info(&env.id).unwrap().unwrap()["state"],
        "accepted"
    );
    assert_eq!(p.a.get(&env.id).unwrap().state, "queued");
}

#[test]
fn blocked_inbound_can_be_explicitly_retried_after_trust_is_restored() {
    let mut p = Pair::new();
    let env = p.send("blocked");
    p.b.accept_remote(&p.bid, &p.aid, &env).unwrap();
    let local = p.b.history(&None, 1).unwrap().pop().unwrap();
    p.b.revoke_peer(&p.aid).unwrap();
    assert_eq!(p.b.get(&local.id).unwrap().state, "blocked");
    p.b.pair_peer(&p.aid, &p.aid, None).unwrap();
    p.b.grant_peer(&p.aid, "user", Permission::Receive, true)
        .unwrap();
    p.b.retry(&local.id, false).unwrap();
    assert_eq!(p.b.claim().unwrap().unwrap().0.id, local.id);
    assert!(p.b.claim().unwrap().is_none());
}

#[test]
fn inbound_retry_cannot_redirect_to_another_provider_with_the_same_session_id() {
    let mut p = Pair::new();
    let session = id();
    p.b.register(
        &Agent {
            name: "sink".into(),
            provider: Provider::Inbox,
            session: session.clone(),
            endpoint: String::new(),
            enabled: true,
        },
        false,
    )
    .unwrap();
    p.b.grant_peer(&p.aid, "sink", Permission::Receive, true)
        .unwrap();
    let mut env = p.send("provider pinned");
    env.destination = RemoteAgent {
        name: "sink".into(),
        provider: Provider::Inbox,
        session: session.clone(),
    };
    p.b.accept_remote(&p.bid, &p.aid, &env).unwrap();
    let local = p.b.history(&None, 1).unwrap().pop().unwrap();
    p.b.register(
        &Agent {
            name: "sink".into(),
            provider: Provider::Codex,
            session,
            endpoint: String::new(),
            enabled: true,
        },
        true,
    )
    .unwrap();
    assert!(p.b.retry(&local.id, false).is_err());
    assert!(p.b.claim().unwrap().is_none());
    assert_eq!(p.b.get(&local.id).unwrap().state, "failed");
}

#[test]
fn retained_history_accounts_utf8_body_copies_and_survives_schema_nine_upgrade() {
    let mut p = Pair::new();
    let env = p.send("retained é🦀\0tail");
    p.b.accept_remote(&p.bid, &p.aid, &env).unwrap();
    let conn = rusqlite::Connection::open(&p.bpath).unwrap();
    let expected = (env.body.len() + serde_json::to_string(&env).unwrap().len()) as i64;
    let usage: (i64, i64) = conn
        .query_row(
            "SELECT messages,bytes FROM inbound_usage WHERE peer_id=?",
            [&p.aid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(usage, (1, expected));
    conn.execute("UPDATE messages SET state='submitted' WHERE id IN (SELECT local_id FROM inbound_transfers)",[]).unwrap();
    assert_eq!(
        conn.query_row::<i64, _, _>(
            "SELECT bytes FROM inbound_usage WHERE peer_id=?",
            [&p.aid],
            |r| r.get(0)
        )
        .unwrap(),
        expected
    );
    conn.execute_batch(include_str!("fixtures/native-metadata-v10.sql"))
        .unwrap();
    conn.execute_batch("DROP TRIGGER inbound_usage_insert; DROP TRIGGER inbound_usage_delete; DROP TABLE inbound_usage; PRAGMA user_version=9;").unwrap();
    drop(conn);
    p.b = Db::open(&p.bpath).unwrap();
    let conn = rusqlite::Connection::open(&p.bpath).unwrap();
    assert_eq!(
        conn.query_row::<i64, _, _>(
            "SELECT bytes FROM inbound_usage WHERE peer_id=?",
            [&p.aid],
            |r| r.get(0)
        )
        .unwrap(),
        expected
    );
    p.b.accept_remote(&p.bid, &p.aid, &env).unwrap();
    assert_eq!(
        conn.query_row::<i64, _, _>(
            "SELECT messages FROM inbound_usage WHERE peer_id=?",
            [&p.aid],
            |r| r.get(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn retained_history_quota_rejects_completed_message_growth_without_breaking_dedup() {
    for boundary in ["peer-bytes", "peer-rows", "global-bytes", "global-rows"] {
        let mut p = Pair::new();
        let first = p.send("first");
        p.b.accept_remote(&p.bid, &p.aid, &first).unwrap();
        let next = p.send("more é🦀");
        let cost = (next.body.len() + serde_json::to_string(&next).unwrap().len()) as i64;
        let other = peer_id();
        p.b.pair_peer(&other, &other, None).unwrap();
        let conn = rusqlite::Connection::open(&p.bpath).unwrap();
        conn.execute("UPDATE messages SET state='submitted' WHERE id IN (SELECT local_id FROM inbound_transfers)",[]).unwrap();
        match boundary {
            "peer-bytes" => {
                conn.execute(
                    "UPDATE inbound_usage SET bytes=? WHERE peer_id=?",
                    rusqlite::params![64 * 1024 * 1024 - cost, &p.aid],
                )
                .unwrap();
                p.b.accept_remote(&p.bid, &p.aid, &next).unwrap();
            }
            "peer-rows" => {
                conn.execute(
                    "UPDATE inbound_usage SET messages=8192 WHERE peer_id=?",
                    [&p.aid],
                )
                .unwrap();
            }
            "global-bytes" => {
                conn.execute(
                    "INSERT INTO inbound_usage VALUES(?,1,?)",
                    rusqlite::params![&other, 256 * 1024 * 1024 - cost],
                )
                .unwrap();
            }
            "global-rows" => {
                conn.execute("INSERT INTO inbound_usage VALUES(?,32767,0)", [&other])
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let failed = p.send("another");
        let before: i64 = conn
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        let error = p.b.accept_remote(&p.bid, &p.aid, &failed).unwrap_err();
        assert!(
            error.to_string().contains("history quota"),
            "{boundary}: {error}"
        );
        assert!(retryable(&error));
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT count(*) FROM messages", [], |r| r.get(0))
                .unwrap(),
            before
        );
        p.b.accept_remote(&p.bid, &p.aid, &first).unwrap();
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT count(*) FROM messages", [], |r| r.get(0))
                .unwrap(),
            before
        );
    }
}

#[test]
fn codex_inbound_uses_canonical_alias_admission_and_duplicates_precede_pressure() {
    let mut p = Pair::new();
    let agent = Agent {
        name: "native".into(),
        provider: Provider::Codex,
        session: "123e4567-e89b-12d3-a456-426614174000".into(),
        endpoint: String::new(),
        enabled: true,
    };
    p.b.register(&agent, false).unwrap();
    let mut alias = agent.clone();
    alias.name = "alias".into();
    p.b.register(&alias, false).unwrap();
    p.b.grant_peer(&p.aid, &agent.name, Permission::Receive, true)
        .unwrap();
    p.a.replace_remote_agents(
        &p.bid,
        &[RemoteAgent {
            name: agent.name.clone(),
            provider: agent.provider.clone(),
            session: agent.session.clone(),
        }],
    )
    .unwrap();
    let remote_id = id();
    p.a.send_remote(&p.aid, &remote_id, "user", "bob/native", "first", &None)
        .unwrap();
    let first = p.a.pending_transfers(&p.bid, 100).unwrap().pop().unwrap();
    p.b.accept_remote(&p.bid, &p.aid, &first).unwrap();
    let (claimed, _) = p.b.claim().unwrap().unwrap();
    p.b.finish(&claimed.id, "submitted", "native accepted")
        .unwrap();
    assert_eq!(p.b.get(&claimed.id).unwrap().state, "submitted");
    // Delivered history does not consume admission capacity: all 64 genuinely
    // pending alias messages must still fit beside the completed remote message.
    for _ in 0..64 {
        p.b.send(&id(), "user", &alias.name, "local", &None)
            .unwrap();
    }
    p.b.accept_remote(&p.bid, &p.aid, &first).unwrap();
    let next_id = id();
    p.a.send_remote(&p.aid, &next_id, "user", "bob/native", "full", &None)
        .unwrap();
    let next =
        p.a.pending_transfers(&p.bid, 100)
            .unwrap()
            .into_iter()
            .find(|e| e.id == next_id)
            .unwrap();
    let error = p.b.accept_remote(&p.bid, &p.aid, &next).unwrap_err();
    assert!(retryable(&error));
    assert!(error.to_string().contains("64 pending"));
    let history = p.b.history(&None, 100).unwrap();
    assert_eq!(history.len(), 65);
    assert_eq!(
        history.iter().filter(|row| row.state == "queued").count(),
        64
    );
    assert_eq!(
        history
            .iter()
            .filter(|row| row.state == "submitted")
            .count(),
        1
    );
}

#[test]
fn outbound_remote_uuid_collision_never_uses_local_codex_slots_or_concurrency() {
    let mut p = Pair::new();
    let session = "123e4567-e89b-12d3-a456-426614174000";
    let local = Agent {
        name: "local-native".into(),
        provider: Provider::Codex,
        session: session.into(),
        endpoint: String::new(),
        enabled: true,
    };
    p.a.register(&local, false).unwrap();
    p.a.replace_remote_agents(
        &p.bid,
        &[RemoteAgent {
            name: "remote-native".into(),
            provider: Provider::Codex,
            session: session.into(),
        }],
    )
    .unwrap();
    let mut outgoing = Vec::new();
    for _ in 0..64 {
        let identifier = id();
        p.a.send_remote(
            &p.aid,
            &identifier,
            "user",
            "bob/remote-native",
            "remote",
            &None,
        )
        .unwrap();
        outgoing.push(identifier);
    }
    // Simulate a concurrent remote transport attempt with the identical UUID.
    let conn = rusqlite::Connection::open(p._dir.path().join("a.sqlite")).unwrap();
    conn.execute(
        "UPDATE messages SET state='sending' WHERE id=?",
        [&outgoing[0]],
    )
    .unwrap();
    let message =
        p.a.send(&id(), "user", &local.name, "local", &None)
            .unwrap();
    assert_eq!(p.a.claim().unwrap().unwrap().0.id, message.id);
}

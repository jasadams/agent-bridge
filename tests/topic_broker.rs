use agent_bridge::{broker::Broker, protocol::Request};
use serde_json::{Value, json};

fn request(broker: &Broker, value: Value) -> anyhow::Result<Value> {
    broker.request(serde_json::from_value::<Request>(value)?)
}

#[tokio::test]
async fn local_topic_contract_roundtrip_and_no_delivery_work() {
    let root = tempfile::tempdir().unwrap();
    let bound = Broker::bind(root.path().join("state"), "/missing/codex".into()).unwrap();
    let broker = &bound.broker;
    let created = request(broker, json!({"op":"topic_create","name":"Engineering"})).unwrap();
    let topic = created["reference"].clone();
    assert!(topic["bridge_id"].as_str().is_some());
    assert_eq!(created["high_water"], 0);
    let local = request(broker, json!({"op":"identity"})).unwrap();
    assert_eq!(topic["bridge_id"], local["peer_id"]);
    let omitted = json!({"topic_id":topic["topic_id"]});
    assert_eq!(
        request(broker, json!({"op":"topic_inspect","topic":omitted})).unwrap(),
        created
    );
    let id = uuid::Uuid::new_v4().to_string();
    let publication = json!({"op":"topic_publish","topic":omitted,"id":id,
        "author":{"agent_id":"inbox-user"},"body":"Ready"});
    let accepted = request(broker, publication.clone()).unwrap();
    assert_eq!(accepted["duplicate"], false);
    assert_eq!(accepted["post"]["sequence"], 1);
    assert_eq!(accepted["post"]["author"]["bridge_id"], local["peer_id"]);
    assert_eq!(accepted["post"]["topic"], topic);
    let retry = request(broker, publication).unwrap();
    assert_eq!(retry["duplicate"], true);
    assert_eq!(retry["post"], accepted["post"]);
    let read = request(broker, json!({"op":"topic_read","topic":topic})).unwrap();
    assert_eq!(read["posts"], json!([accepted["post"]]));
    assert_eq!(read["high_water"], 1);
    assert_eq!(read["has_more"], false);
    assert!(read["next_cursor"].is_string());
    assert_eq!(
        request(broker, json!({"op":"topic_list"})).unwrap()["topics"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // Core publication does not enter the existing DM/native delivery lifecycle.
    assert_eq!(
        request(broker, json!({"op":"history","limit":100})).unwrap(),
        json!([])
    );
    assert!(
        request(
            broker,
            json!({"op":"topic_publish","topic":topic,
        "id":uuid::Uuid::new_v4().to_string(),"author":{"agent_id":"user"},
        "body":"No name targeting"})
        )
        .is_err()
    );
}

#[tokio::test]
async fn archive_restore_rename_and_foreign_scope_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let bound = Broker::bind(root.path().join("state"), "/missing/codex".into()).unwrap();
    let broker = &bound.broker;
    let first = request(broker, json!({"op":"topic_create","name":"Same"})).unwrap();
    let second = request(broker, json!({"op":"topic_create","name":"Same"})).unwrap();
    assert_ne!(first["reference"], second["reference"]);
    let topic = first["reference"].clone();
    let renamed = request(
        broker,
        json!({"op":"topic_rename","topic":topic,"name":"New"}),
    )
    .unwrap();
    assert_eq!(renamed["reference"], topic);
    assert_eq!(renamed["name"], "New");
    let archived = request(broker, json!({"op":"topic_archive","topic":topic})).unwrap();
    assert_eq!(archived["archived"], true);
    let listed = request(broker, json!({"op":"topic_list"})).unwrap();
    assert_eq!(listed["topics"].as_array().unwrap().len(), 1);
    let all = request(broker, json!({"op":"topic_list","include_archived":true})).unwrap();
    assert_eq!(all["topics"].as_array().unwrap().len(), 2);
    assert!(
        request(
            broker,
            json!({"op":"topic_publish","topic":topic,
        "id":uuid::Uuid::new_v4().to_string(),"author":{"agent_id":"inbox-user"},
        "body":"Not while archived"})
        )
        .unwrap_err()
        .to_string()
        .contains("topic_archived")
    );
    request(broker, json!({"op":"topic_read","topic":topic,"cursor":""})).unwrap();
    assert_eq!(
        request(broker, json!({"op":"topic_restore","topic":topic})).unwrap()["reference"],
        topic
    );
    let foreign = libp2p_identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id()
        .to_string();
    assert!(
        request(
            broker,
            json!({"op":"topic_inspect","topic":{
        "bridge_id":foreign,"topic_id":topic["topic_id"]}})
        )
        .unwrap_err()
        .to_string()
        .contains("topic_not_local")
    );
    assert!(request(broker, json!({"op":"topic_read","topic":topic,"limit":101})).is_err());
    assert!(request(broker, json!({"op":"topic_read","topic":topic,"limit":0})).is_err());
    assert!(
        request(
            broker,
            json!({"op":"topic_read","topic":topic,"cursor":"bad"})
        )
        .is_err()
    );
    assert!(request(broker, json!({"op":"topic_read","topic":topic,"before":""})).is_err());
}

#[test]
fn reference_and_request_fields_are_strict() {
    let read: Request = serde_json::from_value(json!({"op":"topic_read", "topic":{
        "topic_id":"00000000-0000-4000-8000-000000000001"}}))
    .unwrap();
    assert!(matches!(read, Request::TopicRead { limit: 50, .. }));
    let history: Request = serde_json::from_value(json!({"op":"history"})).unwrap();
    assert!(matches!(history, Request::History { limit: 30, .. }));
    for value in [
        json!({"op":"topic_inspect","topic":{"topic_id":"x","name":"alias"}}),
        json!({"op":"topic_publish","topic":{"topic_id":"x"},"id":"x",
            "author":{"agent_id":"inbox-user","name":"alias"},"body":"x"}),
        json!({"op":"topic_create","name":"x","members":[]}),
    ] {
        assert!(serde_json::from_value::<Request>(value).is_err());
    }
}

#[tokio::test]
async fn bounded_older_pages_and_catalog_use_ids_not_names() {
    let root = tempfile::tempdir().unwrap();
    let bound = Broker::bind(root.path().join("state"), "/missing/codex".into()).unwrap();
    let broker = &bound.broker;
    for (name, session) in [
        ("alias-one", "worker-one"),
        ("alias-two", "worker-one"),
        ("other", "worker-two"),
    ] {
        request(
            broker,
            json!({"op":"register","agent":{"name":name,
            "provider":"inbox","session":session,"endpoint":"","enabled":name != "other"}}),
        )
        .unwrap();
    }
    let catalog = request(broker, json!({"op":"topic_agents"})).unwrap();
    let agents = catalog["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 3); // user plus two identities, not three aliases
    assert!(
        agents
            .iter()
            .any(|agent| agent["reference"]["agent_id"] == "inbox-worker-one")
    );
    assert!(
        agents
            .iter()
            .any(|agent| agent["reference"]["agent_id"] == "inbox-worker-two")
    );
    assert!(
        !agents
            .iter()
            .any(|agent| agent["reference"]["agent_id"] == "alias-one")
    );
    let topic =
        request(broker, json!({"op":"topic_create","name":"Pages"})).unwrap()["reference"].clone();
    let publish = |n| {
        request(
            broker,
            json!({"op":"topic_publish","topic":topic,
        "id":uuid::Uuid::new_v4().to_string(),"author":{"agent_id":"inbox-worker-one"},
        "body":format!("post {n}"),"mentions":[{"agent_id":"inbox-worker-two"}]}),
        )
        .unwrap()
    };
    for n in 1..=105 {
        publish(n);
    }
    let latest = request(broker, json!({"op":"topic_read","topic":topic})).unwrap();
    assert_eq!(latest["posts"].as_array().unwrap().len(), 50);
    assert_eq!(latest["posts"][0]["sequence"], 56);
    assert_eq!(latest["posts"][49]["sequence"], 105);
    assert_eq!(latest["has_older"], true);
    assert_eq!(latest["has_more"], false);
    publish(106); // appending cannot shift the older-page position
    let older = request(
        broker,
        json!({"op":"topic_read","topic":topic,
        "before":latest["previous_cursor"]}),
    )
    .unwrap();
    assert_eq!(older["posts"][0]["sequence"], 6);
    assert_eq!(older["posts"][49]["sequence"], 55);
    assert_eq!(older["has_older"], true);
    assert_eq!(older["has_more"], true);
    assert_eq!(older["high_water"], 106);
    let oldest = request(
        broker,
        json!({"op":"topic_read","topic":topic,
        "before":older["previous_cursor"]}),
    )
    .unwrap();
    assert_eq!(oldest["posts"].as_array().unwrap().len(), 5);
    assert_eq!(oldest["posts"][0]["sequence"], 1);
    assert_eq!(oldest["has_older"], false);
    let appended = request(
        broker,
        json!({"op":"topic_read","topic":topic,
        "cursor":latest["next_cursor"]}),
    )
    .unwrap();
    assert_eq!(appended["posts"].as_array().unwrap().len(), 1);
    assert_eq!(appended["posts"][0]["sequence"], 106);
    let different =
        request(broker, json!({"op":"topic_create","name":"Pages"})).unwrap()["reference"].clone();
    assert!(
        request(
            broker,
            json!({"op":"topic_read","topic":different,
        "before":latest["previous_cursor"]})
        )
        .is_err()
    );
    assert!(
        request(
            broker,
            json!({"op":"topic_read","topic":topic,
        "before":latest["previous_cursor"],"cursor":latest["next_cursor"]})
        )
        .is_err()
    );
}

#[tokio::test]
async fn topic_mentions_do_not_submit_native_input_or_inherit_alias_replacement() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let endpoint = root.path().join("native.sock");
    let listener = UnixListener::bind(&endpoint).unwrap();
    std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let state = root.path().join("state");
    let bound = Broker::bind(state, "/missing/codex".into()).unwrap();
    let broker = &bound.broker;
    let session = uuid::Uuid::new_v4().to_string();
    let register = |session: &str, replace| {
        request(
            broker,
            json!({"op":"register",
        "agent":{"name":"reviewer","provider":"claude","session":session,
            "endpoint":endpoint.to_str().unwrap(),"enabled":true},"replace":replace}),
        )
        .unwrap()
    };
    register(&session, false);
    let topic =
        request(broker, json!({"op":"topic_create","name":"No push"})).unwrap()["reference"]
            .clone();
    let canonical = format!("claude-{session}");
    let accepted = request(
        broker,
        json!({"op":"topic_publish","topic":topic,
        "id":uuid::Uuid::new_v4().to_string(),"author":{"agent_id":"inbox-user"},
        "body":"Mention only", "mentions":[{"agent_id":canonical}]}),
    )
    .unwrap();
    assert_eq!(accepted["post"]["mentions"][0]["agent_id"], canonical);
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    register(&uuid::Uuid::new_v4().to_string(), true);
    assert!(
        request(
            broker,
            json!({"op":"topic_publish","topic":topic,
        "id":uuid::Uuid::new_v4().to_string(),"author":{"agent_id":canonical},
        "body":"Old identity cannot use replacement"})
        )
        .is_err()
    );
    let read = request(broker, json!({"op":"topic_read","topic":topic})).unwrap();
    assert_eq!(read["posts"][0]["mentions"][0]["agent_id"], canonical);
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
}

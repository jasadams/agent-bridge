use agent_bridge::{
    db::Db,
    protocol::{Agent, Provider},
};

fn register(db: &mut Db, provider: Provider, session: &str, name: &str) -> String {
    db.register(
        &Agent {
            name: name.into(),
            provider,
            session: session.into(),
            endpoint: String::new(),
        },
        false,
    )
    .unwrap();
    name.into()
}

#[test]
fn explicit_aliases_are_the_only_native_names_and_titles_never_route() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open(&dir.path().join("db")).unwrap();
    let s1 = uuid::Uuid::new_v4().to_string();
    let s2 = uuid::Uuid::new_v4().to_string();
    let claude = register(&mut db, Provider::Claude, &s1, "claude-a");
    let codex = register(&mut db, Provider::Codex, &s1, "codex-a");
    assert_eq!(db.agent(&claude).unwrap().session, s1);
    assert_eq!(db.agent(&codex).unwrap().session, s1);
    // Display-like, bare and qualified titles do not resolve.
    for title in ["shared", "claude:shared", "codex:shared", "Coordinator"] {
        assert!(db.agent(title).is_err(), "{title}");
    }
    // An explicit alias never shadows a reserved canonical native route.
    assert!(
        db.register(
            &Agent {
                name: format!("claude-{s2}"),
                provider: Provider::Claude,
                session: s2.clone(),
                endpoint: String::new(),
            },
            false,
        )
        .is_err()
    );
    // Rebinding an alias does not redirect the immutable destination of a
    // stored message.
    let id = uuid::Uuid::new_v4().to_string();
    db.send_names(&id, "user", &claude, "first", &None).unwrap();
    db.register(
        &Agent {
            name: claude.clone(),
            provider: Provider::Claude,
            session: s2.clone(),
            endpoint: String::new(),
        },
        true,
    )
    .unwrap();
    assert!(db.send_names(&id, "user", &claude, "first", &None).is_err());
    assert_eq!(db.get(&id).unwrap().recipient_session, s1);
    // Manual Inbox configuration remains separate from native aliases.
    db.register(
        &Agent {
            name: "shared".into(),
            provider: Provider::Inbox,
            session: "manual".into(),
            endpoint: String::new(),
        },
        false,
    )
    .unwrap();
    assert_eq!(db.agent("shared").unwrap().session, "manual");
    assert_eq!(db.agent("user").unwrap().provider, Provider::Inbox);
    // Removing an explicit alias removes only that route. Reserved canonical
    // native routes cannot be unregistered because no membership is stored.
    db.unregister(&claude, &s2).unwrap();
    assert!(db.agent(&claude).is_err());
    assert!(db.unregister(&format!("claude-{s1}"), &s1).is_err());
    assert_eq!(db.agent(&codex).unwrap().session, s1);
    assert!(db.agent("unknown").is_err());
    assert!("unknown".parse::<Provider>().is_err());
}

use agent_bridge::{
    db::Db,
    protocol::{Agent, Provider},
};

fn agent(session: &str) -> Agent {
    Agent {
        name: format!("claude-{session}"),
        provider: Provider::Claude,
        session: session.into(),
        endpoint: String::new(),
        enabled: true,
    }
}

// The durable overlay still resolves explicit aliases independently of native discovery.
#[test]
fn provider_qualified_names_distinguish_cross_provider_collisions_and_manual_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open(&dir.path().join("db")).unwrap();
    let cc = uuid::Uuid::new_v4().to_string();
    let cx = uuid::Uuid::new_v4().to_string();
    db.register(&agent(&cc), false).unwrap();
    db.observe_claude_name(&cc, "same-name").unwrap();
    db.observe_codex(&cx, Some("same-name"), true).unwrap();
    assert!(db.resolve_agent("same-name").is_err());
    assert_eq!(db.resolve_agent("claude:same-name").unwrap().session, cc);
    assert_eq!(db.resolve_agent("codex:same-name").unwrap().session, cx);
    let mut alias = db.resolve_agent(&format!("codex-{cx}")).unwrap();
    alias.name = "same-name".into();
    db.register(&alias, false).unwrap();
    assert_eq!(db.resolve_agent("same-name").unwrap().session, cx);
    assert_eq!(db.resolve_agent("claude:same-name").unwrap().session, cc);
    db.observe_claude_name(&cc, "renamed").unwrap();
    assert!(db.resolve_agent("claude:same-name").is_err());
    assert_eq!(db.resolve_agent("claude:renamed").unwrap().session, cc);
    let other = uuid::Uuid::new_v4().to_string();
    db.register(&agent(&other), false).unwrap();
    db.observe_claude_name(&other, "renamed").unwrap();
    assert!(db.resolve_agent("claude:renamed").is_err());
}

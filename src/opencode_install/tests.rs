use super::*;
use std::os::unix::fs::symlink;
struct Fixture {
    _root: tempfile::TempDir,
    env: Environment,
    state: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().canonicalize().unwrap();
        let env = Environment {
            global: canonical.join("config/opencode"),
            data: canonical.join("data/opencode"),
            custom: None,
            extra: None,
            inline: None,
            binary: canonical.join("bin with spaces/agent-bridge"),
        };
        let state = canonical.join("state");
        directories(&env.global, true).unwrap();
        directories(&state, true).unwrap();
        fs::write(
            state.join("sentinel.sqlite"),
            b"broker database/grants unchanged",
        )
        .unwrap();
        Self {
            _root: root,
            env,
            state,
        }
    }
    fn target(&self) -> PathBuf {
        self.env.global.join("opencode.jsonc")
    }
    fn run(&self, install: bool) -> Result<PathBuf> {
        run(&self.env, &self.state, None, install)
    }
}
fn snapshots(path: &Path) -> Vec<Vec<u8>> {
    fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .contains("agent-bridge-backup-")
        })
        .map(|p| fs::read(p).unwrap())
        .collect()
}
#[test]
fn install_repeat_update_remove_preserve_unrelated_jsonc_and_state() {
    let mut f = Fixture::new();
    let original = "// header\r\n{\r\n  \"plugin\": [\"unrelated@1\",],\r\n  \"mcp\": {\"other\": {\"command\":[\"other\"],},},\r\n  // permissions comment\r\n  \"permission\" : {\"bash\":\"ask\",},\r\n}\r\n";
    fs::write(f.target(), original).unwrap();
    f.run(true).unwrap();
    let installed = read(&f.target()).unwrap().unwrap();
    assert!(installed.contains("// header\r\n"));
    assert!(installed.contains("\"permission\" : {\"bash\":\"ask\",}"));
    assert!(installed.contains("bin with spaces"));
    assert!(installed.contains("file:///"));
    assert!(!installed.replace("\r\n", "").contains('\n'));
    let backups = snapshots(&f.target());
    assert_eq!(backups, vec![original.as_bytes()]);
    f.run(true).unwrap();
    assert_eq!(read(&f.target()).unwrap().unwrap(), installed);
    assert_eq!(snapshots(&f.target()).len(), 1);
    f.env.binary = f.state.parent().unwrap().join("upgraded/agent-bridge");
    f.run(true).unwrap();
    assert!(
        read(&f.target())
            .unwrap()
            .unwrap()
            .contains("upgraded/agent-bridge")
    );
    f.run(false).unwrap();
    let removed = read(&f.target()).unwrap().unwrap();
    assert!(removed.contains("unrelated@1"));
    assert!(removed.contains("\"other\""));
    assert!(removed.contains("// permissions comment\r\n"));
    assert!(!removed.contains("file:///"));
    assert!(!removed.contains("\"agent-bridge\""));
    assert_eq!(
        fs::read(f.state.join("sentinel.sqlite")).unwrap(),
        b"broker database/grants unchanged"
    );
    f.run(false).unwrap();
}
#[test]
fn extracted_runtime_closure_is_exact_and_file_urls_escape_bytes() {
    let f = Fixture::new();
    f.run(true).unwrap();
    let manifest: Manifest = serde_json::from_str(
        &read(&sidecar(&f.target(), "manifest.json").unwrap())
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    for (name, text) in ASSETS {
        assert_eq!(
            read(&manifest.assets[0].join(name)).unwrap().unwrap(),
            *text
        );
    }
    assert_eq!(
        url(Path::new("/tmp/space #é/index.ts")),
        "file:///tmp/space%20%23%C3%A9/index.ts"
    );
}
#[test]
fn global_array_owner_is_selected_and_explicit_shadowing_refused() {
    let f = Fixture::new();
    let lower = f.env.global.join("opencode.json");
    fs::write(&lower, "{\"plugin\":[\"unrelated\"]}").unwrap();
    fs::write(f.target(), "{// preferred writer\n\"theme\":\"other\"}").unwrap();
    assert_eq!(f.run(true).unwrap(), lower);
    assert_eq!(
        read(&f.target()).unwrap().unwrap(),
        "{// preferred writer\n\"theme\":\"other\"}"
    );
    let g = Fixture::new();
    fs::write(
        g.env.global.join("opencode.json"),
        "{\"plugin\":[\"keep\"]}",
    )
    .unwrap();
    assert!(
        run(&g.env, &g.state, Some(&g.target()), true)
            .unwrap_err()
            .to_string()
            .contains("Global plugin arrays")
    );
    assert!(!g.target().exists());
}
#[test]
fn malformed_duplicate_extended_or_wrong_shape_config_never_modified() {
    for text in [
        "{",
        "{\"mcp\":[],}",
        "{\"plugin\":{},}",
        "{unquoted:1}",
        "{\"x\":1 \"y\":2}",
        "{\"x\":1,\"\\u0078\":2}",
        "{\"unrelated\":{\"x\":1,\"x\":2}}",
    ] {
        let f = Fixture::new();
        fs::write(f.target(), text).unwrap();
        assert!(f.run(true).is_err(), "accepted {text}");
        assert_eq!(read(&f.target()).unwrap().unwrap(), text);
        assert!(snapshots(&f.target()).is_empty());
        assert!(!f.env.data.exists());
    }
}
#[test]
fn unowned_collisions_and_env_overrides_are_never_adopted() {
    for text in [
        "{\"mcp\":{\"agent-bridge\":{\"type\":\"local\"}}}",
        "{\"plugin\":[\"@agent-bridge/opencode-plugin@1\"]}",
    ] {
        let f = Fixture::new();
        fs::write(f.target(), text).unwrap();
        assert!(f.run(true).is_err());
        assert_eq!(read(&f.target()).unwrap().unwrap(), text);
    }
    let mut f = Fixture::new();
    f.env.inline = Some("{\"mcp\":{\"agent-bridge\":{\"enabled\":false}}}".into());
    assert!(f.run(true).is_err());
    assert!(!f.target().exists());
    f.env.inline = None;
    f.env.extra = Some(f.state.parent().unwrap().join("extra"));
    directories(f.env.extra.as_ref().unwrap(), true).unwrap();
    fs::write(
        f.env.extra.as_ref().unwrap().join("opencode.json"),
        "{\"mcp\":{\"agent-bridge\":{}}}",
    )
    .unwrap();
    assert!(f.run(true).is_err());
}
#[test]
fn custom_config_is_selected_and_unrelated_environment_config_preserved() {
    let mut f = Fixture::new();
    fs::write(
        f.env.global.join("opencode.json"),
        "{\"plugin\":[\"global-unrelated\"]}",
    )
    .unwrap();
    f.env.custom = Some(f.state.parent().unwrap().join("custom.jsonc"));
    f.env.inline = Some("{\"theme\":\"unchanged\"}".into());
    let selected = f.run(true).unwrap();
    assert_eq!(Some(selected), f.env.custom);
    assert_eq!(
        read(&f.env.global.join("opencode.json")).unwrap().unwrap(),
        "{\"plugin\":[\"global-unrelated\"]}"
    );
}
#[test]
fn owner_edits_to_managed_values_and_comments_refuse_update_and_removal() {
    for replacement in ["\"enabled\": false", "/* owner comment */\"enabled\": true"] {
        let f = Fixture::new();
        f.run(true).unwrap();
        let modified = read(&f.target())
            .unwrap()
            .unwrap()
            .replace("\"enabled\": true", replacement);
        fs::write(f.target(), &modified).unwrap();
        assert!(f.run(false).is_err());
        assert!(f.run(true).is_err());
        assert_eq!(read(&f.target()).unwrap().unwrap(), modified);
    }
    let f = Fixture::new();
    f.run(true).unwrap();
    let modified = read(&f.target()).unwrap().unwrap().replace(
        "\"enabled\": true",
        "\"enabled\": true // trailing owner comment\n",
    );
    fs::write(f.target(), &modified).unwrap();
    assert!(f.run(false).is_err());
    assert_eq!(read(&f.target()).unwrap().unwrap(), modified);
}
#[test]
fn unrelated_post_install_edits_survive_removal() {
    let f = Fixture::new();
    f.run(true).unwrap();
    let original = read(&f.target()).unwrap().unwrap();
    let modified = original.replacen('{', "{\n// owner update\n\"theme\":\"new\",", 1);
    fs::write(f.target(), &modified).unwrap();
    f.run(false).unwrap();
    let removed = read(&f.target()).unwrap().unwrap();
    assert!(removed.contains("// owner update\n\"theme\":\"new\","));
}
#[test]
fn unsafe_symlink_target_ancestor_and_sidecars_are_rejected() {
    let f = Fixture::new();
    let outside = f.state.parent().unwrap().join("outside.json");
    fs::write(&outside, "{}").unwrap();
    symlink(&outside, f.target()).unwrap();
    assert!(f.run(true).is_err());
    assert_eq!(fs::read_to_string(outside).unwrap(), "{}");
    let f = Fixture::new();
    let real = f.state.parent().unwrap().join("real");
    directories(&real, true).unwrap();
    let link = f.state.parent().unwrap().join("linked");
    symlink(&real, &link).unwrap();
    assert!(run(&f.env, &f.state, Some(&link.join("opencode.json")), true).is_err());
    let f = Fixture::new();
    symlink(
        f.state.join("sentinel.sqlite"),
        sidecar(&f.target(), "manifest.json").unwrap(),
    )
    .unwrap();
    assert!(f.run(true).is_err());
}
#[test]
fn concurrent_writer_and_held_lock_are_rejected_without_config_loss() {
    let f = Fixture::new();
    fs::write(f.target(), "{}").unwrap();
    let observed = preflight(&f.env, &f.target(), "file:///none").unwrap();
    fs::write(f.target(), "{\"owner\":true}").unwrap();
    assert!(verify(&observed).is_err());
    let lock = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(sidecar(&f.target(), "lock").unwrap())
        .unwrap();
    lock.lock_exclusive().unwrap();
    assert!(f.run(true).is_err());
    assert_eq!(read(&f.target()).unwrap().unwrap(), "{\"owner\":true}");
}
#[test]
fn interrupted_transaction_retries_only_old_or_desired_bytes() {
    let f = Fixture::new();
    fs::write(f.target(), "{}").unwrap();
    let manifest = sidecar(&f.target(), "manifest.json").unwrap();
    let journal = sidecar(&f.target(), "pending.json").unwrap();
    let pending = Pending {
        version: 1,
        target: f.target(),
        before: Some("{}".into()),
        after: "{\"installed\":true}".into(),
        manifest_before: None,
        manifest_after: Some("owned".into()),
        observed: preflight(&f.env, &f.target(), "file:///none").unwrap(),
    };
    replace(&journal, &serde_json::to_string(&pending).unwrap()).unwrap();
    // A new global source must prevent any interrupted transaction replay.
    let competing = f.env.global.join("opencode.json");
    fs::write(&competing, "{\"plugin\":[\"owner\"]}").unwrap();
    assert!(recover(&f.target(), &manifest, &journal).is_err());
    assert_eq!(read(&f.target()).unwrap().unwrap(), "{}");
    fs::remove_file(competing).unwrap();
    // Extensionless legacy TOML appearing during the interruption is also a
    // competing source, even when it is absent at the original preflight.
    let legacy = f.env.global.join("config");
    let legacy_text = "plugin = [\"owner-plugin\"]\n[mcp.owner]\ncommand = [\"owner-mcp\"]\n";
    fs::write(&legacy, legacy_text).unwrap();
    assert!(recover(&f.target(), &manifest, &journal).is_err());
    assert_eq!(read(&f.target()).unwrap().unwrap(), "{}");
    assert_eq!(read(&legacy).unwrap().unwrap(), legacy_text);
    fs::remove_file(legacy).unwrap();
    // A failed manifest write leaves the already-written config and journal recoverable.
    fs::create_dir(&manifest).unwrap();
    assert!(recover(&f.target(), &manifest, &journal).is_err());
    assert!(journal.exists());
    fs::remove_dir(&manifest).unwrap();
    recover(&f.target(), &manifest, &journal).unwrap();
    assert!(!journal.exists());
    assert_eq!(read(&manifest).unwrap().unwrap(), "owned");
    replace(&journal, &serde_json::to_string(&pending).unwrap()).unwrap();
    fs::write(f.target(), "{\"owner\":true}").unwrap();
    assert!(recover(&f.target(), &manifest, &journal).is_err());
    assert_eq!(read(&f.target()).unwrap().unwrap(), "{\"owner\":true}");
}

#[test]
fn oversized_recovery_transaction_refuses_before_creating_journal_or_backup() {
    let f = Fixture::new();
    let before = format!("{{\"owner\":\"{}\"}}", "x".repeat(2 * 1024 * 1024));
    fs::write(f.target(), &before).unwrap();
    let observed = preflight(&f.env, &f.target(), "file:///none").unwrap();
    let journal = sidecar(&f.target(), "pending.json").unwrap();
    assert!(
        transaction(
            &f.target(),
            &sidecar(&f.target(), "manifest.json").unwrap(),
            &journal,
            &observed,
            Some(before.clone()),
            format!("{before}\n"),
            None
        )
        .is_err()
    );
    assert!(!journal.exists());
    assert!(snapshots(&f.target()).is_empty());
    assert_eq!(read(&f.target()).unwrap().unwrap(), before);
}

#[test]
fn plugin_residue_from_another_managed_configuration_is_a_conflict() {
    let f = Fixture::new();
    let other_url = url(&f
        .env
        .data
        .join("different-target/different-version/index.ts"));
    fs::write(f.target(), serde_json::json!({"plugin":[[other_url,{"stateDir":f.state}]],"mcp":{"renamed":{"type":"local"}}}).to_string()).unwrap();
    assert!(f.run(true).is_err());
}

#[test]
fn legacy_global_toml_refuses_setup_without_creating_installer_artifacts() {
    let f = Fixture::new();
    let legacy = f.env.global.join("config");
    let original = "# owner legacy configuration\nplugin = [\"unrelated-plugin@1\"]\n[mcp.owner]\ntype = \"local\"\ncommand = [\"owner-command\"]\n";
    fs::write(&legacy, original).unwrap();
    let error = f.run(true).unwrap_err().to_string();
    assert!(error.contains("migrate it with OpenCode"), "{error}");
    assert!(error.contains(legacy.to_str().unwrap()));
    assert_eq!(fs::read(&legacy).unwrap(), original.as_bytes());
    assert!(!f.target().exists());
    assert!(!f.env.data.exists());
    assert_eq!(fs::read_dir(&f.env.global).unwrap().count(), 1);
    assert!(!sidecar(&f.target(), "lock").unwrap().exists());
    assert!(!sidecar(&f.target(), "manifest.json").unwrap().exists());
    assert!(!sidecar(&f.target(), "pending.json").unwrap().exists());
}

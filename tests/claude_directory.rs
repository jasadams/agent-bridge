use agent_bridge::clients::claude_directory::{
    MAX_STDERR_BYTES, MAX_STDOUT_BYTES, parse, query_blocking,
};
use serde_json::{Value, json};
use std::{
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};

const FIRST: &str = "7fca6ad7-5473-4f19-9c44-17c4bef7f1c3";
const SECOND: &str = "ec67a980-2681-4ee0-b079-36dd19f0f5dc";

fn row(session: &str) -> Value {
    json!({
        "sessionId": session,
        "name": "same-name",
        "cwd": "/disposable/project",
        "kind": "interactive",
        "startedAt": 1793607683384_u64,
        "pid": 1391318,
        "status": "idle"
    })
}

fn bytes(rows: Vec<Value>) -> Vec<u8> {
    serde_json::to_vec(&rows).unwrap()
}

#[test]
fn interactive_uuid_is_native_identity_not_name_or_pid() {
    let result = parse(&bytes(vec![row(FIRST), row(SECOND)])).unwrap();
    assert_eq!(result.sessions.len(), 2);
    assert_eq!(result.sessions[0].session, FIRST);
    assert_eq!(result.sessions[1].session, SECOND);
    assert_eq!(result.sessions[0].name.as_deref(), Some("same-name"));
    assert_eq!(result.sessions[0].status.as_deref(), Some("idle"));
    assert_eq!(result.sessions[0].started_at, 1793607683384);
    assert_eq!(result.unaddressable_rows, 0);
}

#[test]
fn missing_uuid_is_explicitly_unaddressable_background_stays_unknown() {
    let background = json!({
        "sessionId": FIRST,
        "cwd": "/disposable/project",
        "kind": "background",
        "startedAt": 1793607683384_u64,
        "id": "short-reference",
        "state": "blocked",
        "waitingFor": "permission",
        "futureField": {"nested": true}
    });
    let mut no_uuid = row(SECOND);
    no_uuid.as_object_mut().unwrap().remove("sessionId");
    let result = parse(&bytes(vec![background, no_uuid])).unwrap();
    assert_eq!(result.sessions.len(), 1);
    assert_eq!(result.unaddressable_rows, 1);
    let row = &result.sessions[0];
    assert_eq!(row.status, None);
    assert_eq!(row.pid, None);
    assert_eq!(row.name, None);
    assert_eq!(row.kind, "background");
    assert_eq!(row.state.as_deref(), Some("blocked"));
    assert_eq!(row.waiting_for.as_deref(), Some("permission"));
}

#[test]
fn malformed_or_conflicting_rows_never_become_success_empty() {
    let mut conflicting = row(FIRST);
    conflicting["cwd"] = json!("/other");
    let invalid = [
        b"not json".to_vec(),
        b"[".to_vec(),
        b"{}".to_vec(),
        bytes(vec![row("not-a-uuid")]),
        bytes(vec![row("7fca6ad754734f199c4417c4bef7f1c3")]),
        bytes(vec![row(FIRST), conflicting]),
        bytes(vec![json!({"sessionId": FIRST})]),
    ];
    for input in invalid {
        assert!(parse(&input).is_err(), "accepted invalid native directory");
    }
}

#[test]
fn uuid_case_normalizes_and_identical_duplicates_collapse() {
    let result = parse(&bytes(vec![row(FIRST), row(&FIRST.to_uppercase())])).unwrap();
    assert_eq!(result.sessions.len(), 1);
    assert_eq!(result.sessions[0].session, FIRST);
}

#[test]
fn successful_empty_is_distinct_from_invalid_output() {
    assert!(parse(b"[]").unwrap().sessions.is_empty());
    assert!(parse(b"").is_err());
}

#[test]
fn status_observations_preserve_busy_waiting_and_future_values() {
    for status in ["busy", "waiting", "future-status"] {
        let mut native = row(FIRST);
        native["status"] = json!(status);
        let snapshot = parse(&bytes(vec![native])).unwrap();
        assert_eq!(snapshot.sessions[0].status.as_deref(), Some(status));
    }
}

#[test]
fn malformed_optional_fields_invalidate_whole_snapshot_without_private_diagnostics() {
    for (key, value) in [
        ("sessionId", json!(42)),
        ("status", json!({"secret": "native-private-content"})),
        ("pid", json!(-1)),
        ("startedAt", json!(-1)),
        ("cwd", json!(null)),
        ("name", json!(false)),
    ] {
        let mut invalid = row(SECOND);
        invalid[key] = value;
        let error = parse(&bytes(vec![row(FIRST), invalid])).unwrap_err();
        assert!(!format!("{error:#}").contains("native-private-content"));
    }
    assert!(parse(&vec![b' '; MAX_STDOUT_BYTES + 1]).is_err());
}

struct FakeCli {
    _root: tempfile::TempDir,
    binary: PathBuf,
    pid_file: PathBuf,
}

fn fixture_dir() -> tempfile::TempDir {
    let root = Path::new("/tmp/opencode");
    if root.is_dir() {
        tempfile::tempdir_in(root).unwrap()
    } else {
        tempfile::tempdir().unwrap()
    }
}

fn executable_template() -> &'static Path {
    static TEMPLATE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    &TEMPLATE.get_or_init(|| {
        let root = fixture_dir();
        let binary = root.path().join("immutable-claude");
        // Publish once, close the writable executable FD, then release all
        // fixture callers through OnceLock. No query can fork while another
        // fixture holds this executable open for writing (ETXTBSY). Evidence:
        // /tmp/opencode/agb43-adapter-flake-repro.txt (12/51 pre-fix runs).
        std::fs::write(&binary,
            "#!/bin/sh\n[ \"$#\" = 2 ] && [ \"$1\" = agents ] && [ \"$2\" = --json ] || exit 92\necho $$ > \"$0.pid\"\n. \"$0.body\"\n"
        ).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        (root, binary)
    }).1
}

impl FakeCli {
    fn new(body: &str) -> Self {
        let template = executable_template();
        let root = fixture_dir();
        let binary = root.path().join("claude");
        let pid_file = root.path().join("claude.pid");
        // Only the fake fixture uses a shell. Production always executes the
        // provided CLI directly, never a shell, and never mutates global env.
        // Per-fixture bodies are sourced as data, never kernel executables.
        std::fs::write(root.path().join("claude.body"), format!("{body}\n")).unwrap();
        symlink(template, &binary).unwrap();
        Self {
            _root: root,
            binary,
            pid_file,
        }
    }

    async fn pid(&self) -> libc::pid_t {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(value) = std::fs::read_to_string(&self.pid_file)
                    && let Ok(pid) = value.trim().parse()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake CLI did not start")
    }
}

async fn assert_exited(pid: libc::pid_t) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            // SAFETY: signal 0 only probes the disposable fixture's process.
            let result = unsafe { libc::kill(pid, 0) };
            if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("directory CLI leaked after cancellation/failure");
}

#[tokio::test]
async fn fake_cli_success_is_complete_and_uses_only_passive_arguments() {
    let cli = FakeCli::new(&format!("printf '%s' '{}'", json!([row(FIRST)])));
    let result = query_blocking(&cli.binary, Duration::from_secs(3)).unwrap();
    assert_eq!(result.sessions[0].session, FIRST);
    let empty = FakeCli::new("printf '[]'");
    assert!(
        query_blocking(&empty.binary, Duration::from_secs(3))
            .unwrap()
            .sessions
            .is_empty()
    );
}

#[tokio::test]
async fn fake_cli_missing_unsupported_nonzero_and_truncated_are_unavailable() {
    let root = fixture_dir();
    assert!(query_blocking(&root.path().join("absent"), Duration::from_secs(3)).is_err());
    for body in [
        "echo 'native-private-content unsupported agents' >&2; exit 2",
        "printf '[]'; exit 1",
        "printf '[{'; exit 0",
        "printf 'native-private-content'; exit 0",
    ] {
        let cli = FakeCli::new(body);
        let error = query_blocking(&cli.binary, Duration::from_secs(3)).unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("unavailable"));
        assert!(!diagnostic.contains("native-private-content"));
    }
}

#[tokio::test]
async fn fake_cli_bounds_both_pipes_and_kills_on_overflow() {
    for (limit, redirect) in [(MAX_STDOUT_BYTES, ""), (MAX_STDERR_BYTES, ">&2")] {
        let cli = FakeCli::new(&format!(
            "head -c {} /dev/zero {redirect}\nexec sleep 30",
            limit + 1
        ));
        let error = query_blocking(&cli.binary, Duration::from_secs(3)).unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(
            diagnostic.contains("output exceeds limit"),
            "unexpected diagnostic: {diagnostic}"
        );
        assert_exited(cli.pid().await).await;
    }
}

#[tokio::test]
async fn fake_cli_timeout_kills_child_and_does_not_return_empty() {
    let cli = FakeCli::new("exec sleep 30");
    let error = query_blocking(&cli.binary, Duration::from_millis(150)).unwrap_err();
    assert!(format!("{error:#}").contains("timed out"));
    assert_exited(cli.pid().await).await;
}

#[test]
fn blocking_cli_matches_parser_and_distinguishes_empty_from_unavailable() {
    let cli = FakeCli::new(&format!("printf '%s' '{}'", json!([row(FIRST)])));
    let snapshot = query_blocking(&cli.binary, Duration::from_secs(3)).unwrap();
    assert_eq!(snapshot, parse(&bytes(vec![row(FIRST)])).unwrap());
    let empty = FakeCli::new("printf '[]'");
    assert_eq!(
        query_blocking(&empty.binary, Duration::from_secs(3)).unwrap(),
        parse(b"[]").unwrap()
    );
    assert!(query_blocking(&empty._root.path().join("absent"), Duration::from_secs(3)).is_err());
    for body in [
        "echo native-private-content >&2; exit 2",
        "printf '[]'; exit 1",
        "printf '[{'",
        "printf native-private-content",
    ] {
        let cli = FakeCli::new(body);
        let error = query_blocking(&cli.binary, Duration::from_secs(3)).unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("unavailable"));
        assert!(!diagnostic.contains("native-private-content"));
    }
}

#[tokio::test]
async fn blocking_cli_bounds_pipes_and_reaps_on_timeout_or_overflow() {
    for body in [
        "exec sleep 30".to_owned(),
        format!("head -c {} /dev/zero\nexec sleep 30", MAX_STDOUT_BYTES + 1),
        format!(
            "head -c {} /dev/zero >&2\nexec sleep 30",
            MAX_STDERR_BYTES + 1
        ),
    ] {
        let cli = FakeCli::new(&body);
        let timeout = if body == "exec sleep 30" {
            Duration::from_millis(150)
        } else {
            Duration::from_secs(3)
        };
        let error = query_blocking(&cli.binary, timeout).unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(
            diagnostic.contains(if body == "exec sleep 30" {
                "timed out"
            } else {
                "output exceeds limit"
            }),
            "unexpected diagnostic: {diagnostic}"
        );
        assert_exited(cli.pid().await).await;
    }
}

#[tokio::test]
async fn blocking_cli_finishes_deadline_after_waiter_is_dropped() {
    let cli = FakeCli::new("exec sleep 30");
    let binary = cli.binary.clone();
    let task =
        tokio::task::spawn_blocking(move || query_blocking(&binary, Duration::from_millis(250)));
    let pid = cli.pid().await;
    drop(task);
    assert_exited(pid).await;
}

//! Wake a suspended Orca chat so it starts and binds a new inbox socket.
//!
//! The note is fixed text: it carries no message content and nothing a sender supplied, so a
//! wake can never be used to put words in front of the recipient. The message itself still
//! arrives through the bridge with its own provenance banner.
//!
//! The command is the installed Orca CLI at one fixed path. Nothing a session reports is ever
//! executed, and the child gets a minimal environment, so no local process can use the broker
//! to run a program of its choosing or to read the broker's environment.
use anyhow::{Result, bail, ensure};
use std::{
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Where Orca installs its CLI on macOS. The only supported location.
pub const ORCA_CLI: &str = "/Applications/Orca.app/Contents/Resources/bin/orca";

const SUBJECT: &str = "Agent Bridge: a message is waiting";
const BODY: &str = "Your Agent Bridge inbox has a message waiting. It is delivered automatically once this session is running; no reply or action is needed.";
/// How long the wake may take before it is ended.
pub const DEADLINE: Duration = Duration::from_secs(10);

pub fn default_cli() -> PathBuf {
    PathBuf::from(ORCA_CLI)
}

/// An Orca session ID as the start hook reports it: a canonical UUID.
pub fn valid_session(orca_session: &str) -> bool {
    uuid::Uuid::parse_str(orca_session).is_ok_and(|id| id.to_string() == orca_session)
}

/// The CLI is an executable regular file.
pub fn available(cli: &Path) -> bool {
    std::fs::metadata(cli)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Send the note from `from` (a bound Claude session's Orca chat) to `to` with the CLI at
/// `cli`. Bounded; never run while holding the database lock.
pub fn wake(cli: &Path, from: &str, to: &str, deadline: Duration) -> Result<()> {
    let mut command = Command::new(cli);
    command
        .args(["orchestration", "send", "--from"])
        .arg(format!("orca_session_id:{from}"))
        .arg("--to")
        .arg(format!("orca_session_id:{to}"))
        .args(["--subject", SUBJECT, "--body", BODY, "--json"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Its own process group, so a timeout can end the CLI and anything it started.
        .process_group(0);
    if let Some(home) = std::env::var_os("HOME") {
        command.env("HOME", home);
    }
    let mut child = command
        .spawn()
        .map_err(|error| anyhow::anyhow!("cannot run orca: {error}"))?;
    let limit = Instant::now() + deadline;
    loop {
        if let Some(status) = child.try_wait()? {
            ensure!(status.success(), "orca exited with {status}");
            return Ok(());
        }
        if Instant::now() >= limit {
            // SAFETY: a negative pid addresses the child's own process group, created above.
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            let _ = child.wait();
            bail!("orca did not answer within {} s", deadline.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("orca");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn the_child_gets_only_home_and_a_fixed_path_and_runs_in_the_root_directory() {
        let dir = tempfile::tempdir().unwrap();
        let seen = dir.path().join("seen");
        let cli = script(
            dir.path(),
            &format!("env | sort > {seen}; pwd >> {seen}", seen = seen.display()),
        );
        wake(&cli, "a", "b", DEADLINE).unwrap();
        let seen = std::fs::read_to_string(seen).unwrap();
        for line in seen.lines().filter(|line| line.contains('=')) {
            let name = line.split('=').next().unwrap();
            assert!(
                ["HOME", "PATH", "PWD", "SHLVL", "_", "OLDPWD"].contains(&name),
                "leaked into the child: {name}"
            );
        }
        assert!(
            seen.contains("PATH=/usr/bin:/bin:/usr/sbin:/sbin"),
            "{seen}"
        );
        assert_eq!(seen.lines().last(), Some("/"));
    }

    #[test]
    fn a_timeout_ends_the_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let pid = dir.path().join("pid");
        let cli = script(
            dir.path(),
            &format!("sleep 30 &\necho $! > {pid}\nwait", pid = pid.display()),
        );
        let error = wake(&cli, "a", "b", Duration::from_millis(500)).unwrap_err();
        assert!(format!("{error:#}").contains("did not answer"), "{error:#}");
        let child: i32 = std::fs::read_to_string(&pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        // SAFETY: signal 0 only checks that the process exists.
        assert_eq!(
            unsafe { libc::kill(child, 0) },
            -1,
            "the script's child survived"
        );
    }

    #[test]
    fn only_an_executable_regular_file_is_available() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!available(&dir.path().join("missing")));
        assert!(!available(dir.path()));
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "x").unwrap();
        assert!(!available(&plain));
        assert!(available(&script(dir.path(), "exit 0")));
    }
}

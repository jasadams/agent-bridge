//! Wake a suspended Orca chat so it starts and binds a new inbox socket.
//!
//! The note is fixed text: it carries no message content and nothing a sender supplied, so a
//! wake can never be used to put words in front of the recipient. The message itself still
//! arrives through the normal bridge path with its own provenance banner.
use anyhow::{Result, bail, ensure};
use std::{
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const SUBJECT: &str = "Agent Bridge: a message is waiting";
const BODY: &str = "Your Agent Bridge inbox has a message waiting. It is delivered automatically once this session is running; no reply or action is needed.";
const DEADLINE: Duration = Duration::from_secs(10);

/// What the start hook may record: an Orca session UUID and the absolute path of the `orca`
/// command that Orca exported to it. Nothing else is ever executed.
pub fn validate(orca_session: &str, orca_cli: &str) -> Result<()> {
    ensure!(
        uuid::Uuid::parse_str(orca_session).is_ok_and(|id| id.to_string() == orca_session),
        "Orca session must be a UUID"
    );
    let path = Path::new(orca_cli);
    ensure!(
        path.is_absolute()
            && orca_cli.len() <= 4096
            && path.file_name().is_some_and(|name| name == "orca"),
        "Orca command must be the absolute path of `orca`"
    );
    Ok(())
}

/// Send the note from `from` (a bound Claude session's Orca chat) to `to`. Bounded; never run
/// while holding the database lock.
pub fn wake(cli: &str, from: &str, to: &str) -> Result<()> {
    let mut child = Command::new(cli)
        .args(["orchestration", "send", "--from"])
        .arg(format!("orca_session_id:{from}"))
        .arg("--to")
        .arg(format!("orca_session_id:{to}"))
        .args(["--subject", SUBJECT, "--body", BODY, "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| anyhow::anyhow!("cannot run orca: {error}"))?;
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(status) = child.try_wait()? {
            ensure!(status.success(), "orca exited with {status}");
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("orca did not answer within {} s", DEADLINE.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

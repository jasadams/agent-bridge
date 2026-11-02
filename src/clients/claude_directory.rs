//! Request-scoped native Claude directory; no discovery state is persisted.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::HashMap,
    io::{ErrorKind, Read},
    os::fd::AsRawFd,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};

pub const MAX_STDOUT_BYTES: usize = 1024 * 1024;
pub const MAX_STDERR_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectorySession {
    pub session: String,
    pub name: Option<String>,
    pub cwd: String,
    pub kind: String,
    pub status: Option<String>,
    pub pid: Option<u32>,
    pub state: Option<String>,
    pub waiting_for: Option<String>,
    /// Unix milliseconds at start, never last activity.
    pub started_at: u64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct DirectorySnapshot {
    pub sessions: Vec<DirectorySession>,
    /// Native entries lacking a UUID cannot be addressable recipients.
    pub unaddressable_rows: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeRow {
    session_id: Option<String>,
    name: Option<String>,
    cwd: String,
    kind: String,
    started_at: u64,
    status: Option<String>,
    pid: Option<u32>,
    state: Option<String>,
    waiting_for: Option<String>,
}

/// Validate a complete native response. Missing UUIDs are counted explicitly;
/// malformed UUIDs or conflicting duplicate observations invalidate the snapshot.
pub fn parse(bytes: &[u8]) -> Result<DirectorySnapshot> {
    ensure!(
        bytes.len() <= MAX_STDOUT_BYTES,
        "Claude directory response exceeds output limit"
    );
    // Do not attach serde's error: native string values can appear in diagnostics.
    let rows: Vec<NativeRow> = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("Claude directory returned invalid JSON records"))?;
    let mut snapshot = DirectorySnapshot::default();
    let mut identities = HashMap::new();
    for row in rows {
        ensure!(
            !row.cwd.is_empty() && !row.kind.is_empty(),
            "Claude directory returned invalid required metadata"
        );
        let Some(session) = row.session_id else {
            snapshot.unaddressable_rows += 1;
            continue;
        };
        super::claude::validate_session(&session)
            .map_err(|_| anyhow::anyhow!("Claude directory returned an invalid session UUID"))?;
        let uuid = uuid::Uuid::parse_str(&session)
            .map_err(|_| anyhow::anyhow!("Claude directory returned an invalid session UUID"))?;
        let normalized = uuid.hyphenated().to_string();
        ensure!(
            session.eq_ignore_ascii_case(&normalized),
            "Claude directory requires a full hyphenated session UUID"
        );
        let session = DirectorySession {
            session: normalized,
            name: row.name,
            cwd: row.cwd,
            kind: row.kind,
            started_at: row.started_at,
            status: row.status,
            pid: row.pid,
            state: row.state,
            waiting_for: row.waiting_for,
        };
        if let Some(&index) = identities.get(&session.session) {
            ensure!(
                snapshot.sessions[index] == session,
                "Claude directory returned conflicting duplicate UUID records"
            );
        } else {
            identities.insert(session.session.clone(), snapshot.sessions.len());
            snapshot.sessions.push(session);
        }
    }
    Ok(snapshot)
}

struct BlockingChild(std::process::Child);

impl Drop for BlockingChild {
    fn drop(&mut self) {
        // A synchronous caller (including spawn_blocking) retains ownership
        // until the bounded operation finishes, even if its requester goes away.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn nonblocking(pipe: &impl AsRawFd) -> Result<()> {
    let fd = pipe.as_raw_fd();
    // SAFETY: the owned pipe remains alive throughout both descriptor calls.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error())
            .context("Cannot configure Claude directory output pipe");
    }
    Ok(())
}

// One read per poll keeps the other pipe and the deadline from being starved by
// a continuously writing child. Never wait for EOF on a blocking descriptor.
fn read_available(pipe: &mut impl Read, bytes: &mut Vec<u8>, limit: usize) -> Result<bool> {
    let mut buffer = [0; 8192];
    match pipe.read(&mut buffer) {
        Ok(0) => Ok(true),
        Ok(count) => {
            ensure!(
                count <= limit.saturating_sub(bytes.len()),
                "Claude directory output exceeds limit"
            );
            bytes.extend_from_slice(&buffer[..count]);
            Ok(false)
        }
        Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {
            Ok(false)
        }
        Err(error) => Err(error).context("Cannot read Claude directory process output"),
    }
}

/// Collect a passive native snapshot before acquiring broker
/// state. No Tokio runtime is constructed or entered. The caller retains this
/// bounded operation's ownership; dropping a spawn_blocking waiter does not
/// extend the CLI's deadline. Both output pipes are byte-bounded.
pub fn query_blocking(binary: &Path, deadline: Duration) -> Result<DirectorySnapshot> {
    let started = Instant::now();
    let mut child = BlockingChild(
        std::process::Command::new(binary)
            .args(["agents", "--json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Claude native directory unavailable: cannot start CLI")?,
    );
    let result = (|| {
        let mut stdout = child
            .0
            .stdout
            .take()
            .context("Claude directory stdout unavailable")?;
        let mut stderr = child
            .0
            .stderr
            .take()
            .context("Claude directory stderr unavailable")?;
        nonblocking(&stdout)?;
        nonblocking(&stderr)?;
        let mut output = Vec::new();
        let mut diagnostics = Vec::new();
        let mut stdout_done = false;
        let mut stderr_done = false;
        let mut status = None;
        loop {
            ensure!(
                started.elapsed() < deadline,
                "Claude directory query timed out"
            );
            if !stdout_done {
                stdout_done = read_available(&mut stdout, &mut output, MAX_STDOUT_BYTES)?;
            }
            if !stderr_done {
                stderr_done = read_available(&mut stderr, &mut diagnostics, MAX_STDERR_BYTES)?;
            }
            if status.is_none() {
                status = child
                    .0
                    .try_wait()
                    .context("Cannot wait for Claude directory CLI")?;
            }
            if let Some(status) = status {
                ensure!(
                    status.success(),
                    "CLI exited unsuccessfully (requires agents --json support)"
                );
                if stdout_done && stderr_done {
                    return parse(&output);
                }
            }
            std::thread::sleep(
                Duration::from_millis(2).min(deadline.saturating_sub(started.elapsed())),
            );
        }
    })();
    result.context("Claude native directory unavailable")
}

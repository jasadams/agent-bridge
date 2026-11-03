//! Passive Claude directory reads. No transcript scanner, registration or model turn.
use super::{ConnectorState, NativeState, StateError};
use crate::protocol::Provider;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    io::{self, Read},
    os::fd::AsRawFd,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const MAX_OUTPUT: usize = 1024 * 1024;
const MAX_ROWS: usize = 4096;
const DEADLINE: Duration = Duration::from_secs(5);

pub struct State {
    executable: PathBuf,
}
impl State {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    fn read(&self) -> Result<Vec<u8>, StateError> {
        let mut child = Command::new(&self.executable)
            .args(["agents", "--json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // Discard stderr without a pipe: bursts cannot deadlock the child,
            // allocate unbounded diagnostics, or leak native private content.
            .stderr(Stdio::null())
            .spawn()
            .map_err(unavailable)?;
        let result = (|| {
            let mut stdout = child.stdout.take().expect("piped stdout");
            let fd = stdout.as_raw_fd();
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(unavailable(io::Error::last_os_error()));
            }
            let deadline = Instant::now() + DEADLINE;
            let mut output = Vec::new();
            let mut eof = false;
            loop {
                let mut buf = [0; 8192];
                loop {
                    match stdout.read(&mut buf) {
                        Ok(0) => {
                            eof = true;
                            break;
                        }
                        Ok(n) if output.len() + n <= MAX_OUTPUT => {
                            output.extend_from_slice(&buf[..n])
                        }
                        Ok(_) => return Err(incomplete("Claude directory exceeds 1 MiB")),
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => return Err(unavailable(e)),
                    }
                }
                if let Some(status) = child.try_wait().map_err(unavailable)? {
                    if !status.success() {
                        return Err(unavailable(format!(
                            "claude agents --json exited with {status}"
                        )));
                    }
                    if eof {
                        return Ok(output);
                    }
                }
                if Instant::now() >= deadline {
                    return Err(unavailable("claude agents --json timed out"));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        })();
        if result.is_err() {
            let _ = child.kill();
        }
        let _ = child.wait();
        result
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Row {
    session_id: Option<String>,
    name: Option<String>,
    cwd: Option<String>,
    status: Option<String>,
    state: Option<String>,
    #[serde(rename = "kind")]
    _kind: Option<String>,
    #[serde(rename = "startedAt")]
    _started_at: Option<u64>,
    #[serde(rename = "pid")]
    _pid: Option<u32>,
}

fn unavailable(error: impl std::fmt::Display) -> StateError {
    StateError::Unavailable(error.to_string())
}
fn incomplete(error: impl std::fmt::Display) -> StateError {
    StateError::Incomplete(error.to_string())
}
fn valid_id(session: &str) -> bool {
    session.len() == 36 && uuid::Uuid::parse_str(session).is_ok()
}

impl ConnectorState for State {
    fn provider(&self) -> Provider {
        Provider::Claude
    }

    fn list(&self, _max_age_days: u32) -> Result<Vec<NativeState>, StateError> {
        let rows: Vec<Row> = serde_json::from_slice(&self.read()?)
            .map_err(|_| incomplete("Invalid Claude directory JSON"))?;
        if rows.len() > MAX_ROWS {
            return Err(incomplete("Claude directory exceeds 4096 rows"));
        }
        let mut sessions = BTreeMap::new();
        for row in rows {
            let session = row
                .session_id
                .filter(|s| valid_id(s))
                .ok_or_else(|| incomplete("Claude directory row lacks a full session UUID"))?;
            let state = NativeState {
                session: session.clone(),
                display_name: row.name,
                status: row.status.or(row.state),
                directory: row.cwd,
                updated_ms: None,
                transport_endpoint: None,
            };
            if let Some(old) = sessions.insert(session, state.clone())
                && old != state
            {
                return Err(incomplete("Conflicting Claude session UUID records"));
            }
        }
        Ok(sessions.into_values().collect())
    }

    fn get(&self, session: &str) -> Result<Option<NativeState>, StateError> {
        if !valid_id(session) {
            return Err(incomplete("Claude lookup requires a full session UUID"));
        }
        Ok(self.list(0)?.into_iter().find(|row| row.session == session))
    }
}

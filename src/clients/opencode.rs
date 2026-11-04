//! OpenCode plugin transport. A receipt confirms native insertion, never model acknowledgement.
use super::{ConnectorState, NativeState, StateError};
use crate::{
    delivery::Outcome,
    protocol::{Agent, MAX_FRAME, Message, Provider},
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    io::{Read, Write},
    os::{fd::FromRawFd, unix::ffi::OsStrExt},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::timeout,
};

pub fn validate_session(session: &str) -> Result<()> {
    ensure!(
        session.starts_with("ses_")
            && session.len() == 30
            && session[4..].bytes().all(|b| b.is_ascii_alphanumeric()),
        "OpenCode requires a native ses_ session ID"
    );
    Ok(())
}

/// Plain text presentation, with immutable tool identities separate from labels.
/// Stable instructions belong to the plugin's system context, not each message.
pub(crate) fn message_content(msg: &Message, machines: &Value) -> String {
    use unicode_general_category::{GeneralCategory as Category, get_general_category};
    let label = machines["sender_name"].as_str().unwrap_or(&msg.sender);
    let label: String = label
        .chars()
        .filter(|c| {
            !matches!(
                get_general_category(*c),
                Category::Control
                    | Category::Format
                    | Category::LineSeparator
                    | Category::ParagraphSeparator
            )
        })
        .take(160)
        .collect();
    let label = if label.trim().is_empty() {
        msg.sender.as_str()
    } else {
        label.trim()
    };
    let parent = msg
        .parent
        .as_ref()
        .map(|id| format!(" parent={id}"))
        .unwrap_or_default();
    format!(
        "{label}:\n{}\n\n[peer id={} from={} to={}{parent}]",
        msg.body, msg.id, msg.sender, msg.recipient
    )
}

pub fn validate_endpoint(endpoint: &str) -> Result<PathBuf> {
    let path = super::claude::validate_socket(Path::new(endpoint))?;
    endpoint_instance(&path)?;
    Ok(path)
}

pub(crate) fn endpoint_instance(path: &Path) -> Result<String> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .context("Missing OpenCode instance")?;
    ensure!(
        path.extension().is_some_and(|s| s == "sock"),
        "Invalid OpenCode endpoint extension"
    );
    let id = uuid::Uuid::parse_str(stem)
        .context("OpenCode endpoint must identify its plugin instance")?;
    ensure!(
        id.to_string() == stem,
        "OpenCode endpoint instance must be canonical"
    );
    Ok(stem.to_owned())
}

/// Request-local transport locators, not native session membership. The broker
/// snapshots these (including expired locators) before releasing its DB lock.
pub struct State {
    endpoints: Vec<String>,
    preferred: Option<String>,
}
impl State {
    pub fn new(mut endpoints: Vec<String>, preferred: Option<String>) -> Self {
        endpoints.sort();
        endpoints.dedup();
        Self {
            endpoints,
            preferred,
        }
    }

    fn first(&self, frame: &Value, deadline: Instant) -> Result<(PathBuf, StateReply), StateError> {
        if self.endpoints.len() > 64 {
            return Err(incomplete("Too many OpenCode transport locators"));
        }
        let mut endpoints: Vec<_> = self.endpoints.iter().collect();
        if frame["op"] == "state_get" {
            endpoints.sort_by_key(|endpoint| {
                (
                    Some(endpoint.as_str()) != self.preferred.as_deref(),
                    *endpoint,
                )
            });
        }
        for endpoint in endpoints {
            let Ok(path) = validate_endpoint(endpoint) else {
                continue;
            };
            match exchange_sync(&path, frame, deadline) {
                Ok(bytes) => return Ok((path.clone(), decode_state(&bytes, &path, frame)?)),
                Err(error) if error.downcast_ref::<StateError>().is_some() => {
                    return Err(incomplete("Invalid OpenCode state frame"));
                }
                Err(_) => {} // A stale/missing locator is not native absence.
            }
        }
        Err(StateError::Unavailable(
            "No usable OpenCode transport locator".into(),
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StateRow {
    session: String,
    #[serde(deserialize_with = "Option::deserialize")]
    display_name: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    status: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    directory: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    updated_ms: Option<i64>,
}
impl StateRow {
    fn native(self, path: &Path) -> Result<NativeState, StateError> {
        validate_session(&self.session)
            .map_err(|_| incomplete("Invalid native OpenCode identity"))?;
        Ok(NativeState {
            session: self.session,
            display_name: self.display_name,
            status: self.status,
            directory: self.directory,
            updated_ms: self.updated_ms,
            transport_endpoint: Some(path.to_string_lossy().into_owned()),
        })
    }
}
#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum StateReply {
    List {
        sessions: Vec<StateRow>,
        #[serde(deserialize_with = "Option::deserialize")]
        next_cursor: Option<String>,
    },
    Get {
        session: String,
        #[serde(deserialize_with = "Option::deserialize")]
        state: Option<StateRow>,
    },
    Error {
        error: NativeFailure,
    },
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum NativeFailure {
    Unavailable,
    Incomplete,
}
fn incomplete(detail: &str) -> StateError {
    StateError::Incomplete(detail.into())
}
fn decode_state(bytes: &[u8], path: &Path, frame: &Value) -> Result<StateReply, StateError> {
    let decode = || -> Result<StateReply> {
        let mut object: serde_json::Map<String, Value> = serde_json::from_slice(bytes)?;
        ensure!(
            object.remove("protocol") == Some(json!(2))
                && object.remove("instance") == Some(json!(endpoint_instance(path)?))
                && object.remove("op") == Some(frame["op"].clone()),
            "State identity mismatch"
        );
        let ok = object.remove("ok");
        let reply: StateReply = serde_json::from_value(Value::Object(object))?;
        ensure!(
            ok == Some(json!(!matches!(reply, StateReply::Error { .. }))),
            "State result mismatch"
        );
        Ok(reply)
    };
    match decode().map_err(|_| incomplete("Invalid OpenCode state reply"))? {
        StateReply::Error {
            error: NativeFailure::Unavailable,
        } => Err(StateError::Unavailable(
            "OpenCode native state unavailable".into(),
        )),
        StateReply::Error {
            error: NativeFailure::Incomplete,
        } => Err(incomplete("OpenCode native state incomplete")),
        reply => Ok(reply),
    }
}
impl ConnectorState for State {
    fn provider(&self) -> Provider {
        Provider::OpenCode
    }
    fn list(&self, max_age_days: u32) -> Result<Vec<NativeState>, StateError> {
        if max_age_days == 0 {
            return Err(incomplete("max_age_days must be at least 1"));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let (path, mut reply) = self.first(&json!({"op":"state_list","cursor":null}), deadline)?;
        let mut rows = BTreeMap::new();
        let mut cursors = HashSet::new();
        for page in 0..64 {
            let StateReply::List {
                sessions,
                next_cursor,
            } = reply
            else {
                return Err(incomplete("Expected an OpenCode state list"));
            };
            for row in sessions {
                let row = row.native(&path)?;
                if let Some(old) = rows.insert(row.session.clone(), row.clone())
                    && old != row
                {
                    return Err(incomplete("Conflicting OpenCode session records"));
                }
                if rows.len() > 4096 {
                    return Err(incomplete("OpenCode state exceeds 4096 sessions"));
                }
            }
            let Some(cursor) = next_cursor else {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| StateError::Unavailable("System clock unavailable".into()))?
                    .as_millis()
                    .min(i64::MAX as u128) as i64;
                return Ok(rows
                    .into_values()
                    .filter(|row| within_age(row.updated_ms, max_age_days, now))
                    .collect());
            };
            if page == 63
                || cursor.is_empty()
                || cursor.len() > 4096
                || !cursors.insert(cursor.clone())
            {
                return Err(incomplete("OpenCode state pagination incomplete"));
            }
            let frame = json!({"op":"state_list","cursor":cursor});
            let bytes = exchange_sync(&path, &frame, deadline)
                .map_err(|_| incomplete("OpenCode state page unavailable"))?;
            reply = decode_state(&bytes, &path, &frame)
                .map_err(|_| incomplete("OpenCode state page incomplete"))?;
        }
        unreachable!("bounded page loop returns at the final page")
    }
    fn get(&self, session: &str) -> Result<Option<NativeState>, StateError> {
        validate_session(session)
            .map_err(|_| incomplete("OpenCode lookup requires an exact session ID"))?;
        let (path, reply) = self.first(
            &json!({"op":"state_get","session":session}),
            Instant::now() + Duration::from_secs(5),
        )?;
        let StateReply::Get {
            session: echoed,
            state,
        } = reply
        else {
            return Err(incomplete("Expected an OpenCode state lookup"));
        };
        if echoed != session || state.as_ref().is_some_and(|row| row.session != session) {
            return Err(incomplete("OpenCode state session mismatch"));
        }
        state.map(|row| row.native(&path)).transpose()
    }
}

fn within_age(updated: Option<i64>, days: u32, now: i64) -> bool {
    updated.is_none_or(|updated| {
        updated >= now.saturating_sub(i64::from(days).saturating_mul(86_400_000))
    })
}

/// Nonblocking std transport shares one deadline across connect/write/read and
/// every page. No temporary Tokio runtime and no DB lock during native I/O.
fn exchange_sync(path: &Path, frame: &Value, deadline: Instant) -> Result<Vec<u8>> {
    use std::os::unix::net::UnixStream as Stream;
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    ensure!(fd >= 0, "Cannot create native socket");
    let mut stream = unsafe { Stream::from_raw_fd(fd) };
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
        "Cannot protect native socket descriptor"
    );
    stream.set_nonblocking(true)?;
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as _;
    let name = path.as_os_str().as_bytes();
    ensure!(
        name.len() < address.sun_path.len(),
        "Native socket path too long"
    );
    for (target, byte) in address.sun_path.iter_mut().zip(name) {
        *target = *byte as _;
    }
    let length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + name.len() + 1) as libc::socklen_t;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = length as _;
    }
    let connected =
        unsafe { libc::connect(fd, (&address as *const libc::sockaddr_un).cast(), length) };
    ensure!(
        connected == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EINPROGRESS),
        "Native socket unavailable"
    );
    let wait = |events| -> Result<()> {
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .context("Native state deadline elapsed")?;
            let mut descriptor = libc::pollfd {
                fd,
                events,
                revents: 0,
            };
            let ready = unsafe {
                libc::poll(
                    &mut descriptor,
                    1,
                    remaining.as_millis().clamp(1, i32::MAX as u128) as i32,
                )
            };
            if ready < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            ensure!(ready > 0, "Native state transport unavailable or timed out");
            return Ok(());
        }
    };
    wait(libc::POLLOUT)?;
    if let Some(error) = stream.take_error()? {
        return Err(error.into());
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            )
        };
        ensure!(
            result == 0
                && length as usize == std::mem::size_of::<libc::ucred>()
                && credentials.uid == unsafe { libc::geteuid() },
            "Native socket peer mismatch"
        );
    }
    #[cfg(target_os = "macos")]
    {
        let (mut uid, mut gid) = (0, 0);
        ensure!(
            unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } == 0
                && uid == unsafe { libc::geteuid() },
            "Native socket peer mismatch"
        );
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    anyhow::bail!("Native socket peer authentication unsupported");
    let mut bytes = serde_json::to_vec(frame)?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() <= MAX_FRAME,
        "OpenCode request exceeds frame limit"
    );
    let mut unsent = bytes.as_slice();
    while !unsent.is_empty() {
        wait(libc::POLLOUT)?;
        match stream.write(unsent) {
            Ok(0) => anyhow::bail!("Native socket closed before request"),
            Ok(n) => unsent = &unsent[n..],
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e.into()),
        }
    }
    let mut response = Vec::new();
    loop {
        wait(libc::POLLIN)?;
        let mut chunk = [0; 8192];
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) if response.len() + n <= MAX_FRAME => response.extend_from_slice(&chunk[..n]),
            Ok(_) => return Err(incomplete("OpenCode state frame exceeds limit").into()),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e.into()),
        }
    }
    if response.last() != Some(&b'\n')
        || response.iter().filter(|byte| **byte == b'\n').count() != 1
    {
        return Err(incomplete("Invalid OpenCode frame").into());
    }
    Ok(response)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    ok: bool,
    protocol: u32,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    namespace: Option<String>,
    #[serde(default)]
    generation: Option<String>,
    #[serde(default)]
    attempt: Option<String>,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    instance: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    native_message_id: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default, rename = "uncertain")]
    _uncertain: Option<bool>,
}

fn unconfirmed_receipt_detail(receipt: &Receipt) -> String {
    let category = match receipt.error.as_deref() {
        Some("Native session, lease or insertion unconfirmed") => {
            "native session, lease or insertion unconfirmed"
        }
        Some(
            "Receipt journal capacity reached; existing native insertion requires reconciliation",
        ) => "receipt journal capacity reached after possible insertion",
        _ => "unconfirmed plugin receipt",
    };
    format!("OpenCode uncertain: {category}; read-only reconciliation required")
}

fn deferred_receipt_detail(receipt: &Receipt) -> &'static str {
    match receipt.error.as_deref() {
        Some("Native session identity or archive check failed before admission") => {
            "native session identity or archive check failed before admission"
        }
        Some("Native session lookup failed before admission") => {
            "native session lookup failed before admission"
        }
        Some("Native transcript lookup failed before admission") => {
            "native transcript lookup failed before admission"
        }
        Some("Broker admission was unavailable; no native attempt was made") => {
            "broker admission unavailable before native attempt"
        }
        Some("Receipt journal capacity reached; no native attempt was made") => {
            "receipt journal capacity reached before native attempt"
        }
        _ => "plugin deferred before native attempt",
    }
}

async fn exchange(path: &Path, frame: &Value, written: &mut bool) -> Result<Receipt> {
    let mut stream = UnixStream::connect(path).await?;
    crate::delivery::validate_peer(&stream, unsafe { libc::geteuid() })?;
    let mut bytes = serde_json::to_vec(frame)?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() <= MAX_FRAME,
        "OpenCode request exceeds frame limit"
    );
    // Once any write is attempted, loss of the receipt cannot establish rejection.
    *written = true;
    stream.write_all(&bytes).await?;
    let mut response = Vec::new();
    BufReader::new(stream)
        .take(MAX_FRAME as u64 + 1)
        .read_until(b'\n', &mut response)
        .await?;
    ensure!(
        response.len() <= MAX_FRAME && response.last() == Some(&b'\n'),
        "Invalid OpenCode receipt frame"
    );
    decode_receipt(&response, path, frame)
}

fn decode_receipt(response: &[u8], path: &Path, frame: &Value) -> Result<Receipt> {
    let receipt: Receipt = serde_json::from_slice(response).context("Invalid OpenCode receipt")?;
    ensure!(
        receipt.protocol == 2,
        "OpenCode plugin protocol version mismatch"
    );
    ensure!(
        receipt.session.as_deref() == frame.get("session").and_then(Value::as_str),
        "OpenCode receipt session mismatch"
    );
    ensure!(
        receipt.instance.as_deref() == Some(endpoint_instance(path)?.as_str()),
        "OpenCode receipt instance mismatch"
    );
    for key in [
        "id",
        "namespace",
        "generation",
        "attempt",
        "native_message_id",
    ] {
        if let Some(expected) = frame.get(key).and_then(Value::as_str) {
            let actual = match key {
                "id" => receipt.id.as_deref(),
                "namespace" => receipt.namespace.as_deref(),
                "generation" => receipt.generation.as_deref(),
                "attempt" => receipt.attempt.as_deref(),
                _ => receipt.native_message_id.as_deref(),
            };
            ensure!(actual == Some(expected), "OpenCode receipt {key} mismatch");
        }
    }
    Ok(receipt)
}

/// Registration uses the same connected-peer and exact-session checks as delivery.
/// Run on a blocking worker when called from the async broker.
pub(crate) fn verify_registration(agent: &Agent) -> Result<()> {
    validate_session(&agent.session)?;
    let path = validate_endpoint(&agent.endpoint)?;
    let frame = json!({"op":"probe","session":agent.session});
    let bytes = exchange_sync(&path, &frame, Instant::now() + Duration::from_secs(5))?;
    let receipt = decode_receipt(&bytes, &path, &frame)?;
    ensure!(receipt.ok, "OpenCode exact-session probe rejected");
    Ok(())
}

pub(crate) async fn deliver(_agent: &Agent, _msg: &Message, _content: &str) -> Outcome {
    Outcome::failed(
        "OpenCode delivery requires the broker's durable lease, queue and admission context",
    )
}

pub(crate) async fn deliver_dispatch(
    agent: &Agent,
    dispatch: &crate::opencode_state::Dispatch,
) -> Outcome {
    let path = match validate_endpoint(&agent.endpoint) {
        Ok(path) => path,
        Err(error) => return Outcome::uncertain(error),
    };
    let mut written = false;
    let result = timeout(
        Duration::from_secs(15),
        exchange(&path, &dispatch.frame, &mut written),
    )
    .await;
    match result {
        Ok(Ok(receipt)) if receipt.ok && receipt.status.as_deref() == Some("inserted") => {
            Outcome::submitted(
                "OpenCode exact native client acceptance verified; this does not prove transcript consumption, model reading or task completion",
            )
        }
        Ok(Ok(receipt))
            if !receipt.ok
                && receipt.status.as_deref() == Some("deferred")
                && dispatch.frame["op"] == "deliver" =>
        {
            Outcome {
                state: "queued",
                detail: format!(
                    "OpenCode deferred before insertion: {}",
                    deferred_receipt_detail(&receipt)
                ),
            }
        }
        Ok(Ok(receipt)) => Outcome::uncertain(unconfirmed_receipt_detail(&receipt)),
        Ok(Err(_)) => Outcome::uncertain(
            "OpenCode uncertain: transport or receipt validation failed; read-only reconciliation required",
        ),
        Err(_) => Outcome::uncertain(
            "OpenCode uncertain: transport timed out; read-only reconciliation required",
        ),
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    #[test]
    fn native_state_age_is_inclusive_unknown_preserving_and_saturating() {
        let now = 40 * 86_400_000;
        assert!(within_age(Some(now - 7 * 86_400_000), 7, now));
        assert!(!within_age(Some(now - 7 * 86_400_000 - 1), 7, now));
        assert!(within_age(None, 7, now));
        assert!(within_age(Some(1), u32::MAX, now));
        assert!(within_age(Some(i64::MIN), u32::MAX, i64::MIN));
    }
    #[test]
    fn unconfirmed_receipt_diagnostic_does_not_expose_untrusted_error_text() {
        let receipt: Receipt = serde_json::from_value(
            json!({"ok":false,"protocol":2,"status":"uncertain","error":"PRIVATE_BODY secret"}),
        )
        .unwrap();
        let detail = unconfirmed_receipt_detail(&receipt);
        assert!(!detail.contains("PRIVATE_BODY"));
        assert!(!detail.contains("secret"));
        assert!(detail.contains("uncertain"));
    }
}

//! Claude native socket adapter. No model session is launched here.
use crate::{
    delivery::Outcome,
    protocol::{Agent, Message},
};
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{io::AsyncWriteExt, net::UnixStream, time::timeout};

pub fn validate_session(session: &str) -> Result<()> {
    uuid::Uuid::parse_str(session).context("Claude registration requires a session UUID")?;
    Ok(())
}

pub fn validate_socket(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "Socket path must be absolute");
    let meta = std::fs::symlink_metadata(path)
        .context("Cannot inspect Claude socket; register a running native session")?;
    ensure!(
        meta.file_type().is_socket(),
        "Endpoint must be a Unix socket, not a symlink or regular file"
    );
    ensure!(
        meta.uid() == unsafe { libc::geteuid() },
        "Socket belongs to another user"
    );
    ensure!(
        meta.mode() & 0o077 == 0,
        "Socket must be accessible only to its owner"
    );
    let parent = path
        .parent()
        .context("Missing socket directory")?
        .canonicalize()?;
    let dir = std::fs::metadata(&parent)?;
    ensure!(
        dir.uid() == unsafe { libc::geteuid() } && dir.mode() & 0o077 == 0,
        "Socket directory must be private to your user"
    );
    let normalized = parent.join(path.file_name().context("Missing socket filename")?);
    ensure!(
        normalized.as_os_str().len() < 104,
        "Socket path is too long for macOS"
    );
    Ok(normalized)
}

/// Check the native listener without sending a frame or waiting for it to read.
/// A busy listener or local resource exhaustion is inconclusive, not proof that
/// the session exited. Keep those sessions visible rather than retiring them.
pub fn stale_reason(path: &Path) -> Option<String> {
    let path = match validate_socket(path) {
        Ok(path) => path,
        Err(error) => {
            return Some(
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                {
                    "socket_missing"
                } else {
                    "socket_invalid"
                }
                .into(),
            );
        }
    };
    // SOCK_NONBLOCK is not portable to macOS; set it before connect instead.
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return None;
    }
    // SAFETY: socket returned a fresh descriptor, owned exclusively here.
    let socket = unsafe { OwnedFd::from_raw_fd(raw) };
    if unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0
        || unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0
    {
        return None;
    }
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Some("socket_invalid".into());
    }
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = *byte as libc::c_char;
    }
    let len =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = len as u8;
    }
    // SAFETY: address is initialized and len includes only its family, path,
    // and terminating NUL; the descriptor is nonblocking and stays owned here.
    let connected = unsafe {
        libc::connect(
            socket.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            len,
        )
    };
    if connected == 0 {
        return None;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ENOENT) => Some("socket_missing".into()),
        Some(libc::ECONNREFUSED) => Some("socket_refused".into()),
        Some(libc::ENOTSOCK | libc::EACCES | libc::ENOTDIR) => Some("socket_invalid".into()),
        _ => None,
    }
}

pub fn claude_frame(msg: &Message, _state_dir: &Path) -> serde_json::Value {
    frame(msg, &claude_content(msg, &serde_json::Value::Null))
}

fn frame(msg: &Message, content: &str) -> serde_json::Value {
    json!({"type":"user","session_id":msg.recipient_session,"from":msg.sender,"msg_id":msg.id,"message":{"role":"user","content":content}})
}

/// Claude's native peer renderer recognises this canonical wrapper. Display
/// names are labels only; the compact reference retains the stable routing IDs.
pub(crate) fn claude_content(msg: &Message, machines: &serde_json::Value) -> String {
    let name = machines["sender_name"].as_str().unwrap_or(&msg.sender);
    let name = claude_display_name(name);
    let name = if name.is_empty() {
        claude_display_name(&msg.sender)
    } else {
        name
    };
    let body = escape_claude_body(&msg.body);
    format!(
        "<cross-session-message from-name=\"{name}\">\n{body}\n\nMessage reference: {}\nReply to: {}\nDelivered to: {}\n</cross-session-message>",
        msg.id, msg.sender, msg.recipient
    )
}

fn claude_display_name(name: &str) -> String {
    use unicode_general_category::{GeneralCategory as Category, get_general_category};
    let name: String = name
        .chars()
        .filter(|c| {
            !matches!(c, '"' | '<' | '>')
                && !matches!(
                    get_general_category(*c),
                    Category::Control
                        | Category::Format
                        | Category::LineSeparator
                        | Category::ParagraphSeparator
                )
        })
        .collect();
    let mut chars = name.trim().chars();
    let mut result: String = chars.by_ref().take(64).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}

// Bracket lookalikes recognised by Claude 2.1.300's closing-tag matcher.
const OPEN_BRACKETS: &str = "<＜﹤〈⟨〈‹˂ᐸ❬❮❰⧼≮≺⋖";
const CLOSE_BRACKETS: &str = ">＞﹥〉⟩〉›˃ᐳ❭❯❱⧽≯≻⋗";

fn envelope_boundary(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
        || matches!(ch, 'ſ' | 'K' | '_' | '-' | '/' | '／' | '∕' | '⁄')
        || OPEN_BRACKETS.contains(ch)
        || CLOSE_BRACKETS.contains(ch)
}

fn escape_claude_body(body: &str) -> String {
    // Claude escapes closing envelope tags with a backslash after '<', not XML
    // entities. Leave ordinary HTML/code intact and preserve already escaped tags.
    let mut result = String::with_capacity(body.len());
    for (index, ch) in body.char_indices() {
        let suffix = &body[index + ch.len_utf8()..];
        let closing = (OPEN_BRACKETS.contains(ch) && !suffix.starts_with('\\'))
            .then(|| suffix.char_indices().find(|(_, c)| envelope_boundary(*c)))
            .flatten();
        let envelope = closing.is_some_and(|(offset, c)| {
            matches!(c, '/' | '／' | '∕' | '⁄') && closing_envelope_name(&suffix[offset..])
        });
        if envelope {
            result.push_str("<\\");
        } else {
            result.push(ch);
        }
    }
    result
}

fn envelope_separator(ch: char) -> bool {
    use unicode_general_category::{GeneralCategory as Category, get_general_category};
    matches!(
        get_general_category(ch),
        Category::DashPunctuation | Category::ConnectorPunctuation
    ) || matches!(
        ch,
        '‗' | 'ˍ' | 'ߺ' | 'ـ' | '−' | '⁻' | '₋' | '˗' | '➖' | '⁃' | 'ー' | 'ｰ'
    )
}

fn closing_envelope_name(suffix: &str) -> bool {
    let mut chars = suffix.chars();
    chars.next(); // slash (possibly a Unicode lookalike)
    for (index, expected) in "cross-session-message".chars().enumerate() {
        let next = chars.find(|ch| {
            !(index == 0 && matches!(ch, '/' | '／' | '∕' | '⁄'))
                && (envelope_boundary(*ch) || expected == '-' && envelope_separator(*ch))
        });
        let Some(next) = next else {
            return false;
        };
        if if expected == '-' {
            !envelope_separator(next)
        } else {
            !(next.eq_ignore_ascii_case(&expected) || expected == 's' && next == 'ſ')
        } {
            return false;
        }
    }
    !chars
        .next()
        .is_some_and(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | 'ſ' | 'K'))
}

pub(crate) async fn deliver(agent: &Agent, msg: &Message, content: &str) -> Outcome {
    deliver_guarded(agent, msg, content, None).await
}

pub(crate) async fn deliver_guarded(
    agent: &Agent,
    msg: &Message,
    content: &str,
    guard: Option<&super::NoticeGuard>,
) -> Outcome {
    let path = match validate_socket(Path::new(&agent.endpoint)) {
        Ok(p) => p,
        Err(e) => return Outcome::failed(e),
    };
    let mut stream = match timeout(Duration::from_secs(3), UnixStream::connect(path)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Outcome::failed(e),
        Err(e) => return Outcome::failed(e),
    };
    if let Err(error) = crate::delivery::validate_peer(&stream, unsafe { libc::geteuid() }) {
        return Outcome::failed(error);
    }
    if let Some(guard) = guard
        && let Err(error) = guard()
    {
        return Outcome::failed(error);
    }
    let frame = frame(msg, content);
    let mut bytes = serde_json::to_vec(&frame).expect("JSON value serialization");
    bytes.push(b'\n');
    write_frame(&mut stream, &bytes).await
}

async fn write_frame(stream: &mut UnixStream, bytes: &[u8]) -> Outcome {
    let mut sent = 0;
    let result = timeout(Duration::from_secs(5), async {
        while sent < bytes.len() {
            let n = stream.write(&bytes[sent..]).await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "Socket closed",
                ));
            }
            sent += n;
        }
        stream.shutdown().await
    })
    .await;
    match result {
        Ok(Ok(())) => Outcome::submitted(
            "Written to Claude inbox socket; native acceptance and model execution are unconfirmed",
        ),
        other => {
            let detail = format!("Claude socket write failed after {sent} bytes: {other:?}");
            if sent == 0 {
                Outcome::failed(detail)
            } else {
                Outcome::uncertain(detail)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn completed_and_partial_socket_writes_keep_distinct_operational_evidence() {
        use std::os::fd::AsRawFd;
        use tokio::io::AsyncReadExt;
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        let buffer: libc::c_int = 1024;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    sender.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&buffer as *const libc::c_int).cast(),
                    std::mem::size_of_val(&buffer) as libc::socklen_t,
                )
            },
            0
        );
        let receive = tokio::spawn(async move {
            let mut byte = [0u8; 1];
            receiver.read_exact(&mut byte).await.unwrap();
            // Close only after a prefix was actually written, while the small
            // sender buffer prevents the complete frame from being accepted.
        });
        let outcome = write_frame(&mut sender, &vec![b'x'; 64 * 1024]).await;
        receive.await.unwrap();
        assert_eq!(outcome.state, "uncertain");
        assert_eq!(
            crate::protocol::delivery_outcome(outcome.state, Some("claude"), None),
            "unknown"
        );

        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        let outcome = write_frame(&mut sender, b"complete frame\n").await;
        assert_eq!(outcome.state, "submitted");
        let mut bytes = Vec::new();
        receiver.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, b"complete frame\n");
        assert_eq!(
            crate::protocol::delivery_outcome(outcome.state, Some("claude"), None),
            "unknown"
        );
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        drop(receiver);
        assert_eq!(
            write_frame(&mut sender, b"not attempted\n").await.state,
            "failed"
        );
    }

    #[tokio::test]
    async fn event_guard_refreshes_after_connect_and_prevents_native_frame() {
        use std::os::unix::fs::PermissionsExt;
        use tokio::io::AsyncReadExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("native.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let agent = Agent {
            name: "claude-test".into(),
            provider: crate::protocol::Provider::Claude,
            session: "123e4567-e89b-12d3-a456-426614174000".into(),
            endpoint: path.to_string_lossy().into_owned(),
            enabled: true,
        };
        let guard: super::super::NoticeGuard =
            std::sync::Arc::new(|| anyhow::bail!("subscription revoked"));
        let outcome =
            deliver_guarded(&agent, &message("typed event"), "typed event", Some(&guard)).await;
        assert_eq!(outcome.state, "failed");
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut received = Vec::new();
        stream.read_to_end(&mut received).await.unwrap();
        assert!(received.is_empty());
    }

    #[test]
    fn liveness_checks_listener_not_just_socket_file() {
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};

        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("native.sock");
        assert_eq!(stale_reason(&path).as_deref(), Some("socket_missing"));
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(stale_reason(&path), None);
        let (mut accepted, _) = listener.accept().unwrap();
        let mut bytes = [0; 1];
        assert_eq!(std::io::Read::read(&mut accepted, &mut bytes).unwrap(), 0);
        drop(listener);
        assert!(path.exists());
        assert_eq!(stale_reason(&path).as_deref(), Some("socket_refused"));
    }

    #[test]
    fn liveness_rejects_invalid_or_public_endpoints() {
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};

        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("native.sock");
        std::fs::write(&path, b"not a socket").unwrap();
        assert_eq!(stale_reason(&path).as_deref(), Some("socket_invalid"));
        std::fs::remove_file(&path).unwrap();
        let _listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(stale_reason(&path).as_deref(), Some("socket_invalid"));
        assert_eq!(
            stale_reason(Path::new("relative.sock")).as_deref(),
            Some("socket_invalid")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn liveness_does_not_wait_for_a_full_listener_backlog() {
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};

        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("native.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 1) }, 0);
        for _ in 0..4 {
            assert_eq!(stale_reason(&path), None);
        }
    }

    fn message(body: &str) -> Message {
        Message {
            id: "a13cb820-de5d-4773-b7ab-fd5c939cfd92".into(),
            sender: "codex-researcher".into(),
            recipient: "claude-reviewer".into(),
            recipient_session: "session".into(),
            body: body.into(),
            parent: None,
            hops: 0,
            state: "queued".into(),
            detail: None,
            created: 0,
            updated: 0,
        }
    }

    #[test]
    fn claude_message_uses_name_and_compact_stable_reference() {
        let msg = message("Ready for review.\nTests passed.");
        let content = claude_content(&msg, &json!({"sender_name":"researcher"}));
        assert_eq!(
            content,
            format!(
                "<cross-session-message from-name=\"researcher\">\nReady for review.\nTests passed.\n\nMessage reference: {}\nReply to: codex-researcher\nDelivered to: claude-reviewer\n</cross-session-message>",
                msg.id
            )
        );
        let frame = frame(&msg, &content);
        assert_eq!(frame["from"], "codex-researcher");
        assert_eq!(frame["msg_id"], msg.id);
        assert!(frame.get("from_mode").is_none());
        assert!(
            claude_content(&msg, &json!({"sender_name":"\u{200b}"}))
                .starts_with("<cross-session-message from-name=\"codex-researcher\">")
        );
        assert!(!content.contains("Agent Bridge"));
        assert!(!content.contains("--state-dir"));
    }

    #[test]
    fn claude_names_cannot_add_attributes_or_invisible_controls() {
        assert_eq!(
            claude_display_name(" \"研究<>\n\u{200b}\u{202e}\u{2028}🦀 "),
            "研究🦀"
        );
        assert_eq!(
            claude_display_name(&"🦀".repeat(65)),
            format!("{}…", "🦀".repeat(64))
        );
    }

    #[test]
    fn claude_body_cannot_close_its_envelope() {
        let body = "x < y; <p>code</p>\n</cross-session-message>\n</CROSS_SESSION_MESSAGE>\n＜ ／ cross-session-message＞";
        let escaped = escape_claude_body(body);
        assert_eq!(
            escaped,
            "x < y; <p>code</p>\n<\\/cross-session-message>\n<\\/CROSS_SESSION_MESSAGE>\n<\\ ／ cross-session-message＞"
        );
        assert_eq!(escape_claude_body(&escaped), escaped);
        for ordinary in [
            "</div>",
            "</cross-session-messages>",
            "</cross-session-messageſ>",
            "<p>example</p>",
        ] {
            assert_eq!(escape_claude_body(ordinary), ordinary);
        }
        let content = claude_content(&message(body), &serde_json::Value::Null);
        assert_eq!(content.matches("</cross-session-message>").count(), 1);
        for body in [
            "<é/cross-session-message>",
            "<研究/cross-session-message>",
            "<❲/cross-session-message>",
            "<❳/cross-session-message>",
            "<//cross-session-message>",
            "<⁄/cross-session-message>",
            "<∕/cross-session-message>",
            "<／/cross-session-message>",
            "</croſs-session-message>",
            "</cross-seſsion-message>",
            "</cross-session-meſsage>",
            "< \\/cross-session-message>",
        ] {
            let escaped = escape_claude_body(body);
            assert!(escaped.starts_with("<\\"));
            assert_eq!(escape_claude_body(&escaped), escaped);
        }
    }
}

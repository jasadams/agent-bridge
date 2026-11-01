use serde::{Deserialize, Serialize};

pub const MAX_FRAME: usize = 256 * 1024;
pub const MAX_BODY: usize = 64 * 1024;
pub const MAX_REPLY_HOPS: u32 = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Claude,
    Codex,
    #[serde(rename = "opencode")]
    #[value(name = "opencode")]
    OpenCode,
    Inbox,
}

impl std::str::FromStr for Provider {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "opencode" => Ok(Self::OpenCode),
            "inbox" => Ok(Self::Inbox),
            _ => anyhow::bail!("Unknown provider: {value}"),
        }
    }
}

impl Provider {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Inbox => "inbox",
        }
    }
}

/// Explicit, bounded local event interests; selector-specific fields are strict.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "event", deny_unknown_fields)]
pub enum EventSelector {
    #[serde(rename = "board.changed")]
    BoardChanged { board: crate::board::Name },
    #[serde(rename = "agent.opened")]
    AgentOpened {},
    #[serde(rename = "agent.closed")]
    AgentClosed {},
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub name: String,
    pub provider: Provider,
    pub session: String,
    pub endpoint: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub sender: String,
    pub recipient: String,
    pub recipient_session: String,
    pub body: String,
    pub parent: Option<String>,
    pub hops: u32,
    pub state: String,
    pub detail: Option<String>,
    pub created: i64,
    pub updated: i64,
}

/// One durable lifecycle, exposed as four delivery outcomes. Legacy names stay
/// on disk and peer protocol v1 so old peers can still synchronize history.
pub fn delivery_state(state: &str) -> &'static str {
    match state {
        "queued" | "sending" | "blocked" | "pending" => "pending",
        "submitted" | "acknowledged" | "delivered" => "delivered",
        "failed" | "cancelled" => "failed",
        _ => "unknown",
    }
}

pub(crate) fn delivery_outcome(
    state: &str,
    provider: Option<&str>,
    phase: Option<&str>,
) -> &'static str {
    if (state == "submitted" && !matches!(provider, Some("codex" | "opencode" | "inbox")))
        || (state == "queued" && phase.is_some_and(|phase| phase != "queued"))
        || (state == "sending" && matches!(phase, Some("reconcile" | "legacy" | "uncertain")))
    {
        "unknown"
    } else {
        delivery_state(state)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Register {
        agent: Agent,
        #[serde(default)]
        replace: bool,
    },
    #[serde(rename = "opencode_observe")]
    OpenCodeObserve {
        session: String,
        endpoint: String,
        title: Option<String>,
        #[serde(default)]
        ready: bool,
    },
    #[serde(rename = "opencode_heartbeat")]
    OpenCodeHeartbeat {
        session: String,
        endpoint: String,
        generation: String,
        ready: bool,
    },
    #[serde(rename = "opencode_close")]
    OpenCodeClose {
        session: String,
        endpoint: String,
        generation: String,
    },
    #[serde(rename = "opencode_admit")]
    OpenCodeAdmit {
        id: String,
        session: String,
        generation: String,
        instance: String,
        attempt: String,
    },
    #[serde(rename = "opencode_notice")]
    OpenCodeNotice {
        id: String,
        recipient: String,
        content: String,
        #[serde(default)]
        event_token: Option<String>,
    },
    CodexNotice {
        id: String,
        recipient: String,
        session: String,
        content: String,
        #[serde(default)]
        event_token: Option<String>,
    },
    Unregister {
        name: String,
        session: String,
    },
    /// A native SessionEnd retires every alias of the verified exact session.
    SessionClose {
        name: String,
        session: String,
        endpoint: String,
    },
    List {
        #[serde(default)]
        all: bool,
    },
    Send {
        id: String,
        sender: String,
        recipient: String,
        body: String,
        #[serde(default)]
        parent: Option<String>,
    },
    History {
        #[serde(default)]
        agent: Option<String>,
        #[serde(default = "history_limit")]
        limit: usize,
    },
    /// Creation-only incremental history in insertion order. An empty cursor starts a scan.
    HistorySince {
        #[serde(default)]
        agent: Option<String>,
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default = "history_limit")]
        limit: usize,
    },
    Get {
        id: String,
    },
    Retry {
        id: String,
        #[serde(default)]
        allow_duplicate: bool,
    },
    Cancel {
        id: String,
    },
    Pause {
        paused: bool,
    },
    BoardMessageSend {
        request: crate::board_messages::Submission,
    },
    AddBoardMessageInput {
        agent: String,
        board: crate::board::Name,
        expected_revision: String,
        id: String,
        label: String,
    },
    BoardList,
    BoardArchivedList,
    BoardInspect {
        board: crate::board::Name,
    },
    BoardArchive {
        board: crate::board::Name,
        expected_revision: String,
    },
    BoardRestore {
        board: crate::board::Name,
        expected_revision: String,
    },
    BoardCreate {
        board: crate::board::Name,
    },
    NamedBoardRead {
        board: crate::board::Name,
    },
    NamedBoardUpdate {
        board: crate::board::Name,
        expected_revision: String,
        edits: Vec<crate::board::Edit>,
        #[serde(default)]
        actor: Option<String>,
    },
    Subscribe {
        agent: String,
        events: Vec<EventSelector>,
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        board_revisions: std::collections::BTreeMap<crate::board::Name, String>,
    },
    Unsubscribe {
        agent: String,
        #[serde(default)]
        events: Option<Vec<EventSelector>>,
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        board_revisions: std::collections::BTreeMap<crate::board::Name, String>,
    },
    BoardRead,
    BoardUpdate {
        expected_revision: String,
        edits: Vec<crate::board::Edit>,
        #[serde(default)]
        actor: Option<String>,
    },
    Status,
    MonitorEnable {
        port: u16,
    },
    MonitorDisable,
    MonitorStatus,
    MonitorAccess,
    Identity,
    MachineShow,
    MachineSetName {
        name: String,
    },
    RelayList,
    RelayAdd {
        address: String,
    },
    RelayRemove {
        peer_id: String,
    },
    PeersList,
    PeerConnections,
    SharedSessions,
    PeerUnshareSession {
        peer: String,
        provider: Provider,
        session: String,
    },
    PeerShare {
        peer: String,
        agent: String,
        shared: bool,
    },
    PeersDiscover,
    NetworkLan {
        #[serde(default)]
        listen: Option<String>,
    },
    ContactCreate {
        seconds: u64,
    },
    ContactConnect {
        link: String,
        use_invite_relays: bool,
        seconds: u64,
    },
    ContactLocation {
        location: String,
        seconds: u64,
    },
    LanPairingOpen {
        seconds: u64,
    },
    LanPairingWatch {
        agent: String,
        #[serde(default)]
        id: Option<String>,
        seconds: u64,
    },
    LanPairingClose,
    LanPairingList,
    LanPairingConnect {
        peer: String,
        #[serde(default)]
        address: Option<String>,
        seconds: u64,
    },
    LanPairingAccept {
        id: String,
    },
    LanPairingConfirm {
        id: String,
        code: String,
    },
    LanPairingCancel {
        id: String,
    },
    PairingOffer {
        seconds: u64,
    },
    PairingJoin {
        token: String,
        fingerprint: String,
        #[serde(default)]
        alias: Option<String>,
    },
    PairingFinish {
        token: String,
        fingerprint: String,
        #[serde(default)]
        alias: Option<String>,
    },
    PairingCancel {
        nonce: String,
    },
    InvitationCreate {
        seconds: u64,
    },
    InvitationAccept {
        token: String,
        fingerprint: String,
        #[serde(default)]
        alias: Option<String>,
    },
    RemoteList,
    RemoteListPeer {
        peer: String,
    },
    NetworkConfigure {
        enabled: bool,
        #[serde(default)]
        listen: Option<String>,
    },
    PeerAddress {
        peer: String,
        address: String,
    },
    PeerPair {
        peer_id: String,
        fingerprint: String,
        #[serde(default)]
        alias: Option<String>,
    },
    PeerGrant {
        peer: String,
        agent: String,
        permission: crate::network::trust::Permission,
        allow: bool,
    },
    PeerRevoke {
        peer: String,
    },
}
impl Request {
    /// Main has no archive lifecycle and keeps its frozen legacy read wire.
    /// Named inspection never falls back to main on an older broker.
    pub fn board_inspect(board: crate::board::Name) -> Self {
        if board.is_main() {
            Self::BoardRead
        } else {
            Self::BoardInspect { board }
        }
    }

    pub fn board_read(board: crate::board::Name) -> Self {
        if board.is_main() {
            Self::BoardRead
        } else {
            Self::NamedBoardRead { board }
        }
    }
    pub fn board_update(
        board: crate::board::Name,
        expected_revision: String,
        edits: Vec<crate::board::Edit>,
        actor: Option<String>,
    ) -> Self {
        if board.is_main() {
            Self::BoardUpdate {
                expected_revision,
                edits,
                actor,
            }
        } else {
            Self::NamedBoardUpdate {
                board,
                expected_revision,
                edits,
                actor,
            }
        }
    }
}

fn history_limit() -> usize {
    30
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_persisted: Option<bool>,
}

#[derive(Debug)]
pub(crate) struct SendNotPersisted(pub String);

impl std::fmt::Display for SendNotPersisted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SendNotPersisted {}

impl Response {
    pub fn from_result(result: anyhow::Result<serde_json::Value>) -> Self {
        match result {
            Ok(result) => Self {
                ok: true,
                result: Some(result),
                error: None,
                send_persisted: None,
            },
            Err(error) => Self {
                ok: false,
                result: None,
                error: Some(format!("{error:#}")),
                send_persisted: error.downcast_ref::<SendNotPersisted>().map(|_| false),
            },
        }
    }
}

#[cfg(test)]
mod send_response_tests {
    use super::*;

    #[derive(Deserialize)]
    struct LegacyResponse {
        ok: bool,
        error: Option<String>,
    }

    #[test]
    fn rejection_metadata_preserves_legacy_response_compatibility() {
        let legacy: Response = serde_json::from_str(r#"{"ok":false,"error":"failed"}"#).unwrap();
        assert_eq!(legacy.send_persisted, None);
        let response = Response::from_result(Err(SendNotPersisted("failed".into()).into()));
        let json = serde_json::to_string(&response).unwrap();
        let old: LegacyResponse = serde_json::from_str(&json).unwrap();
        assert!(!old.ok);
        assert_eq!(old.error.as_deref(), Some("failed"));
        assert_eq!(response.send_persisted, Some(false));
    }
}

#[cfg(test)]
mod board_compatibility_tests {
    use super::*;
    use serde_json::json;

    // Frozen old board protocol: unit reads and strict unknown-field handling.
    #[derive(Debug, Deserialize)]
    #[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
    enum OldBoardRequest {
        #[serde(rename = "board_read")]
        Read,
        #[serde(rename = "board_update")]
        Update {
            expected_revision: String,
            edits: Vec<crate::board::Edit>,
            #[serde(default)]
            actor: Option<String>,
        },
    }
    fn old_accepts(value: serde_json::Value) -> bool {
        // Consume fields as an old handler would, without executing any side effects.
        match serde_json::from_value::<OldBoardRequest>(value) {
            Ok(OldBoardRequest::Read) => true,
            Ok(OldBoardRequest::Update {
                expected_revision,
                edits,
                actor,
            }) => !expected_revision.is_empty() && !edits.is_empty() && actor.is_none(),
            Err(_) => false,
        }
    }
    #[test]
    fn main_builders_keep_legacy_wire_fixtures() {
        let main = crate::board::Name::default();
        let revision = "0".repeat(64);
        let fixtures = [
            (
                Request::board_read(main.clone()),
                json!({"op":"board_read"}),
            ),
            (
                Request::board_update(
                    main.clone(),
                    revision.clone(),
                    vec![crate::board::Edit::Append { text: "x".into() }],
                    None,
                ),
                json!({"op":"board_update","expected_revision":revision,"edits":[{"op":"append","text":"x"}],"actor":null}),
            ),
        ];
        for (request, fixture) in fixtures {
            assert_eq!(serde_json::to_value(request).unwrap(), fixture);
            assert!(old_accepts(fixture.clone()));
            assert!(serde_json::from_value::<Request>(fixture).is_ok());
        }
    }
    #[test]
    fn old_protocol_stub_rejects_every_distinct_named_operation() {
        let name: crate::board::Name = "release".parse().unwrap();
        for request in [
            Request::board_read(name.clone()),
            Request::board_update(
                name.clone(),
                "0".repeat(64),
                vec![crate::board::Edit::Append {
                    text: "must not reach main".into(),
                }],
                None,
            ),
            Request::BoardCreate { board: name },
            Request::BoardList,
        ] {
            let wire = serde_json::to_value(request).unwrap();
            assert!(!old_accepts(wire.clone()), "{wire}");
            assert!(serde_json::from_value::<Request>(wire).is_ok());
        }
    }
}

use serde::{Deserialize, Serialize};

pub const MAX_FRAME: usize = 256 * 1024;
pub const MAX_BODY: usize = 64 * 1024;

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
    },
    Unregister {
        name: String,
        session: String,
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
    Ack {
        id: String,
        agent: String,
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
    BoardRead,
    BoardUpdate {
        expected_revision: String,
        edits: Vec<crate::board::Edit>,
        #[serde(default)]
        actor: Option<String>,
    },
    BoardSubscribe {
        agent: String,
    },
    BoardUnsubscribe {
        agent: String,
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
}

impl Response {
    pub fn from_result(result: anyhow::Result<serde_json::Value>) -> Self {
        match result {
            Ok(result) => Self {
                ok: true,
                result: Some(result),
                error: None,
            },
            Err(error) => Self {
                ok: false,
                result: None,
                error: Some(format!("{error:#}")),
            },
        }
    }
}

//! Paired brokers must not poll each other's directory in the background. The
//! result of such a poll was discarded (peer directories are fetched on demand
//! by `list_agents --peer`), yet each poll made the responder spawn the Claude
//! CLI every 5 s. The 60 s capability probe is deliberately out of scope here
//! (its window is longer than this test), so this proves the 5 s poll is gone,
//! not that an idle broker never lists natively.

use agent_bridge::{
    broker::{self, Broker},
    network::trust::Permission,
    protocol::Request,
};
use serde_json::Value;
use std::{os::unix::fs::PermissionsExt, path::Path, time::Duration};
use tokio::{task::JoinHandle, time::timeout};

struct Running {
    broker: Broker,
    task: JoinHandle<anyhow::Result<()>>,
}
impl Running {
    fn start(state: &Path) -> Self {
        let bound = Broker::bind(state.into(), "/missing/codex".into()).unwrap();
        let broker = bound.broker.clone();
        Self {
            broker,
            task: tokio::spawn(broker::serve(bound)),
        }
    }
    fn request(&self, request: Request) -> Value {
        self.broker.request(request).unwrap()
    }
    fn identity(&self) -> String {
        self.request(Request::Identity)["peer_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

fn fixed_address() -> String {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    format!(
        "/ip4/127.0.0.1/udp/{}/quic-v1",
        socket.local_addr().unwrap().port()
    )
}

/// User + system CPU time of this process in clock ticks (1/100 s).
#[cfg(target_os = "linux")]
fn cpu_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let fields: Vec<&str> = stat
        .rsplit(')')
        .next()
        .unwrap()
        .split_whitespace()
        .collect();
    fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
}

fn spawns(log: &Path) -> usize {
    std::fs::read_to_string(log).map_or(0, |text| text.lines().count())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_paired_brokers_never_list_native_sessions_but_peer_listing_still_works() {
    let root = tempfile::Builder::new()
        .prefix("ab-idle-")
        .tempdir_in("/tmp")
        .unwrap();
    // A fake `claude` that records every invocation. Both brokers share it.
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let log = root.path().join("claude-calls.log");
    let fake = bin.join("claude");
    std::fs::write(
        &fake,
        format!("#!/bin/sh\necho \"$@\" >> '{}'\necho '[]'\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: this test binary holds only this test and no broker or thread
    // exists yet, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        );
    }

    let alice = Running::start(&root.path().join("a"));
    let bob = Running::start(&root.path().join("b"));
    let alice_id = alice.identity();
    let bob_id = bob.identity();
    for (bridge, peer, alias) in [(&alice, &bob_id, "bob"), (&bob, &alice_id, "alice")] {
        bridge.request(Request::PeerPair {
            peer_id: peer.clone(),
            fingerprint: peer.clone(),
            alias: Some(alias.into()),
        });
        for permission in [Permission::Discover, Permission::Receive, Permission::Send] {
            bridge.request(Request::PeerGrant {
                peer: alias.into(),
                agent: "user".into(),
                permission,
                allow: true,
            });
        }
    }
    let alice_address = fixed_address();
    let bob_address = fixed_address();
    for (bridge, address) in [(&alice, &alice_address), (&bob, &bob_address)] {
        bridge.request(Request::NetworkConfigure {
            enabled: true,
            listen: Some(address.clone()),
        });
    }
    alice.request(Request::PeerAddress {
        peer: "bob".into(),
        address: bob_address,
    });
    bob.request(Request::PeerAddress {
        peer: "alice".into(),
        address: alice_address,
    });
    // Both sides connected and exchanged Hello: the worker loop is live.
    timeout(Duration::from_secs(40), async {
        loop {
            let connected = |bridge: &Running, id: &str| {
                bridge
                    .request(Request::PeerConnections)
                    .as_array()
                    .is_some_and(|peers| {
                        peers
                            .iter()
                            .any(|p| p["peer_id"] == id && p["connected"] == true)
                    })
            };
            if connected(&alice, &bob_id) && connected(&bob, &alice_id) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("brokers never connected");

    // Idle window: nothing is pending, so nothing may poll. Zero spawns covers
    // the old 5 s directory poll and the connect/60 s capability probe; the CPU
    // budget covers the old 200 ms network loop re-reading the database.
    tokio::time::sleep(Duration::from_secs(2)).await;
    #[cfg(target_os = "linux")]
    let cpu_before = cpu_ticks();
    tokio::time::sleep(Duration::from_secs(14)).await;
    #[cfg(target_os = "linux")]
    let cpu_used = cpu_ticks() - cpu_before;
    // Debug build, two brokers plus libp2p in one process: the fixed tree uses
    // about 50 ticks here, the 200 ms re-reading loop about 156.
    #[cfg(target_os = "linux")]
    assert!(
        cpu_used < 110,
        "idle paired brokers used {cpu_used} CPU ticks (1/100 s) in 14 s"
    );
    assert_eq!(
        spawns(&log),
        0,
        "idle paired brokers spawned the native CLI: {}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    let before = spawns(&log);

    // On-demand peer listing still asks the peer to list natively.
    let listed = alice
        .broker
        .request_async(Request::RemoteListPeer { peer: "bob".into() });
    let listed = timeout(Duration::from_secs(20), listed)
        .await
        .expect("peer listing timed out")
        .expect("peer listing failed");
    assert!(listed.is_array(), "{listed}");
    assert!(
        spawns(&log) > before,
        "on-demand peer listing must still list natively"
    );
    alice.task.abort();
    bob.task.abort();
}

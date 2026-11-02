// Each integration target compiles its own copy and uses a different subset.
// This exemption is only for shared test utilities, never production code.
#![allow(dead_code)]
use fs2::FileExt;
use std::{fs::OpenOptions, io::ErrorKind, path::Path, time::Duration};

pub mod claude_directory;

/// Keep unrelated provider/board fixtures off the owner's native directory.
pub fn bind(
    state: std::path::PathBuf,
    codex: std::path::PathBuf,
) -> anyhow::Result<agent_bridge::broker::BoundServer> {
    let mut bound = agent_bridge::broker::Broker::bind(state, codex)?;
    let fixture = bound.broker.state_dir.join("native-cli");
    std::fs::create_dir_all(&fixture)?;
    bound.broker = bound
        .broker
        .clone()
        .with_claude_binary(claude_directory::write_cli(
            &fixture,
            &serde_json::json!([]),
        ));
    Ok(bound)
}

/// Aborting and joining an async server does not cancel started blocking jobs.
/// Wait for their ownership fence before rebinding or modifying its database.
pub async fn wait_for_shutdown(state: &Path) {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(state.join("service.lock"))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match lock.try_lock_exclusive() {
                Ok(()) => {
                    FileExt::unlock(&lock).unwrap();
                    break;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("Could not check broker ownership: {error}"),
            }
        }
    })
    .await
    .expect("Old broker did not release ownership after blocking jobs finished");
}

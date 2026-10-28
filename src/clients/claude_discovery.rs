//! Observe native Claude title records. Never modify a transcript or launch a session.
//! The JSONL layout is an internal Claude format, isolated here for compatibility updates.
use crate::broker::Broker;
use anyhow::Result;
use notify::{RecursiveMode, Watcher};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs::OpenOptions,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Default)]
pub struct Titles {
    positions: HashMap<PathBuf, (u64, u64)>,
}
impl Titles {
    pub fn refresh(&mut self, broker: &Broker, projects: &Path) -> Result<()> {
        let sessions: HashSet<_> = broker.claude_sessions()?.into_iter().collect();
        for project in std::fs::read_dir(projects)? {
            let project = project?;
            if !project.file_type()?.is_dir() {
                continue;
            }
            for session in &sessions {
                // Session IDs originate in native hooks, but still must not become arbitrary paths.
                if super::claude::validate_session(session).is_err() {
                    continue;
                }
                let path = project.path().join(format!("{session}.jsonl"));
                if path.exists() {
                    self.read(broker, &path, session)?;
                }
            }
        }
        Ok(())
    }

    fn read(&mut self, broker: &Broker, path: &Path, session: &str) -> Result<()> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } {
            return Ok(());
        }
        let (inode, offset) = self.positions.entry(path.into()).or_default();
        if *inode != meta.ino() || meta.len() < *offset {
            *offset = 0;
            *inode = meta.ino();
        }
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(*offset))?;
        loop {
            let mut line = Vec::new();
            // Bound allocations even when a transcript contains very large tool outputs.
            let n = reader
                .by_ref()
                .take(256 * 1024)
                .read_until(b'\n', &mut line)?;
            if n == 0 {
                break;
            }
            let mut complete = line.last() == Some(&b'\n');
            if !complete && n == 256 * 1024 {
                complete = reader.skip_until(b'\n')? > 0;
                // skip_until also returns a count at EOF; only advance complete records below.
                if complete {
                    let end = reader.stream_position()?;
                    reader.seek(SeekFrom::Start(end - 1))?;
                    let mut last = [0];
                    reader.read_exact(&mut last)?;
                    complete = last[0] == b'\n';
                }
                if complete {
                    *offset = reader.stream_position()?;
                    continue;
                }
            }
            // A rename may be written in multiple writes. Re-read an incomplete last record later.
            if !complete {
                break;
            }
            *offset = reader.stream_position()?;
            let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if value["sessionId"] != session {
                continue;
            }
            let title = match value["type"].as_str() {
                Some("custom-title") => value["customTitle"].as_str(),
                Some("agent-name") => value["agentName"].as_str(),
                _ => None,
            };
            if let Some(title) = title {
                broker.observe_claude_name(session, title)?;
            }
        }
        Ok(())
    }
}

pub async fn observe(broker: &Broker, projects: &Path) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    // A bounded wake channel coalesces bursts; each wake refreshes all registered session titles.
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if !matches!(&event, Ok(e) if matches!(e.kind, notify::EventKind::Access(_))) {
            let _ = tx.try_send(());
        }
    })?;
    watcher.watch(projects, RecursiveMode::Recursive)?;
    let mut titles = Titles::default();
    loop {
        if let Err(error) = titles.refresh(broker, projects) {
            eprintln!("Claude title refresh: {error:#}");
        }
        tokio::select! {
            _ = rx.recv() => {},
            _ = broker.claude_wake.notified() => {},
        }
    }
}

pub async fn run(broker: Broker) {
    let home = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| crate::service::home().ok().map(|h| h.join(".claude")));
    let Some(home) = home else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if let Err(error) = observe(&broker, &home.join("projects")).await {
            eprintln!("Claude title observer: {error:#}");
        }
        // Only retry an unavailable watcher; healthy title discovery is filesystem-event driven.
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

//! Durable lifecycle authority. Callers hold the broker DB administration mutex.
//! Filesystem intents reserve the name until their final fsync and SQL commit.
use crate::{
    board::{
        self, FileIdentity, LastUpdatedSource, Lifecycle, LifecycleState, Name, Snapshot, Summary,
    },
    db::Db,
};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, path::Path};

pub(crate) const RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;

pub(crate) fn initialize(db: &Db) -> Result<()> {
    let synchronous: i64 = db.conn.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
    let journal: String = db.conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
    ensure!(
        synchronous >= 2 && matches!(journal.as_str(), "delete" | "truncate" | "persist" | "wal"),
        "Durable board lifecycle requires a persistent SQLite journal with FULL synchronization"
    );
    Ok(())
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    archived_at: Option<i64>,
    expires_at: Option<i64>,
    archived_from: Option<String>,
    original: Option<FileIdentity>,
    original_fingerprint: Option<board::lifecycle_files::Fingerprint>,
    last_updated: Option<i64>,
    #[serde(default)]
    last_updated_source: LastUpdatedSource,
    restore_from: Option<String>,
    temporary: Option<String>,
    staged: Option<FileIdentity>,
    error: Option<String>,
}

#[derive(Clone)]
struct Record {
    name: Name,
    incarnation: String,
    generation: String,
    phase: LifecycleState,
    archive: Option<String>,
    metadata: Metadata,
}

fn diagnostic(error: &anyhow::Error) -> String {
    let mut text = format!("{error:#}");
    if text.len() > 512 {
        let mut end = 512;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

fn uuid(value: &str) -> Result<()> {
    ensure!(
        uuid::Uuid::parse_str(value)?.to_string() == value,
        "Invalid board lifecycle identity"
    );
    Ok(())
}
fn hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|v| v.is_ascii_hexdigit())
}
fn phase(value: &str) -> Result<LifecycleState> {
    Ok(match value {
        "active" => LifecycleState::Active,
        "archived" => LifecycleState::Archived,
        "restoring" => LifecycleState::Restoring,
        "purging" => LifecycleState::Purging,
        _ => bail!("Invalid persisted board lifecycle phase"),
    })
}
fn phase_label(value: LifecycleState) -> &'static str {
    match value {
        LifecycleState::Active => "active",
        LifecycleState::Archived => "archived",
        LifecycleState::Restoring => "restoring",
        LifecycleState::Purging => "purging",
    }
}
impl Record {
    fn active(name: Name) -> Self {
        Self {
            name,
            incarnation: uuid::Uuid::new_v4().to_string(),
            generation: uuid::Uuid::new_v4().to_string(),
            phase: LifecycleState::Active,
            archive: None,
            metadata: Metadata::default(),
        }
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.name.is_main(),
            "Main cannot have archive lifecycle metadata"
        );
        uuid(&self.incarnation)?;
        uuid(&self.generation)?;
        ensure!(
            self.metadata.error.as_ref().is_none_or(|v| v.len() <= 512),
            "Oversized cleanup diagnostic"
        );
        if self.phase == LifecycleState::Active {
            ensure!(
                self.archive.is_none() && self.metadata == Metadata::default(),
                "Invalid active board archive metadata"
            );
        } else {
            let at = self.metadata.archived_at.context("Missing archive time")?;
            let deadline = at
                .checked_add(RETENTION_MS)
                .context("Archive deadline overflow")?;
            ensure!(
                at >= 0 && Some(deadline) == self.metadata.expires_at,
                "Invalid fixed archive deadline"
            );
            ensure!(
                self.archive
                    .as_ref()
                    .is_some_and(|v| v.len() <= board::MAX_BOARD_BYTES)
                    && self.metadata.original.is_some()
                    && self.metadata.original_fingerprint.is_some(),
                "Invalid immutable archive record"
            );
            ensure!(
                self.metadata.archived_from.as_deref().is_some_and(hash),
                "Invalid archived revision"
            );
            ensure!(
                self.metadata.last_updated.is_none_or(|v| v >= 0),
                "Invalid archived content timestamp"
            );
            if self.phase == LifecycleState::Restoring {
                let temp = self
                    .metadata
                    .temporary
                    .as_deref()
                    .context("Missing restore intent")?;
                let id = temp
                    .strip_prefix(".restore-")
                    .and_then(|v| v.strip_suffix(".tmp"))
                    .context("Invalid restore intent name")?;
                uuid(id)?;
                ensure!(
                    self.metadata.restore_from.as_deref().is_some_and(hash),
                    "Invalid restore revision"
                );
            } else {
                ensure!(
                    self.metadata.temporary.is_none()
                        && self.metadata.staged.is_none()
                        && self.metadata.restore_from.is_none(),
                    "Unexpected restore intent"
                );
            }
        }
        Ok(())
    }
    fn fence(&self, mut board: Snapshot) -> Snapshot {
        let mut digest = Sha256::new();
        for part in [
            "agent-bridge-board-lifecycle",
            &self.incarnation,
            &self.generation,
            &board.revision,
        ] {
            digest.update((part.len() as u64).to_be_bytes());
            digest.update(part.as_bytes());
        }
        board.revision = digest
            .finalize()
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect();
        board.lifecycle = Lifecycle {
            incarnation: Some(self.incarnation.clone()),
            lifecycle_generation: Some(self.generation.clone()),
            state: self.phase,
            archived_at: self.metadata.archived_at,
            expires_at: self.metadata.expires_at,
        };
        board
    }
    fn archived_snapshot(&self, state: &Path) -> Result<Snapshot> {
        let markdown = self.archive.clone().context("Board has no archive")?;
        let mut raw = board::snapshot(state, &self.name, markdown, true);
        raw.last_updated = self.metadata.last_updated;
        raw.last_updated_source = self.metadata.last_updated_source;
        raw.file_identity = self.metadata.original;
        Ok(self.fence(raw))
    }
}

fn load(db: &Db, name: &Name) -> Result<Option<Record>> {
    let row: Option<(String,String,String,Option<String>,String)> = db.conn.query_row(
        "SELECT incarnation,generation,phase,archive,metadata FROM board_lifecycle WHERE name=?", [name.as_str()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))) .optional()?;
    row.map(|(incarnation, generation, state, archive, metadata)| {
        let record = Record {
            name: name.clone(),
            incarnation,
            generation,
            phase: phase(&state)?,
            archive,
            metadata: serde_json::from_str(&metadata)
                .context("Invalid persisted board lifecycle metadata")?,
        };
        record.validate()?;
        Ok(record)
    })
    .transpose()
}
fn save(db: &Db, record: &Record) -> Result<()> {
    record.validate()?;
    db.conn.execute("INSERT INTO board_lifecycle(name,incarnation,generation,phase,archive,metadata) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(name) DO UPDATE SET incarnation=excluded.incarnation,generation=excluded.generation,phase=excluded.phase,archive=excluded.archive,metadata=excluded.metadata",rusqlite::params![record.name.as_str(),record.incarnation,record.generation,phase_label(record.phase),record.archive,serde_json::to_string(&record.metadata)?])?;
    Ok(())
}
fn catalog_names(db: &Db) -> Result<BTreeSet<Name>> {
    let mut stmt = db
        .conn
        .prepare("SELECT name FROM board_lifecycle ORDER BY name LIMIT 257")?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        rows.len() < board::MAX_BOARDS,
        "Board lifecycle catalog exceeds bound"
    );
    rows.into_iter().map(|name| name.parse()).collect()
}
fn capacity(db: &Db, state: &Path, name: &Name) -> Result<()> {
    let mut names = board::names(state)?;
    names.extend(catalog_names(db)?);
    names.insert(name.clone());
    ensure!(
        names.len() <= board::MAX_BOARDS,
        "At most 256 active or archived boards may exist"
    );
    Ok(())
}

pub(crate) fn read_active(db: &Db, state: &Path, name: &Name) -> Result<Snapshot> {
    if name.is_main() {
        return crate::board_metadata::enrich(db, state, board::read_named(state, name)?);
    }
    let record = match load(db, name)? {
        Some(record) => record,
        None => {
            board::read_named(state, name)?;
            capacity(db, state, name)?;
            let record = Record::active(name.clone());
            save(db, &record)?;
            record
        }
    };
    ensure!(
        record.phase == LifecycleState::Active,
        "Board {name} is {}; inspect archived boards or restore it explicitly",
        phase_label(record.phase)
    );
    crate::board_metadata::enrich(db, state, record.fence(board::read_named(state, name)?))
}

pub(crate) fn enrich_written(db: &Db, state: &Path, raw: Snapshot) -> Result<Snapshot> {
    let name: Name = raw.name.parse()?;
    let board = if name.is_main() {
        raw
    } else {
        let record = load(db, &name)?.context("Missing active board lifecycle identity")?;
        ensure!(
            record.phase == LifecycleState::Active,
            "Board is no longer active"
        );
        record.fence(raw)
    };
    crate::board_metadata::enrich(db, state, board)
}

pub(crate) fn list_active(db: &Db, state: &Path) -> Result<Vec<Summary>> {
    let mut result = Vec::new();
    for name in board::names(state)? {
        if load(db, &name)?.is_some_and(|v| v.phase != LifecycleState::Active) {
            continue;
        }
        let board = read_active(db, state, &name)?;
        result.push(Summary {
            name,
            exists: board.exists,
            revision: board.revision,
            last_updated: board.last_updated,
            last_updated_source: board.last_updated_source,
            lifecycle: board.lifecycle,
        });
    }
    Ok(result)
}

pub(crate) fn create(db: &Db, state: &Path, name: &Name) -> Result<Snapshot> {
    if name.is_main() {
        return crate::board_metadata::enrich(db, state, board::create(state, name)?);
    }
    if let Some(record) = load(db, name)? {
        ensure!(
            record.phase == LifecycleState::Active,
            "Archived-name conflict: {name} is reserved until restore or permanent cleanup"
        );
    }
    ensure!(
        board::lifecycle_files::target(state, name)?.is_none(),
        "Board {name} already exists"
    );
    capacity(db, state, name)?;
    // Reserve the new identity before creating the file. A crash cannot rebind
    // a prior incarnation to the newly created empty file.
    save(db, &Record::active(name.clone()))?;
    board::create(state, name)?;
    read_active(db, state, name)
}

fn observe_expiry(db: &Db, record: &mut Record, now: i64) -> Result<()> {
    if record.phase == LifecycleState::Archived
        && now >= record.metadata.expires_at.context("Missing expiry")?
    {
        record.phase = LifecycleState::Purging;
        save(db, record)?;
    }
    Ok(())
}
pub(crate) fn inspect(db: &Db, state: &Path, name: &Name, now: i64) -> Result<Snapshot> {
    if let Some(mut record) = load(db, name)?
        && record.phase != LifecycleState::Active
    {
        observe_expiry(db, &mut record, now)?;
        return record.archived_snapshot(state);
    }
    read_active(db, state, name)
}
pub(crate) fn archived(db: &Db, state: &Path, now: i64) -> Result<Vec<Summary>> {
    let mut result = Vec::new();
    for name in catalog_names(db)? {
        let mut record =
            load(db, &name)?.context("Archive disappeared during serialized listing")?;
        if record.phase == LifecycleState::Active {
            continue;
        }
        observe_expiry(db, &mut record, now)?;
        let board = record.archived_snapshot(state)?;
        result.push(Summary {
            name,
            exists: true,
            revision: board.revision,
            last_updated: board.last_updated,
            last_updated_source: board.last_updated_source,
            lifecycle: board.lifecycle,
        });
    }
    Ok(result)
}
pub(crate) fn archive(
    db: &Db,
    state: &Path,
    name: &Name,
    expected: &str,
    now: i64,
) -> Result<Snapshot> {
    ensure!(
        !name.is_main(),
        "Main is always available and cannot be archived"
    );
    ensure!(
        hash(expected) && now >= 0,
        "Invalid archive revision or UTC time"
    );
    if let Some(mut record) = load(db, name)?
        && record.phase != LifecycleState::Active
    {
        observe_expiry(db, &mut record, now)?;
        let current = record.archived_snapshot(state)?;
        if record.metadata.archived_from.as_deref() != Some(expected)
            && current.revision != expected
        {
            return Err(board::Conflict { current }.into());
        }
        return Ok(current);
    }
    let current = read_active(db, state, name)?;
    if current.revision != expected {
        return Err(board::Conflict { current }.into());
    }
    let mut record = load(db, name)?.context("Missing active lifecycle identity")?;
    let original = current.file_identity.context(
        "Archive requires filesystem birth-time identity; refusing unsafe inode-only fallback",
    )?;
    let original_fingerprint =
        board::lifecycle_files::capture_original(state, name, original, &current.markdown)?;
    record.phase = LifecycleState::Archived;
    record.generation = uuid::Uuid::new_v4().to_string();
    record.archive = Some(current.markdown);
    record.metadata = Metadata {
        archived_at: Some(now),
        expires_at: Some(
            now.checked_add(RETENTION_MS)
                .context("Archive deadline overflow")?,
        ),
        archived_from: Some(current.revision),
        original: current.file_identity,
        original_fingerprint: Some(original_fingerprint),
        last_updated: current.last_updated,
        last_updated_source: current.last_updated_source,
        ..Default::default()
    };
    save(db, &record)?;
    record.archived_snapshot(state)
}

fn finish_restore(db: &Db, state: &Path, record: &mut Record) -> Result<Snapshot> {
    let temporary = record
        .metadata
        .temporary
        .clone()
        .context("Missing restore staging name")?;
    if record.metadata.staged.is_none() {
        record.metadata.staged = Some(board::lifecycle_files::stage_empty(state, &temporary)?);
        save(db, record)?;
    }
    let staged = record
        .metadata
        .staged
        .context("Missing durable staging identity")?;
    let markdown = record
        .archive
        .as_deref()
        .context("Missing immutable archive")?;
    if board::lifecycle_files::target(state, &record.name)? != Some(staged) {
        board::lifecycle_files::write_staged(state, &temporary, staged, markdown)?;
    }
    board::lifecycle_files::install(
        state,
        &record.name,
        &temporary,
        staged,
        record
            .metadata
            .original
            .context("Missing archived original identity")?,
        record
            .metadata
            .original_fingerprint
            .context("Missing archived original fingerprint")?,
        markdown,
    )?;
    let time = record.metadata.last_updated;
    let source = record.metadata.last_updated_source;
    let tx = db.conn.unchecked_transaction()?;
    let mut active = record.clone();
    active.phase = LifecycleState::Active;
    active.archive = None;
    active.metadata = Metadata::default();
    save(db, &active)?;
    let mut restored = active.fence(board::read_named(state, &record.name)?);
    restored.last_updated = time;
    restored.last_updated_source = source;
    let restored = crate::board_metadata::enrich(db, state, restored)?;
    tx.commit()?;
    Ok(restored)
}

fn begin_restore(db: &Db, state: &Path, name: &Name, expected: &str, now: i64) -> Result<Record> {
    ensure!(hash(expected), "Invalid restore revision");
    let mut record = load(db, name)?
        .context("Archived board not found; it may have been permanently deleted")?;
    observe_expiry(db, &mut record, now)?;
    ensure!(
        record.phase != LifecycleState::Purging,
        "Archive expired; restoration is no longer available"
    );
    if record.phase == LifecycleState::Restoring {
        if record.metadata.restore_from.as_deref() != Some(expected) {
            return Err(board::Conflict {
                current: record.archived_snapshot(state)?,
            }
            .into());
        }
    } else {
        ensure!(
            record.phase == LifecycleState::Archived,
            "Board is already active"
        );
        let current = record.archived_snapshot(state)?;
        if current.revision != expected {
            return Err(board::Conflict { current }.into());
        }
        board::lifecycle_files::validate_original(
            state,
            name,
            record
                .metadata
                .original
                .context("Missing archived original identity")?,
            record
                .metadata
                .original_fingerprint
                .context("Missing archived original fingerprint")?,
            record
                .archive
                .as_deref()
                .context("Missing immutable archive")?,
        )?;
        record.phase = LifecycleState::Restoring;
        record.generation = uuid::Uuid::new_v4().to_string();
        record.metadata.restore_from = Some(expected.into());
        record.metadata.temporary = Some(format!(".restore-{}.tmp", uuid::Uuid::new_v4()));
        record.metadata.error = None;
        save(db, &record)?;
    }
    Ok(record)
}

pub(crate) fn restore(
    db: &Db,
    state: &Path,
    name: &Name,
    expected: &str,
    now: i64,
) -> Result<Snapshot> {
    let mut record = begin_restore(db, state, name, expected, now)?;
    match finish_restore(db, state, &mut record) {
        Ok(board) => Ok(board),
        Err(error) => {
            record.metadata.error = Some(diagnostic(&error));
            save(db, &record)?;
            Err(error)
        }
    }
}

pub(crate) fn cleanup(db: &Db, state: &Path, now: i64) -> Result<Vec<String>> {
    let mut errors = Vec::new();
    for name in catalog_names(db)? {
        let mut record = load(db, &name)?.context("Missing lifecycle record")?;
        observe_expiry(db, &mut record, now)?;
        let result = match record.phase {
            LifecycleState::Restoring => finish_restore(db, state, &mut record).map(|_| ()),
            LifecycleState::Purging => (|| -> Result<()> {
                board::lifecycle_files::purge(
                    state,
                    &name,
                    record
                        .metadata
                        .original
                        .context("Missing archive identity")?,
                    record
                        .metadata
                        .original_fingerprint
                        .context("Missing archive fingerprint")?,
                    record
                        .archive
                        .as_deref()
                        .context("Missing immutable archive")?,
                )?;
                let tx = db.conn.unchecked_transaction()?;
                let removed=db.conn.execute("DELETE FROM board_lifecycle WHERE name=? AND incarnation=? AND generation=? AND phase='purging'",rusqlite::params![name.as_str(),record.incarnation,record.generation])?;
                ensure!(
                    removed == 1,
                    "Archive cleanup identity changed; retaining deletion intent"
                );
                crate::board_metadata::forget(db, &name)?;
                tx.commit()?;
                Ok(())
            })(),
            _ => continue,
        };
        if let Err(error) = result {
            let detail = diagnostic(&error);
            record.metadata.error = Some(detail.clone());
            save(db, &record)?;
            errors.push(format!("{name}: {detail}"));
        }
    }
    Ok(errors)
}

pub(crate) fn diagnostics(db: &Db) -> Result<serde_json::Value> {
    let mut records = Vec::new();
    for name in catalog_names(db)? {
        let record = load(db, &name)?.context("Missing lifecycle record")?;
        if record.phase != LifecycleState::Active {
            records.push(serde_json::json!({"board":name,"state":record.phase,"expires_at":record.metadata.expires_at,"last_error":record.metadata.error}));
        }
    }
    Ok(serde_json::json!({"retention_ms":RETENTION_MS,"archives":records}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };

    fn setup() -> (tempfile::TempDir, Db, Name) {
        let state = tempfile::tempdir().unwrap();
        fs::set_permissions(state.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let db = Db::open(&state.path().join("test.sqlite")).unwrap();
        initialize(&db).unwrap();
        let name: Name = "project".parse().unwrap();
        create(&db, state.path(), &name).unwrap();
        (state, db, name)
    }
    fn content(db: &Db, state: &Path, name: &Name, text: &str) -> Snapshot {
        let raw = board::read_named(state, name).unwrap();
        board::update_named(
            state,
            name,
            &raw.revision,
            &[board::Edit::Set {
                markdown: text.into(),
            }],
        )
        .unwrap();
        read_active(db, state, name).unwrap()
    }
    #[test]
    fn immutable_archive_repeat_restore_and_generation_preserve_bytes_and_time() {
        let (state, db, name) = setup();
        let before = content(&db, state.path(), &name, "exact\n\n雪\t\0end");
        let saved = archive(&db, state.path(), &name, &before.revision, 1000).unwrap();
        assert_eq!(saved.markdown, before.markdown);
        assert!(read_active(&db, state.path(), &name).is_err());
        assert!(
            !list_active(&db, state.path())
                .unwrap()
                .iter()
                .any(|v| v.name == name)
        );
        assert!(
            create(&db, state.path(), &name)
                .unwrap_err()
                .to_string()
                .contains("Archived-name")
        );
        assert_eq!(
            archive(&db, state.path(), &name, &before.revision, 9000)
                .unwrap()
                .lifecycle
                .archived_at,
            Some(1000)
        );
        fs::write(
            &before.path,
            "outside edit cannot change immutable snapshot",
        )
        .unwrap();
        assert_eq!(
            inspect(&db, state.path(), &name, 1001).unwrap().markdown,
            before.markdown
        );
        assert!(restore(&db, state.path(), &name, &saved.revision, 1002).is_err());
        assert_eq!(
            fs::read_to_string(&before.path).unwrap(),
            "outside edit cannot change immutable snapshot"
        );
        // The owner explicitly removes the conflicting path; restoration may
        // then recreate it from the still-retained immutable archive.
        fs::remove_file(&before.path).unwrap();
        let restored = restore(&db, state.path(), &name, &saved.revision, 1002).unwrap();
        assert_eq!(restored.markdown, before.markdown);
        assert_eq!(restored.last_updated, before.last_updated);
        assert_eq!(restored.lifecycle.incarnation, before.lifecycle.incarnation);
        assert_ne!(
            restored.lifecycle.lifecycle_generation,
            before.lifecycle.lifecycle_generation
        );
        assert_ne!(restored.revision, before.revision);
        assert!(archived(&db, state.path(), 1003).unwrap().is_empty());
        let again = archive(&db, state.path(), &name, &restored.revision, 5000).unwrap();
        assert_eq!(again.lifecycle.archived_at, Some(5000));
        assert_eq!(again.lifecycle.expires_at, Some(5000 + RETENTION_MS));
    }
    #[test]
    fn fixed_deadline_restart_cutoff_and_clock_rollback_are_irreversible() {
        let (state, db, name) = setup();
        let before = content(&db, state.path(), &name, "retained");
        let saved = archive(&db, state.path(), &name, &before.revision, 1000).unwrap();
        drop(db);
        let db = Db::open(&state.path().join("test.sqlite")).unwrap();
        assert!(cleanup(&db, state.path(), 999).unwrap().is_empty());
        assert!(before.path.exists());
        assert_eq!(
            inspect(&db, state.path(), &name, 1001)
                .unwrap()
                .lifecycle
                .expires_at,
            Some(1000 + RETENTION_MS)
        );
        assert_eq!(
            inspect(&db, state.path(), &name, 999 + RETENTION_MS)
                .unwrap()
                .lifecycle
                .state,
            LifecycleState::Archived
        );
        assert!(
            restore(
                &db,
                state.path(),
                &name,
                &saved.revision,
                1000 + RETENTION_MS
            )
            .unwrap_err()
            .to_string()
            .contains("expired")
        );
        assert!(
            before.path.exists(),
            "expiry rejects restore before physical cleanup"
        );
        assert!(
            restore(&db, state.path(), &name, &saved.revision, 0).is_err(),
            "rollback cannot undo observed expiry"
        );
        assert!(
            cleanup(&db, state.path(), 1001 + RETENTION_MS)
                .unwrap()
                .is_empty()
        );
        assert!(!before.path.exists());
        assert!(load(&db, &name).unwrap().is_none());
        let recreated = create(&db, state.path(), &name).unwrap();
        assert_ne!(
            recreated.lifecycle.incarnation,
            before.lifecycle.incarnation
        );
        let recreated = content(&db, state.path(), &name, "retained");
        assert_ne!(
            recreated.revision, before.revision,
            "identical bytes must not revive an old revision"
        );
    }
    #[test]
    fn restore_before_cutoff_missing_file_and_main_age_protection() {
        let (state, db, name) = setup();
        let before = content(&db, state.path(), &name, "explicit restore");
        let saved = archive(&db, state.path(), &name, &before.revision, 10).unwrap();
        fs::remove_file(&before.path).unwrap();
        let restored =
            restore(&db, state.path(), &name, &saved.revision, 9 + RETENTION_MS).unwrap();
        assert_eq!(restored.markdown, before.markdown);
        assert!(cleanup(&db, state.path(), i64::MAX).unwrap().is_empty());
        assert_eq!(
            read_active(&db, state.path(), &name).unwrap().markdown,
            before.markdown
        );
        let main = read_active(&db, state.path(), &Name::default()).unwrap();
        assert!(
            archive(&db, state.path(), &Name::default(), &main.revision, 10)
                .unwrap_err()
                .to_string()
                .contains("Main")
        );
        assert!(
            list_active(&db, state.path())
                .unwrap()
                .iter()
                .any(|v| v.name.is_main())
        );
    }
    #[test]
    fn purge_replacement_symlink_and_hardlink_errors_retain_retryable_archive() {
        let (state, db, name) = setup();
        let before = content(&db, state.path(), &name, "original");
        archive(&db, state.path(), &name, &before.revision, 10).unwrap();
        let held = state.path().join("held-original");
        fs::rename(&before.path, &held).unwrap();
        fs::write(&before.path, "unrelated replacement").unwrap();
        fs::set_permissions(&before.path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            cleanup(&db, state.path(), 10 + RETENTION_MS).unwrap().len(),
            1
        );
        assert_eq!(
            fs::read_to_string(&before.path).unwrap(),
            "unrelated replacement"
        );
        fs::remove_file(&before.path).unwrap();
        symlink(&held, &before.path).unwrap();
        assert_eq!(
            cleanup(&db, state.path(), 10 + RETENTION_MS).unwrap().len(),
            1
        );
        fs::remove_file(&before.path).unwrap();
        fs::hard_link(&held, &before.path).unwrap();
        assert_eq!(
            cleanup(&db, state.path(), 10 + RETENTION_MS).unwrap().len(),
            1
        );
        fs::remove_file(&before.path).unwrap();
        // Leave the relocated original alone: its link/rename metadata changed.
        // Once the conflicting managed path is removed, cleanup retries absence.
        assert!(
            cleanup(&db, state.path(), 10 + RETENTION_MS)
                .unwrap()
                .is_empty()
        );
        assert!(load(&db, &name).unwrap().is_none());
        assert_eq!(fs::read_to_string(held).unwrap(), "original");
    }
    #[test]
    fn failed_final_purge_transaction_recovers_absent_file_without_losing_record() {
        let (state, db, name) = setup();
        let before = content(&db, state.path(), &name, "durable");
        archive(&db, state.path(), &name, &before.revision, 10).unwrap();
        db.conn.execute_batch("CREATE TEMP TRIGGER fail_purge BEFORE DELETE ON board_lifecycle BEGIN SELECT RAISE(FAIL,'injected purge metadata failure');END;").unwrap();
        assert_eq!(
            cleanup(&db, state.path(), 10 + RETENTION_MS).unwrap().len(),
            1
        );
        assert!(!before.path.exists());
        assert_eq!(
            load(&db, &name).unwrap().unwrap().archive.as_deref(),
            Some("durable")
        );
        assert!(
            diagnostics(&db).unwrap()["archives"][0]["last_error"]
                .as_str()
                .unwrap()
                .contains("injected")
        );
        drop(db);
        let db = Db::open(&state.path().join("test.sqlite")).unwrap();
        assert!(
            cleanup(&db, state.path(), 10 + RETENTION_MS)
                .unwrap()
                .is_empty()
        );
        assert!(load(&db, &name).unwrap().is_none());
        let timestamps: Option<String> = db
            .conn
            .query_row(
                "SELECT value FROM settings WHERE key='board_timestamps'",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert!(!timestamps.unwrap_or_default().contains("project"));
    }

    #[test]
    fn restore_staging_faults_preserve_foreign_files_and_retry_the_same_intent() {
        for recorded in [false, true] {
            let (state, db, name) = setup();
            let before = content(&db, state.path(), &name, "immutable original");
            let saved = archive(&db, state.path(), &name, &before.revision, 1000).unwrap();
            let mut record =
                begin_restore(&db, state.path(), &name, &saved.revision, 1001).unwrap();
            let temporary = record.metadata.temporary.clone().unwrap();
            let target = state.path().join("boards").join(&temporary);
            let held = state.path().join("held-stage");
            if recorded {
                record.metadata.staged =
                    Some(board::lifecycle_files::stage_empty(state.path(), &temporary).unwrap());
                save(&db, &record).unwrap();
                fs::rename(&target, &held).unwrap();
            }
            fs::write(&target, "foreign staging bytes").unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
            assert!(restore(&db, state.path(), &name, &saved.revision, 1002).is_err());
            assert_eq!(
                fs::read_to_string(&target).unwrap(),
                "foreign staging bytes"
            );
            assert_eq!(
                inspect(&db, state.path(), &name, 1003).unwrap().markdown,
                before.markdown
            );
            let pending = load(&db, &name).unwrap().unwrap();
            assert_eq!(pending.phase, LifecycleState::Restoring);
            assert!(pending.metadata.error.is_some());
            assert_eq!(pending.generation, record.generation);
            fs::remove_file(&target).unwrap();
            if recorded {
                fs::rename(held, &target).unwrap();
            }
            let restored = restore(&db, state.path(), &name, &saved.revision, 1004).unwrap();
            assert_eq!(restored.markdown, before.markdown);
            assert_eq!(
                restored.lifecycle.lifecycle_generation.as_deref(),
                Some(record.generation.as_str())
            );
        }
    }

    #[test]
    fn catalog_capacity_deadline_overflow_and_archive_storage_are_bounded() {
        let (state, db, name) = setup();
        let before = read_active(&db, state.path(), &name).unwrap();
        assert!(archive(&db, state.path(), &name, &before.revision, i64::MAX).is_err());
        assert_eq!(
            read_active(&db, state.path(), &name).unwrap().revision,
            before.revision
        );
        let transaction = db.conn.unchecked_transaction().unwrap();
        for index in 0..board::MAX_BOARDS - 2 {
            save(
                &db,
                &Record::active(format!("reserved-{index}").parse().unwrap()),
            )
            .unwrap();
        }
        transaction.commit().unwrap();
        assert_eq!(catalog_names(&db).unwrap().len(), board::MAX_BOARDS - 1);
        let overflow: Name = "overflow".parse().unwrap();
        assert!(
            create(&db, state.path(), &overflow)
                .unwrap_err()
                .to_string()
                .contains("256")
        );
        assert!(!state.path().join("boards/overflow.md").exists());
        assert_eq!(catalog_names(&db).unwrap().len(), board::MAX_BOARDS - 1);
        assert!(
            db.conn
                .execute(
                    "UPDATE board_lifecycle SET archive=? WHERE name='project'",
                    ["x".repeat(board::MAX_BOARD_BYTES + 1)]
                )
                .is_err()
        );
    }

    #[test]
    fn abrupt_restore_process_exits_recover_each_durable_boundary() {
        for boundary in 0..=8 {
            let (state, db, name) = setup();
            let original = content(&db, state.path(), &name, "exact archive\n雪\tcontent\n");
            archive(&db, state.path(), &name, &original.revision, 1000).unwrap();
            drop(db);
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "board_lifecycle::tests::restore_crash_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("LANG", "C.UTF-8")
                .env("AGB_ARCHIVE_TEST_STATE", state.path())
                .env("AGB_ARCHIVE_TEST_BOUNDARY", boundary.to_string())
                .output()
                .unwrap();
            assert_eq!(
                result.status.code(),
                Some(73),
                "boundary {boundary}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            let db = Db::open(&state.path().join("test.sqlite")).unwrap();
            let interrupted = load(&db, &name).unwrap().unwrap();
            let generation = interrupted.generation.clone();
            assert!(matches!(
                interrupted.phase,
                LifecycleState::Restoring | LifecycleState::Active
            ));
            // A pre-expiry restore intent wins even when startup occurs later.
            assert!(
                cleanup(&db, state.path(), i64::MAX).unwrap().is_empty(),
                "boundary {boundary}"
            );
            let restored = read_active(&db, state.path(), &name).unwrap();
            assert_eq!(restored.markdown, original.markdown, "boundary {boundary}");
            assert_eq!(
                restored.last_updated, original.last_updated,
                "boundary {boundary}"
            );
            assert_eq!(
                restored.lifecycle.lifecycle_generation.as_deref(),
                Some(generation.as_str())
            );
            assert_ne!(restored.revision, original.revision);
            assert!(archived(&db, state.path(), i64::MAX).unwrap().is_empty());
            let files = fs::read_dir(state.path().join("boards"))
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect::<Vec<_>>();
            assert_eq!(
                files,
                vec![std::ffi::OsString::from("project.md")],
                "No abandoned staging content"
            );
        }
    }

    /// Invoked only by the parent test with its own private directory. exit()
    /// deliberately bypasses Rust destructors; no production failure switches.
    #[test]
    #[ignore]
    fn restore_crash_worker() {
        let Some(state) = std::env::var_os("AGB_ARCHIVE_TEST_STATE") else {
            return;
        };
        let state = std::path::PathBuf::from(state);
        let boundary: u8 = std::env::var("AGB_ARCHIVE_TEST_BOUNDARY")
            .unwrap()
            .parse()
            .unwrap();
        let db = Db::open(&state.join("test.sqlite")).unwrap();
        let name: Name = "project".parse().unwrap();
        let expected = inspect(&db, &state, &name, 1001).unwrap().revision;
        let mut record = begin_restore(&db, &state, &name, &expected, 1001).unwrap();
        if boundary == 0 {
            std::process::exit(73);
        }
        let temporary = record.metadata.temporary.clone().unwrap();
        let id = board::lifecycle_files::stage_empty(&state, &temporary).unwrap();
        if boundary == 1 {
            std::process::exit(73);
        }
        record.metadata.staged = Some(id);
        save(&db, &record).unwrap();
        if boundary == 2 {
            std::process::exit(73);
        }
        board::lifecycle_files::write_staged(&state, &temporary, id, "partial").unwrap();
        if boundary == 3 {
            std::process::exit(73);
        }
        let markdown = record.archive.as_deref().unwrap();
        board::lifecycle_files::write_staged(&state, &temporary, id, markdown).unwrap();
        if boundary == 4 {
            std::process::exit(73);
        }
        // Stop between rename and the production helper's directory fsync.
        fs::rename(
            state.join("boards").join(&temporary),
            state.join("boards/project.md"),
        )
        .unwrap();
        if boundary == 5 {
            std::process::exit(73);
        }
        board::lifecycle_files::install(
            &state,
            &name,
            &temporary,
            id,
            record.metadata.original.unwrap(),
            record.metadata.original_fingerprint.unwrap(),
            markdown,
        )
        .unwrap();
        if boundary == 6 {
            std::process::exit(73);
        }
        if boundary == 7 {
            db.conn.execute_batch("CREATE TEMP TRIGGER stop_restore BEFORE UPDATE ON board_lifecycle WHEN NEW.phase='active' BEGIN SELECT RAISE(FAIL,'interrupted active commit'); END;").unwrap();
            assert!(finish_restore(&db, &state, &mut record).is_err());
            std::process::exit(73);
        }
        finish_restore(&db, &state, &mut record).unwrap();
        std::process::exit(73);
    }
}

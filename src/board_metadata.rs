//! Durable revision-bound board metadata in the existing settings store.
use crate::{
    board::{self, LastUpdatedSource, Name, Snapshot},
    db::Db,
};
use anyhow::{Context, Result, ensure};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

const KEY: &str = "board_timestamps";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Recorded {
    revision: String,
    exists: bool,
    last_updated: Option<i64>,
}

pub(crate) fn enrich(db: &Db, state_dir: &Path, mut board: Snapshot) -> Result<Snapshot> {
    board.last_updated = recorded_time(
        db,
        state_dir,
        board.name.parse()?,
        &board.revision,
        board.exists,
        board.last_updated,
    )?;
    board.last_updated_source = source(board.last_updated);
    Ok(board)
}

fn source(time: Option<i64>) -> LastUpdatedSource {
    if time.is_some() {
        LastUpdatedSource::FileMtime
    } else {
        LastUpdatedSource::Unknown
    }
}

fn recorded_time(
    db: &Db,
    state_dir: &Path,
    name: Name,
    revision: &str,
    exists: bool,
    last_updated: Option<i64>,
) -> Result<Option<i64>> {
    let stored: Option<String> = db
        .conn
        .query_row("SELECT value FROM settings WHERE key=?", [KEY], |row| {
            row.get(0)
        })
        .optional()
        .context("Cannot read board timestamp metadata")?;
    let mut records: BTreeMap<Name, Recorded> = match stored {
        Some(value) => {
            serde_json::from_str(&value).context("Invalid stored board timestamp metadata")?
        }
        None => BTreeMap::new(),
    };
    ensure!(
        records.len() <= board::MAX_BOARDS,
        "Stored board timestamp metadata exceeds board limit"
    );
    for record in records.values() {
        ensure!(
            record.revision.len() == 64 && record.revision.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid stored board timestamp revision"
        );
        ensure!(
            record.last_updated.is_none_or(|time| time >= 0)
                && (record.exists || record.last_updated.is_none()),
            "Invalid stored board timestamp value"
        );
    }
    if let Some(record) = records.get(&name)
        && record.revision == revision
        && record.exists == exists
    {
        return Ok(record.last_updated);
    }
    if !records.contains_key(&name) && records.len() == board::MAX_BOARDS {
        let names = board::list(state_dir)?
            .into_iter()
            .map(|entry| entry.name)
            .collect::<std::collections::BTreeSet<_>>();
        records.retain(|name, _| names.contains(name));
    }
    records.insert(
        name,
        Recorded {
            revision: revision.to_owned(),
            exists,
            last_updated,
        },
    );
    ensure!(
        records.len() <= board::MAX_BOARDS,
        "Board timestamp metadata exceeds board limit"
    );
    db.conn.execute("INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", rusqlite::params![KEY, serde_json::to_string(&records)?]).context("Cannot persist board timestamp metadata")?;
    Ok(last_updated)
}

/// Remove metadata only after a lifecycle purge durably unlinked its file.
pub(crate) fn forget(db: &Db, name: &Name) -> Result<()> {
    let stored: Option<String> = db
        .conn
        .query_row("SELECT value FROM settings WHERE key=?", [KEY], |r| {
            r.get(0)
        })
        .optional()?;
    if let Some(stored) = stored {
        let mut records: BTreeMap<Name, Recorded> = serde_json::from_str(&stored)?;
        ensure!(
            records.len() <= board::MAX_BOARDS,
            "Board timestamp metadata exceeds board limit"
        );
        records.remove(name);
        db.conn.execute(
            "UPDATE settings SET value=? WHERE key=?",
            rusqlite::params![serde_json::to_string(&records)?, KEY],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::{self, File, Permissions},
        os::unix::fs::{MetadataExt, PermissionsExt},
        time::{Duration, UNIX_EPOCH},
    };

    fn state() -> tempfile::TempDir {
        let state = tempfile::tempdir().unwrap();
        fs::set_permissions(state.path(), Permissions::from_mode(0o700)).unwrap();
        state
    }
    fn at(board: &Snapshot, seconds: u64) {
        File::options()
            .write(true)
            .open(&board.path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(seconds))
            .unwrap();
    }
    fn read(db: &Db, path: &Path, name: &Name) -> Snapshot {
        enrich(db, path, board::read_named(path, name).unwrap()).unwrap()
    }
    #[test]
    fn timestamp_survives_touch_noop_conflict_restart_and_tracks_external_content() {
        let state = state();
        let db_path = state.path().join("db.sqlite");
        let db = Db::open(&db_path).unwrap();
        let main = Name::default();
        let empty = read(&db, state.path(), &main);
        assert_eq!(empty.last_updated, None);
        board::update(
            state.path(),
            &empty.revision,
            &[board::Edit::Set {
                markdown: String::new(),
            }],
        )
        .unwrap();
        assert!(!state.path().join("boards").exists());
        let first = board::update(
            state.path(),
            &empty.revision,
            &[board::Edit::Set {
                markdown: "one".into(),
            }],
        )
        .unwrap();
        at(&first, 1_000);
        let first = read(&db, state.path(), &main);
        assert_eq!(first.last_updated, Some(1_000_000));
        at(&first, 2_000);
        let inode = fs::metadata(&first.path).unwrap().ino();
        let noop = board::update(
            state.path(),
            &first.revision,
            &[board::Edit::Set {
                markdown: "one".into(),
            }],
        )
        .unwrap();
        assert_eq!(fs::metadata(&noop.path).unwrap().ino(), inode);
        assert_eq!(
            enrich(&db, state.path(), noop).unwrap().last_updated,
            first.last_updated
        );
        assert!(
            board::update(
                state.path(),
                &empty.revision,
                &[board::Edit::Append { text: "bad".into() }]
            )
            .is_err()
        );
        assert!(
            board::update(
                state.path(),
                &first.revision,
                &[board::Edit::Replace {
                    old: "absent".into(),
                    new: "bad".into()
                }]
            )
            .is_err()
        );
        drop(db);
        let db = Db::open(&db_path).unwrap();
        assert_eq!(
            read(&db, state.path(), &main).last_updated,
            first.last_updated
        );
        fs::write(&first.path, "external").unwrap();
        at(&first, 3_000);
        let external = read(&db, state.path(), &main);
        assert_eq!(external.last_updated, Some(3_000_000));
        assert_ne!(external.revision, first.revision);
        let second = board::update(
            state.path(),
            &external.revision,
            &[board::Edit::Set {
                markdown: "supported".into(),
            }],
        )
        .unwrap();
        assert_eq!(
            enrich(&db, state.path(), second.clone())
                .unwrap()
                .last_updated,
            second.last_updated
        );
        assert_ne!(second.last_updated, external.last_updated);
    }
    #[test]
    fn names_creation_unknown_and_invalid_records_are_explicit() {
        let state = state();
        let db = Db::open(&state.path().join("db.sqlite")).unwrap();
        let main = Name::default();
        assert_eq!(read(&db, state.path(), &main).last_updated, None);
        let created = board::create(state.path(), &main).unwrap();
        at(&created, 4_000);
        assert_eq!(read(&db, state.path(), &main).last_updated, Some(4_000_000));
        let alpha: Name = "alpha".parse().unwrap();
        let created = board::create(state.path(), &alpha).unwrap();
        at(&created, 5_000);
        assert_eq!(
            read(&db, state.path(), &alpha).last_updated,
            Some(5_000_000)
        );
        assert_eq!(read(&db, state.path(), &main).last_updated, Some(4_000_000));
        let future: Name = "future".parse().unwrap();
        let created = board::create(state.path(), &future).unwrap();
        at(&created, 32_503_680_000);
        let unknown = read(&db, state.path(), &future);
        assert_eq!(unknown.last_updated, None);
        assert_eq!(unknown.last_updated_source, LastUpdatedSource::Unknown);
        db.conn
            .execute("UPDATE settings SET value='invalid' WHERE key=?", [KEY])
            .unwrap();
        assert!(enrich(&db, state.path(), created).is_err());
    }
    #[test]
    fn observed_content_round_trip_records_the_restored_revision_time() {
        let state = state();
        let db = Db::open(&state.path().join("db.sqlite")).unwrap();
        let name = Name::default();
        let created = board::create(state.path(), &name).unwrap();
        fs::write(&created.path, "A").unwrap();
        at(&created, 1_000);
        let first = read(&db, state.path(), &name);
        fs::write(&created.path, "B").unwrap();
        at(&created, 2_000);
        let middle = read(&db, state.path(), &name);
        assert_ne!(first.revision, middle.revision);
        fs::write(&created.path, "A").unwrap();
        at(&created, 3_000);
        let restored = read(&db, state.path(), &name);
        assert_eq!(restored.revision, first.revision);
        assert_eq!(restored.last_updated, Some(3_000_000));
        assert_ne!(restored.last_updated, first.last_updated);
    }

    #[test]
    fn unknown_time_remains_unknown_for_the_same_revision() {
        let state = state();
        let db = Db::open(&state.path().join("db.sqlite")).unwrap();
        let name = Name::default();
        let created = board::create(state.path(), &name).unwrap();
        at(&created, 32_503_680_000);
        assert_eq!(read(&db, state.path(), &name).last_updated, None);
        at(&created, 1_000);
        assert_eq!(
            board::read_named(state.path(), &name).unwrap().last_updated,
            Some(1_000_000)
        );
        let cached = read(&db, state.path(), &name);
        assert_eq!(cached.last_updated, None);
        assert_eq!(cached.last_updated_source, LastUpdatedSource::Unknown);
    }

    fn stored_records(db: &Db) -> BTreeMap<Name, Recorded> {
        let json: String = db
            .conn
            .query_row("SELECT value FROM settings WHERE key=?", [KEY], |row| {
                row.get(0)
            })
            .unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn full_metadata_cache_prunes_deleted_names_without_evicting_current_boards() {
        let state = state();
        let db = Db::open(&state.path().join("db.sqlite")).unwrap();
        let main = Name::default();
        let created = board::create(state.path(), &main).unwrap();
        at(&created, 1_000);
        read(&db, state.path(), &main);
        let mut records = stored_records(&db);
        let revision = records.get(&main).unwrap().revision.clone();
        // Deleted names retain their most recent observation until capacity is
        // needed; no hundreds of board files are required to establish this state.
        for index in 0..board::MAX_BOARDS - 1 {
            records.insert(
                format!("deleted-{index}").parse().unwrap(),
                Recorded {
                    revision: revision.clone(),
                    exists: true,
                    last_updated: Some(2_000_000),
                },
            );
        }
        db.conn
            .execute(
                "UPDATE settings SET value=? WHERE key=?",
                rusqlite::params![serde_json::to_string(&records).unwrap(), KEY],
            )
            .unwrap();
        let alpha: Name = "alpha".parse().unwrap();
        let created = board::create(state.path(), &alpha).unwrap();
        at(&created, 3_000);
        assert_eq!(
            read(&db, state.path(), &alpha).last_updated,
            Some(3_000_000)
        );
        let records = stored_records(&db);
        assert_eq!(records.len(), 2);
        assert!(records.contains_key(&main));
        assert!(records.contains_key(&alpha));
        assert_eq!(read(&db, state.path(), &main).last_updated, Some(1_000_000));
    }

    #[test]
    fn malformed_and_oversized_persisted_metadata_fail_explicitly() {
        let state = state();
        let db = Db::open(&state.path().join("db.sqlite")).unwrap();
        let name = Name::default();
        let snapshot = read(&db, state.path(), &name);
        let revision = snapshot.revision.clone();
        let mut oversized = BTreeMap::new();
        for index in 0..=board::MAX_BOARDS {
            oversized.insert(
                format!("board-{index}"),
                serde_json::json!({
                    "revision": revision, "exists": true, "last_updated": 1_000,
                }),
            );
        }
        for invalid in [
            "invalid".to_owned(),
            serde_json::json!({"main":{"revision":"bad","exists":true,"last_updated":1000}}).to_string(),
            serde_json::json!({"main":{"revision":revision,"exists":true,"last_updated":-1}}).to_string(),
            serde_json::json!({"main":{"revision":revision,"exists":false,"last_updated":1000}}).to_string(),
            serde_json::json!({"main":{"revision":revision,"exists":true,"last_updated":1000,"extra":true}}).to_string(),
            serde_json::json!({"../invalid":{"revision":revision,"exists":true,"last_updated":1000}}).to_string(),
            serde_json::to_string(&oversized).unwrap(),
        ] {
            db.conn.execute("UPDATE settings SET value=? WHERE key=?", rusqlite::params![invalid, KEY]).unwrap();
            assert!(enrich(&db, state.path(), snapshot.clone()).is_err());
        }
    }
}

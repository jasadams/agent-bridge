//! A local Markdown document with revision-checked, atomic incremental edits.
//!
//! The broker serializes operations. Callers which edit the file outside the
//! broker must coordinate with active writers; filesystem editors do not share
//! the broker's transaction boundary.
use anyhow::{Context, Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::{CStr, CString},
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

pub const MAX_BOARD_BYTES: usize = 64 * 1024;
pub const MAX_EDITS: usize = 32;

const DIRECTORY: &CStr = c"boards";
pub const MAX_BOARDS: usize = 256;
const MAX_DIRECTORY_ENTRIES: usize = 1024;

/// A local board slug; validation is shared by every input boundary.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Name(String);
impl JsonSchema for Name {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "BoardName".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": "string", "pattern": "^[a-z0-9][a-z0-9_-]{0,63}$", "minLength": 1, "maxLength": 64 })
    }
}
impl Name {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn is_main(&self) -> bool {
        self.0 == "main"
    }
    fn document(&self) -> CString {
        CString::new(format!("{}.md", self.0)).expect("Validated slug")
    }
}
impl Default for Name {
    fn default() -> Self {
        Self("main".into())
    }
}
impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::str::FromStr for Name {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        ensure!(
            !value.is_empty()
                && value.len() <= 64
                && (value.as_bytes()[0].is_ascii_lowercase()
                    || value.as_bytes()[0].is_ascii_digit())
                && value.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'_'
                    || b == b'-'),
            "Invalid board name"
        );
        Ok(Self(value.into()))
    }
}
impl TryFrom<String> for Name {
    type Error = anyhow::Error;
    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}
impl From<Name> for String {
    fn from(value: Name) -> Self {
        value.0
    }
}

/// Provenance of the revision-bound UTC timestamp.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LastUpdatedSource {
    FileMtime,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Summary {
    pub name: Name,
    pub exists: bool,
    pub revision: String,
    #[serde(default)]
    pub last_updated: Option<i64>,
    #[serde(default)]
    pub last_updated_source: LastUpdatedSource,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub name: String,
    pub markdown: String,
    pub revision: String,
    pub path: PathBuf,
    pub exists: bool,
    #[serde(default)]
    pub last_updated: Option<i64>,
    #[serde(default)]
    pub last_updated_source: LastUpdatedSource,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Edit {
    /// Replace exactly one occurrence; missing or ambiguous text is a conflict.
    Replace {
        old: String,
        new: String,
    },
    Append {
        text: String,
    },
    Prepend {
        text: String,
    },
    Set {
        markdown: String,
    },
}

#[derive(Debug)]
pub struct Conflict {
    pub current: Snapshot,
}
impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Board changed; read the current revision and retry the intended edits"
        )
    }
}
impl std::error::Error for Conflict {}

fn snapshot(state_dir: &Path, name: &Name, markdown: String, exists: bool) -> Snapshot {
    Snapshot {
        name: name.to_string(),
        revision: revision_digest(name, &markdown)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        markdown,
        path: state_dir.join("boards").join(format!("{name}.md")),
        exists,
        last_updated: None,
        last_updated_source: LastUpdatedSource::Unknown,
    }
}

fn stable_metadata(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
        && before.mode() == after.mode()
        && before.uid() == after.uid()
        && before.nlink() == after.nlink()
}

fn assign_file_time(board: &mut Snapshot, metadata: &std::fs::Metadata) {
    use std::time::{SystemTime, UNIX_EPOCH};
    board.last_updated = metadata
        .modified()
        .ok()
        .filter(|time| *time <= SystemTime::now())
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_millis()).ok());
    board.last_updated_source = if board.last_updated.is_some() {
        LastUpdatedSource::FileMtime
    } else {
        LastUpdatedSource::Unknown
    };
}

fn revision_digest(name: &Name, markdown: &str) -> sha2::digest::Output<Sha256> {
    let mut digest = Sha256::new();
    if !name.is_main() {
        digest.update(b"agent-bridge-board\0");
        digest.update(name.as_str().as_bytes());
        digest.update(b"\0");
    }
    digest.update(markdown.as_bytes());
    digest.finalize()
}

fn root(state_dir: &Path) -> Result<File> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(state_dir)
        .context("Cannot open the private board state directory")?;
    validate_directory(&directory)?;
    Ok(directory)
}

fn validate_directory(directory: &File) -> Result<()> {
    let metadata = directory.metadata()?;
    ensure!(metadata.is_dir(), "Board directory must be a directory");
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o7777 == 0o700,
        "Board directories must be owned by this user with mode 0700"
    );
    Ok(())
}

fn open_at(directory: &File, name: &CStr, flags: libc::c_int) -> std::io::Result<File> {
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn board_directory(state: &File, create: bool) -> Result<Option<File>> {
    match open_at(state, DIRECTORY, libc::O_RDONLY | libc::O_DIRECTORY) {
        Ok(directory) => {
            validate_directory(&directory)?;
            Ok(Some(directory))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let result = unsafe { libc::mkdirat(state.as_raw_fd(), DIRECTORY.as_ptr(), 0o700) };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error).context("Cannot create private board directory");
                }
            }
            let directory = open_at(state, DIRECTORY, libc::O_RDONLY | libc::O_DIRECTORY)
                .context("Cannot open private board directory")?;
            validate_directory(&directory)?;
            // Persist the parent's new directory entry as well as the document
            // and boards directory synced by the subsequent atomic write.
            state
                .sync_all()
                .context("Cannot persist board directory creation")?;
            Ok(Some(directory))
        }
        Err(error) => Err(error).context("Board directory must not be a symbolic link"),
    }
}

fn read_document(state_dir: &Path, name: &Name, directory: &File) -> Result<Snapshot> {
    let mut file = match open_at(directory, &name.document(), libc::O_RDONLY) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(
                name.is_main(),
                "Board {name} not found; create it explicitly first"
            );
            return Ok(snapshot(state_dir, name, String::new(), false));
        }
        Err(error) => {
            return Err(error).context("Cannot open board document without following links");
        }
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o7777 == 0o600
            && metadata.nlink() == 1,
        "Board document must be a regular, single-link, user-owned file with mode 0600"
    );
    ensure!(
        metadata.len() <= MAX_BOARD_BYTES as u64,
        "Board exceeds 64 KiB"
    );
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_BOARD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_BOARD_BYTES, "Board exceeds 64 KiB");
    let markdown =
        String::from_utf8(bytes).context("Board document must contain UTF-8 Markdown")?;
    let after = file.metadata()?;
    ensure!(
        stable_metadata(&metadata, &after),
        "Board changed while reading; retry the read"
    );
    let mut board = snapshot(state_dir, name, markdown, true);
    assign_file_time(&mut board, &after);
    Ok(board)
}

/// Reading an absent board returns an empty document without creating a file.
pub fn read(state_dir: &Path) -> Result<Snapshot> {
    read_named(state_dir, &Name::default())
}
pub fn read_named(state_dir: &Path, name: &Name) -> Result<Snapshot> {
    let state = root(state_dir)?;
    let Some(directory) = board_directory(&state, false)? else {
        ensure!(
            name.is_main(),
            "Board {name} not found; create it explicitly first"
        );
        return Ok(snapshot(state_dir, name, String::new(), false));
    };
    read_document(state_dir, name, &directory)
}

/// Create without replacing any existing entry. Broker serialization protects transactions.
pub fn create(state_dir: &Path, name: &Name) -> Result<Snapshot> {
    let entries = list(state_dir)?;
    ensure!(
        entries
            .iter()
            .all(|entry| &entry.name != name || !entry.exists),
        "Board {name} already exists"
    );
    ensure!(
        entries.iter().any(|entry| &entry.name == name) || entries.len() < MAX_BOARDS,
        "At most 256 boards may exist"
    );
    let state = root(state_dir)?;
    let directory = board_directory(&state, true)?.context("Board directory was not created")?;
    let file = open_at(
        &directory,
        &name.document(),
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
    )
    .context("Cannot create board; it may already exist")?;
    file.sync_all().context("Cannot persist new board")?;
    directory
        .sync_all()
        .context("Cannot persist new board directory entry")?;
    let mut board = snapshot(state_dir, name, String::new(), true);
    assign_file_time(&mut board, &file.metadata()?);
    Ok(board)
}

#[cfg(target_os = "linux")]
unsafe fn directory_errno() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}
#[cfg(target_os = "macos")]
unsafe fn directory_errno() -> *mut libc::c_int {
    unsafe { libc::__error() }
}

/// Enumerate from the validated directory descriptor, with explicit work/output limits.
pub fn list(state_dir: &Path) -> Result<Vec<Summary>> {
    let state = root(state_dir)?;
    let mut names = std::collections::BTreeSet::from([Name::default()]);
    if let Some(directory) = board_directory(&state, false)? {
        let fd = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        ensure!(fd >= 0, "Cannot duplicate board directory descriptor");
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            unsafe {
                libc::close(fd);
            }
            anyhow::bail!("Cannot enumerate boards");
        }
        let result = (|| -> Result<()> {
            let mut count = 0;
            loop {
                // POSIX readdir signals end or failure using errno.
                unsafe {
                    *directory_errno() = 0;
                }
                let entry = unsafe { libc::readdir(stream) };
                if entry.is_null() {
                    let code = unsafe { *directory_errno() };
                    ensure!(
                        code == 0,
                        "Cannot enumerate boards: {}",
                        std::io::Error::from_raw_os_error(code)
                    );
                    break;
                }
                count += 1;
                ensure!(
                    count <= MAX_DIRECTORY_ENTRIES,
                    "Board directory exceeds 1024 entries"
                );
                let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
                let Ok(filename) = std::str::from_utf8(bytes) else {
                    continue;
                };
                let Some(slug) = filename.strip_suffix(".md") else {
                    continue;
                };
                let Ok(name) = slug.parse::<Name>() else {
                    continue;
                };
                names.insert(name);
                ensure!(
                    names.len() <= MAX_BOARDS,
                    "Board listing exceeds 256 boards"
                );
            }
            Ok(())
        })();
        unsafe {
            libc::closedir(stream);
        }
        result?;
    }
    names
        .into_iter()
        .map(|name| {
            let board = read_named(state_dir, &name)?;
            Ok(Summary {
                name,
                exists: board.exists,
                revision: board.revision,
                last_updated: board.last_updated,
                last_updated_source: board.last_updated_source,
            })
        })
        .collect()
}

fn apply(markdown: &mut String, edit: &Edit) -> Result<()> {
    match edit {
        Edit::Replace { old, new } => {
            ensure!(
                !old.is_empty(),
                "Replacement text must not be empty; use set or append to initialize the board"
            );
            let Some(index) = markdown.find(old.as_str()) else {
                anyhow::bail!("Replacement text was not found; read the board and retry");
            };
            ensure!(
                markdown.rfind(old.as_str()) == Some(index),
                "Replacement text is ambiguous; select a unique row or section"
            );
            let final_length = markdown
                .len()
                .checked_sub(old.len())
                .and_then(|len| len.checked_add(new.len()))
                .context("Board size overflow")?;
            ensure!(final_length <= MAX_BOARD_BYTES, "Board exceeds 64 KiB");
            markdown.replace_range(index..index + old.len(), new);
        }
        Edit::Append { text } => {
            ensure!(
                markdown.len().saturating_add(text.len()) <= MAX_BOARD_BYTES,
                "Board exceeds 64 KiB"
            );
            markdown.push_str(text);
        }
        Edit::Prepend { text } => {
            ensure!(
                markdown.len().saturating_add(text.len()) <= MAX_BOARD_BYTES,
                "Board exceeds 64 KiB"
            );
            markdown.insert_str(0, text);
        }
        Edit::Set {
            markdown: replacement,
        } => {
            ensure!(replacement.len() <= MAX_BOARD_BYTES, "Board exceeds 64 KiB");
            markdown.clone_from(replacement);
        }
    }
    Ok(())
}

/// Validate all edits against an exact content revision before atomically replacing
/// the file. Validation/conflict failures leave it unchanged; an I/O failure after
/// rename may mean the new content was applied but durability is unconfirmed.
/// Read again before retrying an ambiguous failure with a fresh revision.
/// Concurrent callers must be serialized by the broker's administration mutex.
pub fn update(state_dir: &Path, expected_revision: &str, edits: &[Edit]) -> Result<Snapshot> {
    update_named(state_dir, &Name::default(), expected_revision, edits)
}
pub fn update_named(
    state_dir: &Path,
    name: &Name,
    expected_revision: &str,
    edits: &[Edit],
) -> Result<Snapshot> {
    ensure!(
        expected_revision.len() == 64
            && expected_revision
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()),
        "Expected revision must be a full SHA-256 hash returned by board_read"
    );
    ensure!(
        (1..=MAX_EDITS).contains(&edits.len()),
        "A board update requires 1–32 edits"
    );
    let state = root(state_dir)?;
    let existing_directory = board_directory(&state, false)?;
    let current = match &existing_directory {
        Some(directory) => read_document(state_dir, name, directory)?,
        None => {
            ensure!(
                name.is_main(),
                "Board {name} not found; create it explicitly first"
            );
            snapshot(state_dir, name, String::new(), false)
        }
    };
    if current.revision != expected_revision {
        return Err(Conflict { current }.into());
    }
    let mut markdown = current.markdown.clone();
    for edit in edits {
        apply(&mut markdown, edit)?;
    }
    if markdown == current.markdown {
        return Ok(current);
    }
    let directory = match existing_directory {
        Some(directory) => directory,
        None => board_directory(&state, true)?.context("Board directory was not created")?,
    };
    let temporary = CString::new(format!(".board-{}.tmp", uuid::Uuid::new_v4()))?;
    let result = (|| -> Result<std::fs::Metadata> {
        let mut file = open_at(
            &directory,
            &temporary,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        )
        .context("Cannot stage board update")?;
        file.write_all(markdown.as_bytes())?;
        file.sync_all()?;
        let metadata = file.metadata()?;
        let result = unsafe {
            libc::renameat(
                directory.as_raw_fd(),
                temporary.as_ptr(),
                directory.as_raw_fd(),
                name.document().as_ptr(),
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .context("Cannot replace board document atomically");
        }
        directory
            .sync_all()
            .context("Cannot persist board directory")?;
        Ok(metadata)
    })();
    if result.is_err() {
        unsafe {
            libc::unlinkat(directory.as_raw_fd(), temporary.as_ptr(), 0);
        }
    }
    let metadata = result?;
    let mut board = snapshot(state_dir, name, markdown, true);
    assign_file_time(&mut board, &metadata);
    Ok(board)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };

    fn private_state() -> tempfile::TempDir {
        let state = tempfile::tempdir().unwrap();
        fs::set_permissions(state.path(), fs::Permissions::from_mode(0o700)).unwrap();
        state
    }
    fn set(state: &Path, markdown: &str) -> Snapshot {
        let before = read(state).unwrap();
        update(
            state,
            &before.revision,
            &[Edit::Set {
                markdown: markdown.into(),
            }],
        )
        .unwrap()
    }

    #[test]
    fn descriptor_validation_detects_concurrent_in_place_and_unlink_changes() {
        let state = private_state();
        let board = set(state.path(), "before");
        let file = File::open(&board.path).unwrap();
        let before = file.metadata().unwrap();
        let path = board.path.clone();
        std::thread::spawn(move || {
            let mut writer = OpenOptions::new().write(true).open(path).unwrap();
            writer.write_all(b"after!").unwrap();
            writer
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000))
                .unwrap();
        })
        .join()
        .unwrap();
        assert!(!stable_metadata(&before, &file.metadata().unwrap()));
        let stable = file.metadata().unwrap();
        // Atomic external saves do not mutate the open old descriptor's bytes.
        let replacement = state.path().join("replacement");
        std::fs::write(&replacement, "replacement").unwrap();
        std::fs::rename(&replacement, &board.path).unwrap();
        // Unlink changes ctime/nlink, so rejecting rather than returning a
        // snapshot is also honest; the replacement is available on retry.
        assert!(!stable_metadata(&stable, &file.metadata().unwrap()));
    }

    #[test]
    fn board_names_validate_all_input_boundaries_and_schema() {
        for value in [
            "",
            "Main",
            ".",
            "..",
            "/tmp/a",
            "a/b",
            "a.md",
            "-a",
            "_a",
            "a b",
            "é",
            &"x".repeat(65),
        ] {
            assert!(value.parse::<Name>().is_err(), "{value}");
            assert!(
                serde_json::from_value::<Name>(serde_json::json!(value)).is_err(),
                "{value}"
            );
        }
        for value in ["main", "0", "a-b_c", &"a".repeat(64)] {
            let name: Name = value.parse().unwrap();
            assert_eq!(name.as_str(), value);
            assert_eq!(serde_json::to_value(name).unwrap(), value);
        }
        let schema = schemars::schema_for!(Name);
        assert_eq!(schema.as_value()["pattern"], "^[a-z0-9][a-z0-9_-]{0,63}$");
    }

    #[test]
    fn named_boards_are_explicit_independent_and_revision_bound() {
        let state = private_state();
        let alpha: Name = "alpha".parse().unwrap();
        let beta: Name = "beta".parse().unwrap();
        let unknown: Name = "unknown".parse().unwrap();
        assert!(read_named(state.path(), &unknown).is_err());
        assert!(
            update_named(
                state.path(),
                &unknown,
                &"0".repeat(64),
                &[Edit::Set {
                    markdown: "x".into()
                }]
            )
            .is_err()
        );
        assert!(!state.path().join("boards").exists());
        let empty_alpha = create(state.path(), &alpha).unwrap();
        let empty_beta = create(state.path(), &beta).unwrap();
        assert_ne!(empty_alpha.revision, empty_beta.revision);
        assert!(
            update_named(
                state.path(),
                &beta,
                &empty_alpha.revision,
                &[Edit::Set {
                    markdown: "wrong".into()
                }]
            )
            .unwrap_err()
            .is::<Conflict>()
        );
        let written = update_named(
            state.path(),
            &alpha,
            &empty_alpha.revision,
            &[Edit::Set {
                markdown: "private alpha".into(),
            }],
        )
        .unwrap();
        assert!(create(state.path(), &alpha).is_err());
        assert_eq!(
            read_named(state.path(), &alpha).unwrap().markdown,
            written.markdown
        );
        assert_eq!(read_named(state.path(), &beta).unwrap().markdown, "");
        assert!(!read(state.path()).unwrap().exists);
        let listed = list(state.path()).unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta", "main"]
        );
        let serialized = serde_json::to_value(&listed).unwrap();
        assert!(
            serialized
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry.get("markdown").is_none())
        );
        assert_eq!(
            read(state.path()).unwrap().revision,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn named_security_failures_preserve_main_and_reject_unsafe_listing() {
        let state = private_state();
        let main = set(state.path(), "main content");
        let name: Name = "secure".parse().unwrap();
        let secure = create(state.path(), &name).unwrap();
        fs::remove_file(&secure.path).unwrap();
        symlink(&main.path, &secure.path).unwrap();
        assert!(read_named(state.path(), &name).is_err());
        assert!(list(state.path()).is_err());
        assert!(create(state.path(), &name).is_err());
        fs::remove_file(&secure.path).unwrap();
        fs::hard_link(&main.path, &secure.path).unwrap();
        assert!(read_named(state.path(), &name).is_err());
        fs::remove_file(&secure.path).unwrap();
        assert_eq!(read(state.path()).unwrap().markdown, "main content");
        let secure = create(state.path(), &name).unwrap();
        fs::set_permissions(&secure.path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_named(state.path(), &name).is_err());
        assert!(list(state.path()).is_err());
        assert_eq!(read(state.path()).unwrap().markdown, "main content");
    }

    #[test]
    fn listing_ignores_arbitrary_names_and_explicitly_bounds_enumeration() {
        let state = private_state();
        set(state.path(), "main");
        let directory = state.path().join("boards");
        fs::write(directory.join("secret.txt"), "not a board").unwrap();
        fs::write(directory.join("Upper.md"), "not a board").unwrap();
        symlink("/nonexistent", directory.join("arbitrary-link")).unwrap();
        assert_eq!(list(state.path()).unwrap().len(), 1);
        for index in 0..MAX_DIRECTORY_ENTRIES {
            fs::write(directory.join(format!("ignored-{index}")), "").unwrap();
        }
        assert!(list(state.path()).unwrap_err().to_string().contains("1024"));
    }

    #[test]
    fn creation_limit_reserves_main_and_keeps_a_full_listing_usable() {
        let state = private_state();
        create(state.path(), &"board-0".parse().unwrap()).unwrap();
        let directory = state.path().join("boards");
        for index in 1..MAX_BOARDS - 1 {
            let path = directory.join(format!("board-{index}.md"));
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .unwrap();
            drop(file);
        }
        let full = list(state.path()).unwrap();
        assert_eq!(full.len(), MAX_BOARDS);
        assert!(
            !full
                .iter()
                .find(|entry| entry.name.is_main())
                .unwrap()
                .exists
        );
        let overflow: Name = "overflow".parse().unwrap();
        assert!(create(state.path(), &overflow).is_err());
        assert!(!directory.join("overflow.md").exists());
        assert_eq!(list(state.path()).unwrap().len(), MAX_BOARDS);
        // Main already occupies a reserved listing slot and can still be created.
        assert!(create(state.path(), &Name::default()).unwrap().exists);
        assert_eq!(list(state.path()).unwrap().len(), MAX_BOARDS);
        assert!(create(state.path(), &overflow).is_err());
    }

    #[test]
    fn concurrent_create_has_exactly_one_winner_without_overwriting() {
        let state = private_state();
        let name: Name = "race".parse().unwrap();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let one = scope.spawn(|| {
                barrier.wait();
                create(state.path(), &name).is_ok()
            });
            let two = scope.spawn(|| {
                barrier.wait();
                create(state.path(), &name).is_ok()
            });
            assert_ne!(one.join().unwrap(), two.join().unwrap());
        });
        assert!(read_named(state.path(), &name).unwrap().exists);
    }
    #[test]
    fn absent_read_is_empty_and_does_not_create_files() {
        let state = private_state();
        let board = read(state.path()).unwrap();
        assert_eq!(board.name, "main");
        assert!(!board.exists);
        assert!(board.markdown.is_empty());
        assert_eq!(board.revision.len(), 64);
        assert!(!state.path().join("boards").exists());
    }

    #[test]
    fn incremental_unicode_updates_are_atomic_and_preserve_arbitrary_markdown() {
        let state = private_state();
        let original =
            "# 🦀 Team\n\n| Agent | Task |\n| --- | --- |\n| α | Research |\n| β | Test |\n";
        let before = set(state.path(), original);
        let after = update(
            state.path(),
            &before.revision,
            &[
                Edit::Replace {
                    old: "| α | Research |".into(),
                    new: "| α | Needs owner |".into(),
                },
                Edit::Prepend {
                    text: "<!-- user-defined board -->\n".into(),
                },
                Edit::Append {
                    text: "\n```text\n\u{1b}[31m literal terminal data\n```\n".into(),
                },
            ],
        )
        .unwrap();
        assert!(after.markdown.contains("| β | Test |"));
        assert!(after.markdown.contains("| α | Needs owner |"));
        assert!(after.markdown.contains('\u{1b}'));
        assert_eq!(fs::read_to_string(&after.path).unwrap(), after.markdown);
        assert_eq!(fs::metadata(&after.path).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn stale_revision_returns_current_document_without_overwriting_rows() {
        let state = private_state();
        let before = set(state.path(), "| A | old |\n| B | old |\n");
        let first = update(
            state.path(),
            &before.revision,
            &[Edit::Replace {
                old: "| A | old |".into(),
                new: "| A | done |".into(),
            }],
        )
        .unwrap();
        let failure = update(
            state.path(),
            &before.revision,
            &[Edit::Replace {
                old: "| B | old |".into(),
                new: "| B | done |".into(),
            }],
        )
        .unwrap_err();
        let conflict = failure.downcast_ref::<Conflict>().unwrap();
        assert_eq!(conflict.current.revision, first.revision);
        let final_board = update(
            state.path(),
            &conflict.current.revision,
            &[Edit::Replace {
                old: "| B | old |".into(),
                new: "| B | done |".into(),
            }],
        )
        .unwrap();
        assert_eq!(final_board.markdown, "| A | done |\n| B | done |\n");
    }

    #[test]
    fn invalid_or_ambiguous_batch_leaves_document_unchanged_and_no_staging_files() {
        let state = private_state();
        let before = set(state.path(), "same\nsame\nunique\n");
        for edits in [
            vec![
                Edit::Append {
                    text: "would change".into(),
                },
                Edit::Replace {
                    old: "missing".into(),
                    new: "x".into(),
                },
            ],
            vec![Edit::Replace {
                old: "same".into(),
                new: "x".into(),
            }],
            vec![Edit::Replace {
                old: String::new(),
                new: "x".into(),
            }],
        ] {
            assert!(update(state.path(), &before.revision, &edits).is_err());
            assert_eq!(read(state.path()).unwrap().markdown, before.markdown);
        }
        assert_eq!(
            fs::read_dir(state.path().join("boards")).unwrap().count(),
            1
        );
    }

    #[test]
    fn overlapping_replacement_matches_are_ambiguous() {
        let state = private_state();
        for (markdown, old) in [("aaa", "aa"), ("ééé", "éé")] {
            let before = set(state.path(), markdown);
            assert!(
                update(
                    state.path(),
                    &before.revision,
                    &[Edit::Replace {
                        old: old.into(),
                        new: "changed".into()
                    }]
                )
                .is_err()
            );
            assert_eq!(read(state.path()).unwrap().markdown, markdown);
        }
    }

    #[test]
    fn bounds_and_invalid_requests_never_create_or_truncate_a_board() {
        let state = private_state();
        let before = read(state.path()).unwrap();
        assert!(
            update(
                state.path(),
                "invalid",
                &[Edit::Append { text: "x".into() }]
            )
            .is_err()
        );
        assert!(update(state.path(), &before.revision, &[]).is_err());
        assert!(
            update(
                state.path(),
                &before.revision,
                &vec![
                    Edit::Append {
                        text: String::new()
                    };
                    MAX_EDITS + 1
                ]
            )
            .is_err()
        );
        assert!(
            update(
                state.path(),
                &before.revision,
                &[Edit::Set {
                    markdown: "é".repeat(MAX_BOARD_BYTES / 2 + 1)
                }]
            )
            .is_err()
        );
        assert!(!state.path().join("boards").exists());
        let full = set(state.path(), &"x".repeat(MAX_BOARD_BYTES));
        assert!(
            update(
                state.path(),
                &full.revision,
                &[Edit::Append { text: "x".into() }]
            )
            .is_err()
        );
        assert_eq!(read(state.path()).unwrap().markdown.len(), MAX_BOARD_BYTES);
    }

    #[test]
    fn unsafe_board_files_and_directory_aliases_are_rejected() {
        let state = private_state();
        let outside = private_state();
        symlink(outside.path(), state.path().join("boards")).unwrap();
        assert!(read(state.path()).is_err());
        fs::remove_file(state.path().join("boards")).unwrap();
        let before = set(state.path(), "private");
        let other = state.path().join("other");
        fs::rename(&before.path, &other).unwrap();
        symlink(&other, &before.path).unwrap();
        assert!(read(state.path()).is_err());
        fs::remove_file(&before.path).unwrap();
        fs::hard_link(&other, &before.path).unwrap();
        assert!(read(state.path()).is_err());
        fs::remove_file(&other).unwrap();
        fs::set_permissions(&before.path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read(state.path()).is_err());
        fs::set_permissions(&before.path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read(state.path()).unwrap().markdown, "private");
        fs::remove_file(&before.path).unwrap();
        let fifo = CString::new(before.path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(read(state.path()).is_err());
    }

    #[test]
    fn oversized_and_invalid_utf8_existing_documents_are_rejected() {
        let state = private_state();
        let before = set(state.path(), "small");
        fs::write(&before.path, [0xff]).unwrap();
        assert!(read(state.path()).is_err());
        fs::write(&before.path, vec![b'x'; MAX_BOARD_BYTES + 1]).unwrap();
        assert!(read(state.path()).is_err());
    }
}

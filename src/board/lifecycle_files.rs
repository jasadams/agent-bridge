//! Descriptor-relative filesystem steps for persisted lifecycle intents.
//! No caller may discard its intent before the final directory fsync succeeds.
use super::*;

/// Long-lived original-object evidence. Unlike staging identities, an archived
/// inode may be recycled while the broker is offline. Compare this and the exact
/// immutable bytes before replacing or unlinking the original path.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Fingerprint {
    changed_seconds: i64,
    changed_nanoseconds: i64,
    length: u64,
}

fn fingerprint(metadata: &std::fs::Metadata) -> Fingerprint {
    Fingerprint {
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
        length: metadata.len(),
    }
}

fn original_proof(
    directory: &File,
    name: &Name,
    expected: FileIdentity,
    markdown: &str,
) -> Result<Option<Fingerprint>> {
    let mut file = match open_at(directory, &name.document(), libc::O_RDONLY) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Cannot inspect archived original"),
    };
    ensure!(identity(&file)? == expected, "Archived path was replaced");
    let before = file.metadata()?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_BOARD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    ensure!(
        stable_metadata(&before, &after) && bytes == markdown.as_bytes(),
        "Archived original changed; retaining archive and unrelated content"
    );
    Ok(Some(fingerprint(&after)))
}

pub(crate) fn capture_original(
    state_dir: &Path,
    name: &Name,
    expected: FileIdentity,
    markdown: &str,
) -> Result<Fingerprint> {
    let state = root(state_dir)?;
    let directory = board_directory(&state, false)?.context("Missing archive directory")?;
    original_proof(&directory, name, expected, markdown)?.context("Archive original disappeared")
}

fn validate_original_at(
    directory: &File,
    name: &Name,
    expected: FileIdentity,
    proof: Fingerprint,
    markdown: &str,
) -> Result<bool> {
    let current = original_proof(directory, name, expected, markdown)?;
    ensure!(
        current.is_none() || current == Some(proof),
        "Archived original fingerprint changed; refusing replacement or cleanup"
    );
    Ok(current.is_some())
}

pub(crate) fn validate_original(
    state_dir: &Path,
    name: &Name,
    expected: FileIdentity,
    proof: Fingerprint,
    markdown: &str,
) -> Result<()> {
    let state = root(state_dir)?;
    if let Some(directory) = board_directory(&state, false)? {
        validate_original_at(&directory, name, expected, proof, markdown)?;
    }
    Ok(())
}

fn identity(file: &File) -> Result<FileIdentity> {
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o7777 == 0o600
            && meta.nlink() == 1
            && meta.len() <= MAX_BOARD_BYTES as u64,
        "Lifecycle artifact must be a private, bounded, regular single-link file"
    );
    FileIdentity::from_metadata(&meta)
}

fn temporary(value: &str) -> Result<CString> {
    let uuid = value
        .strip_prefix(".restore-")
        .and_then(|v| v.strip_suffix(".tmp"))
        .context("Invalid restore staging name")?;
    ensure!(
        uuid::Uuid::parse_str(uuid)?.to_string() == uuid,
        "Invalid restore staging identity"
    );
    Ok(CString::new(value)?)
}

fn lookup(directory: &File, name: &CStr) -> Result<Option<FileIdentity>> {
    match open_at(directory, name, libc::O_RDONLY) {
        Ok(file) => Ok(Some(identity(&file)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("Cannot inspect managed lifecycle artifact"),
    }
}

pub(crate) fn target(state_dir: &Path, name: &Name) -> Result<Option<FileIdentity>> {
    let state = root(state_dir)?;
    match board_directory(&state, false)? {
        Some(directory) => lookup(&directory, &name.document()),
        None => Ok(None),
    }
}

/// Before content is written, persist this identity in the restore intent.
/// An interrupted pre-identity create may leave only an empty reserved artifact.
pub(crate) fn stage_empty(state_dir: &Path, value: &str) -> Result<FileIdentity> {
    let state = root(state_dir)?;
    let directory = board_directory(&state, true)?.context("Missing board directory")?;
    let name = temporary(value)?;
    let file = match open_at(
        &directory,
        &name,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
    ) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let file = open_at(&directory, &name, libc::O_RDWR)?;
            ensure!(
                file.metadata()?.len() == 0,
                "Unrecorded restore staging file is not empty"
            );
            file
        }
        Err(error) => return Err(error).context("Cannot create restore staging file"),
    };
    let id = identity(&file)?;
    file.sync_all()
        .context("Cannot persist empty restore staging file")?;
    directory
        .sync_all()
        .context("Cannot persist restore staging directory entry")?;
    Ok(id)
}

/// Rebuilding a partially written temp is safe only after its identity is durable.
pub(crate) fn write_staged(
    state_dir: &Path,
    value: &str,
    expected: FileIdentity,
    markdown: &str,
) -> Result<()> {
    ensure!(
        markdown.len() <= MAX_BOARD_BYTES,
        "Archived board exceeds 64 KiB"
    );
    let state = root(state_dir)?;
    let directory =
        board_directory(&state, false)?.context("Restore staging directory disappeared")?;
    let mut file = open_at(&directory, &temporary(value)?, libc::O_RDWR)?;
    ensure!(
        identity(&file)? == expected,
        "Restore staging file was replaced"
    );
    file.set_len(0)?;
    file.write_all(markdown.as_bytes())?;
    file.sync_all()
        .context("Cannot persist restored Markdown")?;
    Ok(())
}

/// Idempotent across rename and fsync ambiguity. Never overwrite an unexpected target.
pub(crate) fn install(
    state_dir: &Path,
    name: &Name,
    value: &str,
    staged: FileIdentity,
    original: FileIdentity,
    proof: Fingerprint,
    markdown: &str,
) -> Result<()> {
    let state = root(state_dir)?;
    let directory = board_directory(&state, false)?.context("Restore directory disappeared")?;
    let current = lookup(&directory, &name.document())?;
    if current == Some(staged) {
        let snapshot = read_document(state_dir, name, &directory)?;
        ensure!(
            snapshot.markdown == markdown && snapshot.file_identity == Some(staged),
            "Installed restore content changed"
        );
        return directory
            .sync_all()
            .context("Cannot persist restored directory entry");
    }
    validate_original_at(&directory, name, original, proof, markdown)?;
    let temporary = temporary(value)?;
    let file = open_at(&directory, &temporary, libc::O_RDONLY)?;
    ensure!(
        identity(&file)? == staged,
        "Restore staging file was replaced"
    );
    let mut bytes = Vec::new();
    file.take(MAX_BOARD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes == markdown.as_bytes(),
        "Restore staging content is incomplete"
    );
    let result = unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            temporary.as_ptr(),
            directory.as_raw_fd(),
            name.document().as_ptr(),
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("Cannot install restored board");
    }
    directory
        .sync_all()
        .context("Cannot persist restored directory entry")
}

/// Called only with a durable purging intent while the broker reserves this name.
pub(crate) fn purge(
    state_dir: &Path,
    name: &Name,
    expected: FileIdentity,
    proof: Fingerprint,
    markdown: &str,
) -> Result<()> {
    let state = root(state_dir)?;
    let Some(directory) = board_directory(&state, false)? else {
        return state
            .sync_all()
            .context("Cannot persist absent archive directory");
    };
    if validate_original_at(&directory, name, expected, proof, markdown)? {
        let result = unsafe { libc::unlinkat(directory.as_raw_fd(), name.document().as_ptr(), 0) };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .context("Cannot remove expired archived board");
        }
    }
    directory
        .sync_all()
        .context("Cannot persist archive deletion")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn missing_birth_time_has_no_inode_only_fallback() {
        let error = FileIdentity::from_created(
            1,
            2,
            Err(std::io::Error::from(std::io::ErrorKind::Unsupported)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("filesystem birth-time identity"));
    }

    #[test]
    fn same_inode_and_bytes_do_not_authorize_changed_birth_or_original_fingerprint() {
        let state = tempfile::tempdir().unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let name: Name = "identity-proof".parse().unwrap();
        let initial = create(state.path(), &name).unwrap();
        let saved = update_named(
            state.path(),
            &name,
            &initial.revision,
            &[Edit::Set {
                markdown: "exact unchanged bytes".into(),
            }],
        )
        .unwrap();
        let original = target(state.path(), &name).unwrap().unwrap();
        let proof = capture_original(state.path(), &name, original, &saved.markdown).unwrap();
        // Deterministically model recycled device/inode numbers with identical
        // content. No reliance on the host filesystem allocator or clock sleeps.
        let mut prior_birth = original;
        prior_birth.created_nanoseconds = (prior_birth.created_nanoseconds + 1) % 1_000_000_000;
        assert!(purge(state.path(), &name, prior_birth, proof, &saved.markdown).is_err());
        let mut prior_change = proof;
        prior_change.changed_nanoseconds = (prior_change.changed_nanoseconds + 1) % 1_000_000_000;
        assert!(purge(state.path(), &name, original, prior_change, &saved.markdown).is_err());
        let temporary = format!(".restore-{}.tmp", uuid::Uuid::new_v4());
        let staged = stage_empty(state.path(), &temporary).unwrap();
        write_staged(state.path(), &temporary, staged, &saved.markdown).unwrap();
        assert!(
            install(
                state.path(),
                &name,
                &temporary,
                staged,
                prior_birth,
                proof,
                &saved.markdown
            )
            .is_err()
        );
        assert!(
            install(
                state.path(),
                &name,
                &temporary,
                staged,
                original,
                prior_change,
                &saved.markdown
            )
            .is_err()
        );
        let mut prior_stage = staged;
        prior_stage.created_nanoseconds = (prior_stage.created_nanoseconds + 1) % 1_000_000_000;
        assert!(write_staged(state.path(), &temporary, prior_stage, "foreign overwrite").is_err());
        assert!(
            install(
                state.path(),
                &name,
                &temporary,
                prior_stage,
                original,
                proof,
                &saved.markdown
            )
            .is_err()
        );
        assert_eq!(
            read_named(state.path(), &name).unwrap().markdown,
            saved.markdown
        );
        assert_eq!(target(state.path(), &name).unwrap(), Some(original));
        assert_eq!(
            std::fs::read_to_string(state.path().join("boards").join(temporary)).unwrap(),
            saved.markdown
        );
        // Supported filesystems must preserve the full birth identity across
        // writes and rename; otherwise installation/recovery fails closed.
        let temporary = format!(".restore-{}.tmp", uuid::Uuid::new_v4());
        let staged = stage_empty(state.path(), &temporary).unwrap();
        write_staged(state.path(), &temporary, staged, &saved.markdown).unwrap();
        install(
            state.path(),
            &name,
            &temporary,
            staged,
            original,
            proof,
            &saved.markdown,
        )
        .unwrap();
        assert_eq!(target(state.path(), &name).unwrap(), Some(staged));
    }
}

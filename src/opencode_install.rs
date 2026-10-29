//! Explicit OpenCode V1 setup. It never starts a service, native client or model.
mod config;
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
};

const ASSETS: &[(&str, &str)] = &[
    ("index.ts", include_str!("../plugins/opencode/index.ts")),
    ("rpc.ts", include_str!("../plugins/opencode/rpc.ts")),
    ("peer.ts", include_str!("../plugins/opencode/peer.ts")),
    ("journal.ts", include_str!("../plugins/opencode/journal.ts")),
    ("LICENSE", include_str!("../LICENSE")),
];
const LIMIT: u64 = 4 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    target: PathBuf,
    owned: config::Owned,
    assets: Vec<PathBuf>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    version: u32,
    target: PathBuf,
    before: Option<String>,
    after: String,
    manifest_before: Option<String>,
    manifest_after: Option<String>,
    observed: Vec<(PathBuf, Option<String>)>,
}
struct Environment {
    global: PathBuf,
    data: PathBuf,
    custom: Option<PathBuf>,
    extra: Option<PathBuf>,
    inline: Option<String>,
    binary: PathBuf,
}
impl Environment {
    fn current() -> Result<Self> {
        let home = crate::service::home()?;
        Ok(Self {
            global: std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"))
                .join("opencode"),
            data: std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/share"))
                .join("agent-bridge/opencode"),
            custom: std::env::var_os("OPENCODE_CONFIG").map(PathBuf::from),
            extra: std::env::var_os("OPENCODE_CONFIG_DIR").map(PathBuf::from),
            inline: std::env::var("OPENCODE_CONFIG_CONTENT").ok(),
            binary: std::env::current_exe()?,
        })
    }
}
fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn absolute(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && !path
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
            && !path
                .as_os_str()
                .as_bytes()
                .iter()
                .any(|b| *b < 32 || *b == 127),
        "Use an absolute normalized path without control characters: {}",
        path.display()
    );
    Ok(())
}
// macOS exposes root-owned /tmp, /var and /etc aliases. Resolve only these
// system aliases; user-controlled symlink configuration paths remain forbidden.
fn system_path(path: &Path) -> Result<PathBuf> {
    absolute(path)?;
    if cfg!(target_os = "macos") {
        for alias in ["/tmp", "/var", "/etc"] {
            let prefix = Path::new(alias);
            if let Ok(suffix) = path.strip_prefix(prefix) {
                let metadata = fs::symlink_metadata(prefix)?;
                if metadata.file_type().is_symlink() && metadata.uid() == 0 {
                    let resolved = fs::canonicalize(prefix)?;
                    ensure!(
                        resolved.starts_with("/private"),
                        "Unexpected system directory alias: {alias}"
                    );
                    return Ok(resolved.join(suffix));
                }
            }
        }
    }
    Ok(path.into())
}

fn uid() -> u32 {
    unsafe { libc::geteuid() }
}
fn check_dir(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)
        .with_context(|| format!("Inspect directory {}", path.display()))?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "Refusing non-directory or symlink: {}",
        path.display()
    );
    ensure!(
        meta.uid() == uid() || meta.uid() == 0,
        "Directory belongs to another user: {}",
        path.display()
    );
    ensure!(
        meta.mode() & 0o022 == 0 || (meta.uid() == 0 && meta.mode() & 0o1000 != 0),
        "Directory is writable by others: {}",
        path.display()
    );
    Ok(())
}
fn directories(path: &Path, create: bool) -> Result<()> {
    absolute(path)?;
    let mut current = PathBuf::from("/");
    for component in path.components().skip(1) {
        current.push(component.as_os_str());
        if !current.try_exists()? {
            ensure!(create, "Missing directory: {}", current.display());
            fs::DirBuilder::new().mode(0o700).create(&current)?;
            sync_parent(&current)?;
        }
        check_dir(&current)?;
    }
    Ok(())
}
fn read(path: &Path) -> Result<Option<String>> {
    absolute(path)?;
    if let Some(parent) = path.parent() {
        if !parent.try_exists()? {
            return Ok(None);
        }
        directories(parent, false)?;
    }
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("Read {}", path.display())),
    };
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.uid() == uid() && meta.nlink() == 1 && meta.mode() & 0o022 == 0,
        "Refusing unsafe configuration/installer file: {}",
        path.display()
    );
    ensure!(
        meta.len() <= LIMIT,
        "Configuration/installer file exceeds 4 MiB"
    );
    let mut text = String::new();
    Read::by_ref(&mut file)
        .take(LIMIT + 1)
        .read_to_string(&mut text)?;
    ensure!(
        text.len() as u64 <= LIMIT,
        "Configuration/installer file exceeds 4 MiB"
    );
    Ok(Some(text))
}
fn read_private(path: &Path) -> Result<Option<String>> {
    let text = read(path)?;
    if text.is_some() {
        ensure!(
            fs::symlink_metadata(path)?.mode() & 0o077 == 0,
            "Installer metadata must be private to your user: {}",
            path.display()
        );
    }
    Ok(text)
}

fn sync_parent(path: &Path) -> Result<()> {
    File::open(path.parent().context("Missing parent")?)?.sync_all()?;
    Ok(())
}
fn replace(path: &Path, text: &str) -> Result<()> {
    directories(path.parent().context("Missing parent")?, false)?;
    let mode = fs::symlink_metadata(path)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0o600);
    let temp = path.with_file_name(format!(".agent-bridge-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        sync_parent(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
fn remove(path: &Path) -> Result<()> {
    if path.try_exists()? {
        read(path)?;
        fs::remove_file(path)?;
        sync_parent(path)?;
    }
    Ok(())
}
fn sidecar(target: &Path, suffix: &str) -> Result<PathBuf> {
    let name = target
        .file_name()
        .context("Configuration must name a file")?
        .to_str()
        .context("Configuration filename must be UTF-8")?;
    Ok(target.with_file_name(format!(".{name}.agent-bridge-{suffix}")))
}
fn url(path: &Path) -> String {
    let mut result = String::from("file://");
    for byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(byte) {
            result.push(*byte as char);
        } else {
            result.push_str(&format!("%{byte:02X}"));
        }
    }
    result
}
fn asset_namespace(env: &Environment, target: &Path) -> PathBuf {
    env.data.join(digest(target.as_os_str().as_bytes()))
}
fn asset_version(namespace: &Path) -> PathBuf {
    let mut bytes = Vec::new();
    for (name, source) in ASSETS {
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(source.as_bytes());
        bytes.push(0);
    }
    namespace.join(digest(&bytes))
}
fn extract(directory: &Path) -> Result<()> {
    directories(directory, true)?;
    for (name, text) in ASSETS {
        let path = directory.join(name);
        match read(&path)? {
            Some(old) => ensure!(
                old == *text,
                "Bundled asset was modified; preserving {}",
                path.display()
            ),
            None => replace(&path, text)?,
        }
    }
    Ok(())
}
fn candidates(directory: &Path) -> Vec<PathBuf> {
    ["config.json", "opencode.json", "opencode.jsonc"]
        .into_iter()
        .map(|n| directory.join(n))
        .collect()
}
fn select(env: &Environment, explicit: Option<&Path>) -> Result<PathBuf> {
    absolute(&env.global)?;
    absolute(&env.data)?;
    if let Some(path) = explicit {
        absolute(path)?;
        return Ok(path.into());
    }
    if let Some(path) = &env.custom {
        absolute(path)?;
        return Ok(path.clone());
    }
    let mut existing = None;
    let mut owner = None;
    for path in candidates(&env.global) {
        if let Some(text) = read(&path)? {
            let root = config::parse(&text)
                .with_context(|| format!("Invalid OpenCode config {}", path.display()))?;
            config::validate_shape(&root)?;
            existing = Some(path.clone());
            if config::value(&root, "plugin").is_some() {
                owner = Some(path);
            }
        }
    }
    Ok(owner
        .or(existing)
        .unwrap_or_else(|| env.global.join("opencode.jsonc")))
}
fn preflight(
    env: &Environment,
    target: &Path,
    namespace: &str,
) -> Result<Vec<(PathBuf, Option<String>)>> {
    // V1 loads this extensionless TOML after the JSON globals and may migrate
    // it by overwriting config.json. Do not freeze or shadow that configuration.
    let legacy = env.global.join("config");
    ensure!(
        read(&legacy)?.is_none(),
        "Legacy OpenCode TOML configuration exists at {}; migrate it with OpenCode and review the resulting JSON configuration before retrying setup",
        legacy.display()
    );
    let globals = candidates(&env.global);
    let mut paths = globals.clone();
    if let Some(path) = &env.custom {
        absolute(path)?;
        paths.push(path.clone());
    }
    if let Some(directory) = &env.extra {
        absolute(directory)?;
        paths.extend(candidates(directory));
    }
    paths.push(target.into());
    paths.sort();
    paths.dedup();
    let mut observed = vec![(legacy, None)];
    let mut active_global = None;
    for path in &globals {
        if let Some(text) = read(path)? {
            let root = config::parse(&text)?;
            config::validate_shape(&root)?;
            if config::value(&root, "plugin").is_some() {
                active_global = Some(path);
            }
        }
    }
    if globals.iter().any(|p| p == target)
        && let Some(owner) = active_global
    {
        ensure!(
            owner == target,
            "Global plugin arrays replace earlier arrays; use --config {} to preserve the effective plugins",
            owner.display()
        );
    }
    for path in paths {
        let text = read(&path)?;
        if path != target
            && let Some(text) = &text
        {
            let root = config::parse(text)
                .with_context(|| format!("Invalid OpenCode config {}", path.display()))?;
            ensure!(
                !config::conflict(&root, namespace)?,
                "Agent Bridge configuration also exists in {}; resolve that conflict before changing {}",
                path.display(),
                target.display()
            );
        }
        observed.push((path, text));
    }
    if let Some(text) = &env.inline {
        ensure!(
            !config::conflict(&config::parse(text)?, namespace)?,
            "OPENCODE_CONFIG_CONTENT contains conflicting Agent Bridge settings; remove that override before installing"
        );
    }
    Ok(observed)
}
fn verify(observed: &[(PathBuf, Option<String>)]) -> Result<()> {
    for (path, before) in observed {
        ensure!(
            read(path)? == *before,
            "Configuration changed concurrently: {}; retry",
            path.display()
        );
    }
    Ok(())
}
fn validate_manifest(manifest: &Manifest, target: &Path, namespace: &Path) -> Result<()> {
    ensure!(
        manifest.version == 1 && manifest.target == target,
        "Installer ownership manifest does not match this configuration"
    );
    for path in &manifest.assets {
        ensure!(
            path.parent() == Some(namespace)
                && path
                    .file_name()
                    .is_some_and(|n| n.as_bytes().len() == 64
                        && n.as_bytes().iter().all(u8::is_ascii_hexdigit)),
            "Invalid owned asset path"
        );
    }
    ensure!(
        manifest
            .assets
            .iter()
            .any(|p| url(&p.join("index.ts")) == manifest.owned.url),
        "Managed plugin URL does not match owned assets"
    );
    Ok(())
}
fn recover(target: &Path, manifest_path: &Path, journal: &Path) -> Result<()> {
    let Some(text) = read_private(journal)? else {
        return Ok(());
    };
    let pending: Pending =
        serde_json::from_str(&text).context("Invalid installer recovery journal")?;
    ensure!(
        pending.version == 1 && pending.target == target,
        "Installer recovery journal targets another file"
    );
    // Other source files can change precedence or introduce a conflicting entry
    // while this transaction is interrupted; never replay across those changes.
    for (path, before) in &pending.observed {
        if path != target {
            ensure!(
                read(path)? == *before,
                "Another configuration changed after interrupted setup: {}; preserving the pending transaction",
                path.display()
            );
        }
    }
    config::parse(&pending.after)?;
    let current = read(target)?;
    ensure!(
        current == pending.before || current.as_deref() == Some(pending.after.as_str()),
        "Configuration changed after interrupted setup; preserving it and recovery journal {}",
        journal.display()
    );
    let manifest = read_private(manifest_path)?;
    ensure!(
        manifest == pending.manifest_before || manifest == pending.manifest_after,
        "Ownership changed after interrupted setup; refusing recovery"
    );
    if current == pending.before {
        replace(target, &pending.after)?;
    }
    match pending.manifest_after {
        Some(text) => replace(manifest_path, &text)?,
        None => remove(manifest_path)?,
    }
    remove(journal)
}
fn transaction(
    target: &Path,
    manifest_path: &Path,
    journal: &Path,
    observed: &[(PathBuf, Option<String>)],
    before: Option<String>,
    after: String,
    manifest_after: Option<String>,
) -> Result<()> {
    let manifest_before = read_private(manifest_path)?;
    verify(observed)?;
    if before.as_deref() == Some(after.as_str()) && manifest_before == manifest_after {
        return Ok(());
    }
    let pending = Pending {
        version: 1,
        target: target.into(),
        before,
        after,
        manifest_before,
        manifest_after,
        observed: observed.to_vec(),
    };
    let encoded = serde_json::to_string_pretty(&pending)?;
    ensure!(
        encoded.len() as u64 <= LIMIT,
        "Combined configuration is too large for safe setup recovery; no configuration was changed"
    );
    if let Some(text) = &pending.before {
        let backup = target.with_file_name(format!(
            "{}.agent-bridge-backup-{}",
            target.file_name().unwrap().to_string_lossy(),
            uuid::Uuid::new_v4()
        ));
        replace(&backup, text)?;
        println!("Backup: {}", backup.display());
    }
    replace(journal, &encoded)?;
    // Compare again after persisting the journal; failed CAS leaves recovery evidence.
    verify(observed)?;
    recover(target, manifest_path, journal)
}

pub fn configure(state: &Path, explicit: Option<&Path>, install: bool) -> Result<()> {
    let mut env = Environment::current()?;
    env.global = system_path(&env.global)?;
    env.data = system_path(&env.data)?;
    env.binary = system_path(&env.binary)?;
    env.custom = env.custom.as_deref().map(system_path).transpose()?;
    env.extra = env.extra.as_deref().map(system_path).transpose()?;
    let state = system_path(state)?;
    let explicit = explicit.map(system_path).transpose()?;
    let target = run(&env, &state, explicit.as_deref(), install)?;
    println!(
        "{} OpenCode V1 integration in {}. Restart the OpenCode process to reload plugins and MCP configuration.",
        if install { "Installed" } else { "Removed" },
        target.display()
    );
    Ok(())
}
fn run(env: &Environment, state: &Path, explicit: Option<&Path>, install: bool) -> Result<PathBuf> {
    absolute(state)?;
    absolute(&env.binary)?;
    ensure!(
        state.to_str().is_some() && env.binary.to_str().is_some(),
        "Native state and executable paths must be valid UTF-8"
    );
    let target = select(env, explicit)?;
    let namespace = asset_namespace(env, &target);
    let namespace_url = format!("{}/", url(&env.data));
    // Preflight before creating directories or installer files.
    preflight(env, &target, &namespace_url)?;
    directories(
        target.parent().context("Missing configuration parent")?,
        true,
    )?;
    let lock_path = sidecar(&target, "lock")?;
    if lock_path.try_exists()? {
        read(&lock_path)?;
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock_path)?;
    let metadata = lock.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == uid()
            && metadata.nlink() == 1
            && metadata.mode() & 0o077 == 0,
        "Unsafe installer lock"
    );
    lock.try_lock_exclusive()
        .context("Another OpenCode setup owns the configuration lock; retry after it finishes")?;
    let manifest_path = sidecar(&target, "manifest.json")?;
    let journal = sidecar(&target, "pending.json")?;
    recover(&target, &manifest_path, &journal)?;
    let observed = preflight(env, &target, &namespace_url)?;
    let before = read(&target)?;
    let old = read_private(&manifest_path)?
        .map(|text| serde_json::from_str::<Manifest>(&text))
        .transpose()?;
    if let Some(old) = &old {
        validate_manifest(old, &target, &namespace)?;
    }
    if !install && old.is_none() {
        return Ok(target);
    }
    let directory = asset_version(&namespace);
    let plugin_url = url(&directory.join("index.ts"));
    let (after, owned) = config::edit(
        before.as_deref().unwrap_or("{}\n"),
        old.as_ref().map(|m| &m.owned),
        install.then_some((plugin_url.as_str(), env.binary.as_path(), state)),
        &namespace_url,
    )?;
    let manifest_after = if let Some(owned) = owned {
        let mut assets = old.as_ref().map(|m| m.assets.clone()).unwrap_or_default();
        if !assets.contains(&directory) {
            assets.push(directory.clone());
        }
        let manifest = Manifest {
            version: 1,
            target: target.clone(),
            owned,
            assets,
        };
        extract(&directory)?;
        Some(serde_json::to_string_pretty(&manifest)?)
    } else {
        None
    };
    transaction(
        &target,
        &manifest_path,
        &journal,
        &observed,
        before,
        after,
        manifest_after,
    )?;
    // Immutable previous assets are retained: an already-running plugin may still
    // import them. Removal unregisters configuration, never live sessions or state.
    Ok(target)
}

#[cfg(test)]
mod tests;

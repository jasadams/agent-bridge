//! Release discovery is advisory: it never installs software or changes board content.
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CHECK_INTERVAL: u64 = 24 * 60 * 60;
const MAX_RESPONSE: u64 = 256 * 1024;
const RELEASE_URL: &str = "https://api.github.com/repos/jasadams/agent-bridge/releases/latest";
const CACHE_FILE: &str = "update-status.json";

#[derive(Clone, Debug, Serialize)]
pub struct UpdateStatus {
    pub current_version: String,
    pub available_version: Option<String>,
    pub release_notes_url: Option<String>,
    pub automatic_checks: bool,
    pub last_checked_at: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Cache {
    automatic_checks: Option<bool>,
    last_checked_at: Option<u64>,
    latest_version: Option<String>,
    installed_version: Option<String>,
    last_error: Option<String>,
}

/// Stable release tags deliberately match the installer's vMAJOR.MINOR.PATCH policy.
fn stable_version(value: &str) -> Option<[u64; 3]> {
    let value = value.strip_prefix('v').unwrap_or(value);
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut result = [0; 3];
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty()
            || !part.bytes().all(|byte| byte.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0'))
        {
            return None;
        }
        result[index] = part.parse().ok()?;
    }
    Some(result)
}

fn status(cache: &Cache, current: &str) -> UpdateStatus {
    let current_version = stable_version(current);
    let installed = cache.installed_version.as_deref().and_then(stable_version);
    let comparison = current_version
        .map(|current| installed.map_or(current, |installed| current.max(installed)));
    let available = cache.latest_version.as_ref().filter(|latest| {
        matches!((stable_version(latest), comparison), (Some(latest), Some(current)) if latest > current)
    });
    UpdateStatus {
        current_version: current.to_owned(),
        available_version: available.cloned(),
        release_notes_url: available.map(|version| {
            format!("https://github.com/jasadams/agent-bridge/releases/tag/v{version}")
        }),
        automatic_checks: cache.automatic_checks.unwrap_or(true),
        last_checked_at: cache.last_checked_at,
        last_error: cache.last_error.clone(),
    }
}

fn read_cache(state: &Path) -> Result<Cache> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(state.join(CACHE_FILE))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Cache::default()),
        Err(error) => return Err(error).context("Cannot open update settings/cache"),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() },
        "Update cache must be a regular file owned by your user"
    );
    let mut data = Vec::new();
    file.take(MAX_RESPONSE + 1).read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= MAX_RESPONSE,
        "Update cache is too large"
    );
    serde_json::from_slice(&data).context("Cannot parse update settings/cache")
}

fn write_cache(state: &Path, cache: &Cache) -> Result<()> {
    crate::service::atomic_write(&state.join(CACHE_FILE), &serde_json::to_vec(cache)?, 0o600)
}

fn lock(state: &Path) -> Result<File> {
    crate::broker::private_dir(state)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(state.join("update.lock"))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() },
        "Invalid update lock"
    );
    Ok(file)
}

/// Own only an acquired lock. Closing alone can leave flock held by a child
/// between fork and exec, even with CLOEXEC; release the shared description
/// explicitly before File closes it on every return/error/unwind path.
struct UpdateLock(File);

impl Drop for UpdateLock {
    fn drop(&mut self) {
        // Drop cannot report errors. File close remains the fallback if unlock
        // fails; no child may extend a successfully released lock's lifetime.
        let _ = FileExt::unlock(&self.0);
    }
}

/// Read-only and network-free; suitable for status and the board renderer.
pub fn cached_status(state: &Path) -> Result<UpdateStatus> {
    Ok(status(&read_cache(state)?, crate::VERSION))
}

pub fn set_automatic_checks(state: &Path, enabled: bool) -> Result<UpdateStatus> {
    let lock = lock(state)?;
    lock.lock_exclusive()?;
    let _lock = UpdateLock(lock);
    let mut cache = read_cache(state)?;
    cache.automatic_checks = Some(enabled);
    write_cache(state, &cache)?;
    Ok(status(&cache, crate::VERSION))
}

/// Reconcile an upgrade without discarding settings or the check throttle.
pub fn record_installed(state: &Path, version: &str) -> Result<()> {
    ensure!(
        stable_version(version).is_some(),
        "Invalid installed version"
    );
    let lock = lock(state)?;
    lock.lock_exclusive()?;
    let _lock = UpdateLock(lock);
    let mut cache = read_cache(state)?;
    cache.installed_version = Some(version.strip_prefix('v').unwrap_or(version).to_owned());
    write_cache(state, &cache)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
}

fn parse_release(bytes: &[u8]) -> Result<String> {
    ensure!(
        bytes.len() as u64 <= MAX_RESPONSE,
        "Release response is too large"
    );
    let release: Release = serde_json::from_slice(bytes).context("Invalid release response")?;
    ensure!(
        !release.draft && !release.prerelease,
        "Latest release is not stable"
    );
    ensure!(
        release.tag_name.starts_with('v') && stable_version(&release.tag_name).is_some(),
        "Latest release has an unsupported version tag"
    );
    Ok(release.tag_name[1..].to_owned())
}

fn fetch_release() -> Result<Vec<u8>> {
    fetch_response(RELEASE_URL, "=https", "10")
}

fn fetch_response(url: &str, protocol: &str, timeout: &str) -> Result<Vec<u8>> {
    let mut child = Command::new("curl")
        .args([
            "--disable",
            "--proto",
            protocol,
            "--tlsv1.2",
            "--fail",
            "--silent",
            "--connect-timeout",
            "3",
            "--max-time",
            timeout,
            "--max-filesize",
            "262144",
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            "X-GitHub-Api-Version: 2022-11-28",
            "--user-agent",
            "agent-bridge-update-check",
            url,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("Cannot run curl for release check")?;
    let mut response = Vec::new();
    let read = child
        .stdout
        .take()
        .context("Missing release response stream")?
        .take(MAX_RESPONSE + 1)
        .read_to_end(&mut response);
    if read.is_err() || response.len() as u64 > MAX_RESPONSE {
        let _ = child.kill();
        let _ = child.wait();
        read.context("Cannot read release response")?;
        bail!("Release response is too large");
    }
    ensure!(
        child.wait()?.success(),
        "Release check failed (network unavailable, timeout or HTTP error)"
    );
    Ok(response)
}

fn check_with(
    state: &Path,
    forced: bool,
    timestamp: u64,
    fetch: impl FnOnce() -> Result<Vec<u8>>,
) -> Result<UpdateStatus> {
    let lock = lock(state)?;
    if forced {
        lock.lock_exclusive()?;
    } else if let Err(error) = lock.try_lock_exclusive() {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return cached_status(state);
        }
        return Err(error).context("Cannot lock update check");
    }
    let _lock = UpdateLock(lock);
    let mut cache = read_cache(state)?;
    if !forced
        && (!cache.automatic_checks.unwrap_or(true)
            || cache
                .last_checked_at
                .is_some_and(|last| timestamp.saturating_sub(last) < CHECK_INTERVAL))
    {
        return Ok(status(&cache, crate::VERSION));
    }
    // Persist the attempt before network I/O, including failures and interrupted checks.
    cache.last_checked_at = Some(timestamp);
    write_cache(state, &cache)?;
    match fetch().and_then(|data| parse_release(&data)) {
        Ok(latest) => {
            cache.latest_version = Some(latest);
            cache.last_error = None;
            write_cache(state, &cache)?;
            Ok(status(&cache, crate::VERSION))
        }
        Err(error) => {
            cache.last_error = Some(error.to_string());
            write_cache(state, &cache)?;
            Err(error)
        }
    }
}

/// Explicit checks work even when automatic checks have been disabled.
pub fn check(state: &Path) -> Result<UpdateStatus> {
    check_with(state, true, now(), fetch_release)
}

/// Independent advisory task: slow/offline release checks cannot block broker traffic.
pub async fn run_checks(state: PathBuf) {
    // Let startup/readiness complete first; short-lived broker fixtures stay offline.
    tokio::time::sleep(Duration::from_secs(60)).await;
    loop {
        let state = state.clone();
        match tokio::task::spawn_blocking(move || check_with(&state, false, now(), fetch_release))
            .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => eprintln!("Agent Bridge update check: {error:#}"),
            Err(error) => eprintln!("Agent Bridge update check task failed: {error}"),
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Keep inherited file descriptions open across the parent's checker return.
    /// The post-fork child uses only async-signal-safe system calls, never Rust
    /// allocation, locks, destructors, or test assertions.
    struct InheritedLockChild {
        socket: UnixStream,
        pid: Option<libc::pid_t>,
    }

    impl InheritedLockChild {
        fn spawn() -> Self {
            let (parent, child) = UnixStream::pair().unwrap();
            parent
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let parent_fd = parent.as_raw_fd();
            let child_fd = child.as_raw_fd();
            // SAFETY: only the parent returns into Rust after fork. The child
            // is bounded by poll and exits directly without running destructors.
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0, "fork failed: {}", std::io::Error::last_os_error());
            if pid == 0 {
                unsafe {
                    libc::close(parent_fd);
                    let ready = b'R';
                    let sent = libc::write(child_fd, (&ready as *const u8).cast(), 1);
                    let mut descriptor = libc::pollfd {
                        fd: child_fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let polled = libc::poll(&mut descriptor, 1, 30_000);
                    let mut release = 0_u8;
                    let received = if polled == 1 {
                        libc::read(child_fd, (&mut release as *mut u8).cast(), 1)
                    } else {
                        -1
                    };
                    libc::_exit(i32::from(
                        !(sent == 1 && polled == 1 && received == 1 && release == b'X'),
                    ));
                }
            }
            drop(child);
            let mut result = Self {
                socket: parent,
                pid: Some(pid),
            };
            let mut ready = [0];
            result.socket.read_exact(&mut ready).unwrap();
            assert_eq!(ready, *b"R");
            result
        }

        fn finish(mut self) {
            let status = self.release().unwrap();
            assert!(libc::WIFEXITED(status));
            assert_eq!(libc::WEXITSTATUS(status), 0);
        }

        fn release(&mut self) -> std::io::Result<i32> {
            let Some(pid) = self.pid.take() else {
                return Ok(0);
            };
            let _ = self.socket.write_all(b"X");
            let mut status = 0;
            // SAFETY: pid is this fixture's child; the child's poll bounds the
            // wait even when the parent test unwinds before releasing it.
            loop {
                let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
                if waited == pid {
                    return Ok(status);
                }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }

    impl Drop for InheritedLockChild {
        fn drop(&mut self) {
            let _ = self.release();
        }
    }

    fn release(tag: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"tag_name":tag,"draft":false,"prerelease":false}))
            .unwrap()
    }

    fn fixture_response(
        status: &str,
        body: Vec<u8>,
        delay: Duration,
    ) -> (String, std::thread::JoinHandle<()>) {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}/", listener.local_addr().unwrap());
        let status = status.to_owned();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request);
            std::thread::sleep(delay);
            let header = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            if socket.write_all(header.as_bytes()).is_ok() {
                let _ = socket.write_all(&body);
            }
        });
        (address, server)
    }

    #[test]
    fn bounded_http_transport_handles_real_responses_failures_and_timeout() {
        let (address, server) = fixture_response("200 OK", release("v99.0.0"), Duration::ZERO);
        assert_eq!(
            parse_release(&fetch_response(&address, "=http", "2").unwrap()).unwrap(),
            "99.0.0"
        );
        server.join().unwrap();
        let (address, server) = fixture_response(
            "429 Too Many Requests",
            b"rate limited".to_vec(),
            Duration::ZERO,
        );
        assert!(fetch_response(&address, "=http", "2").is_err());
        server.join().unwrap();
        let (address, server) = fixture_response(
            "200 OK",
            vec![b'x'; MAX_RESPONSE as usize + 1],
            Duration::ZERO,
        );
        assert!(fetch_response(&address, "=http", "2").is_err());
        server.join().unwrap();
        let (address, server) =
            fixture_response("200 OK", release("v99.0.0"), Duration::from_secs(2));
        assert!(fetch_response(&address, "=http", "1").is_err());
        server.join().unwrap();
    }

    #[test]
    fn versions_and_untrusted_release_metadata() {
        for invalid in [
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.2.3-rc.1",
            "1.2.3+meta",
            "1.2.-3",
            "1.2.3\n",
        ] {
            assert!(stable_version(invalid).is_none(), "{invalid}");
        }
        assert!(stable_version("v1.10.0") > stable_version("1.9.9"));
        assert!(parse_release(&release("v1.10.0")).is_ok());
        assert!(parse_release(&release("1.10.0")).is_err());
        assert!(parse_release(b"garbage").is_err());
        assert!(parse_release(&vec![b' '; MAX_RESPONSE as usize + 1]).is_err());
        for flag in ["draft", "prerelease"] {
            let mut data =
                serde_json::json!({"tag_name":"v1.2.3","draft":false,"prerelease":false});
            data[flag] = true.into();
            assert!(parse_release(&serde_json::to_vec(&data).unwrap()).is_err());
        }
    }

    #[test]
    fn notice_only_for_newer_release_and_reconciles_after_upgrade() {
        let cache = Cache {
            latest_version: Some("1.10.0".into()),
            ..Cache::default()
        };
        assert_eq!(
            status(&cache, "1.9.9").available_version.as_deref(),
            Some("1.10.0")
        );
        assert!(status(&cache, "1.10.0").available_version.is_none());
        assert!(status(&cache, "2.0.0").available_version.is_none());
    }

    #[test]
    fn cached_checks_throttle_across_restarts_and_explicit_check_bypasses() {
        let state = tempfile::tempdir().unwrap();
        let calls = AtomicUsize::new(0);
        let fetch = || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(release("v99.0.0"))
        };
        check_with(state.path(), false, 100, fetch).unwrap();
        check_with(state.path(), false, 101, fetch).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            cached_status(state.path())
                .unwrap()
                .available_version
                .as_deref(),
            Some("99.0.0")
        );
        check_with(state.path(), true, 102, fetch).unwrap();
        check_with(state.path(), false, 102 + CHECK_INTERVAL, fetch).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn completed_check_releases_lock_while_fork_child_retains_file_description() {
        let state = tempfile::tempdir().unwrap();
        let calls = AtomicUsize::new(0);
        let fetch = || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(release("v99.0.0"))
        };
        check_with(state.path(), false, 100, fetch).unwrap();
        check_with(state.path(), false, 101, fetch).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let mut child = None;
        check_with(state.path(), true, 102, || {
            child = Some(InheritedLockChild::spawn());
            fetch()
        })
        .unwrap();
        check_with(state.path(), false, 102 + CHECK_INTERVAL, fetch).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        child.unwrap().finish();
    }

    #[test]
    fn failed_check_releases_inherited_lock_and_preserves_failure_throttle() {
        let state = tempfile::tempdir().unwrap();
        let mut child = None;
        let result = check_with(state.path(), true, 100, || {
            child = Some(InheritedLockChild::spawn());
            bail!("controlled fetch failure")
        });
        assert!(result.is_err());
        assert_eq!(
            cached_status(state.path()).unwrap().last_error.as_deref(),
            Some("controlled fetch failure")
        );
        check_with(state.path(), false, 101, || panic!("failure must throttle")).unwrap();
        let calls = AtomicUsize::new(0);
        check_with(state.path(), false, 100 + CHECK_INTERVAL, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(release("v99.0.0"))
        })
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(cached_status(state.path()).unwrap().last_error.is_none());
        child.unwrap().finish();
    }

    #[test]
    fn panicking_check_releases_inherited_lock_without_discarding_attempt_time() {
        let state = tempfile::tempdir().unwrap();
        let mut child = None;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            check_with(state.path(), true, 100, || {
                child = Some(InheritedLockChild::spawn());
                panic!("controlled fetch panic")
            })
        }));
        assert!(result.is_err());
        assert_eq!(
            cached_status(state.path()).unwrap().last_checked_at,
            Some(100)
        );
        check_with(state.path(), false, 101, || panic!("attempt must throttle")).unwrap();
        let calls = AtomicUsize::new(0);
        check_with(state.path(), false, 100 + CHECK_INTERVAL, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(release("v99.0.0"))
        })
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        child.unwrap().finish();
    }

    #[test]
    fn failure_is_persisted_and_throttled_without_discarding_previous_notice() {
        let state = tempfile::tempdir().unwrap();
        check_with(state.path(), false, 100, || Ok(release("v99.0.0"))).unwrap();
        for reason in ["offline", "timeout", "HTTP 429"] {
            assert!(check_with(state.path(), true, 200, || bail!("{reason}")).is_err());
            let cached = cached_status(state.path()).unwrap();
            assert_eq!(cached.last_error.as_deref(), Some(reason));
            assert!(cached.available_version.is_some());
            check_with(state.path(), false, 201, || {
                panic!("must not retry failed check")
            })
            .unwrap();
        }
    }

    #[test]
    fn opt_out_is_durable_but_explicit_check_still_works() {
        let state = tempfile::tempdir().unwrap();
        set_automatic_checks(state.path(), false).unwrap();
        assert!(!cached_status(state.path()).unwrap().automatic_checks);
        check_with(state.path(), false, 100, || {
            panic!("automatic check disabled")
        })
        .unwrap();
        let status = check_with(state.path(), true, 100, || Ok(release("v99.0.0"))).unwrap();
        assert!(!status.automatic_checks);
        assert!(status.available_version.is_some());
        record_installed(state.path(), "99.0.0").unwrap();
        assert!(
            cached_status(state.path())
                .unwrap()
                .available_version
                .is_none()
        );
    }

    #[test]
    fn installed_marker_uses_actual_target_and_preserves_newer_notice() {
        let state = tempfile::tempdir().unwrap();
        check_with(state.path(), true, 100, || Ok(release("v99.0.0"))).unwrap();
        record_installed(state.path(), "98.0.0").unwrap();
        assert_eq!(
            cached_status(state.path())
                .unwrap()
                .available_version
                .as_deref(),
            Some("99.0.0")
        );
        record_installed(state.path(), "99.0.0").unwrap();
        assert!(
            cached_status(state.path())
                .unwrap()
                .available_version
                .is_none()
        );
        check_with(state.path(), true, 101, || Ok(release("v99.0.0"))).unwrap();
        assert!(
            cached_status(state.path())
                .unwrap()
                .available_version
                .is_none()
        );
    }

    #[test]
    fn automatic_check_does_not_wait_for_another_checker() {
        let state = tempfile::tempdir().unwrap();
        let held = lock(state.path()).unwrap();
        held.lock_exclusive().unwrap();
        check_with(state.path(), false, 100, || panic!("already locked")).unwrap();
    }

    #[test]
    fn symlink_cache_and_lock_are_refused() {
        let state = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), state.path().join(CACHE_FILE)).unwrap();
        assert!(cached_status(state.path()).is_err());
        std::os::unix::fs::symlink(outside.path(), state.path().join("update.lock")).unwrap();
        assert!(set_automatic_checks(state.path(), false).is_err());
    }
}

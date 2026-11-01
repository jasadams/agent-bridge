use fs2::FileExt;
use std::{fs::OpenOptions, io::ErrorKind, path::Path, time::Duration};

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

//! FUSE name-quarantine integration test (issue #60).
//!
//! A `hydration.list` reply mixes a representable entry with one whose name
//! exceeds `NAME_MAX` (255 bytes) and therefore can't be a single Linux path
//! component. The mount must skip the unrepresentable entry from `readdir`
//! (and not error the whole listing), surfacing only the good one.
//!
//! Requires a FUSE-enabled environment (fusermount3 setuid + /dev/fuse rw),
//! same as `getattr_readdir.rs`.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use support::fake_jvm::{replies, FakeJvm};
use unidrive_mount::fuse_fs::UnidriveFs;
use unidrive_mount::reconnect::ReconnectingIpcClient;
mod support;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readdir_skips_entry_with_name_over_name_max() {
    // 300-byte basename: longer than NAME_MAX (255), so it cannot be a single
    // local path component. The good entry alongside it must still appear.
    let long_name = "z".repeat(300);
    let list_reply = format!(
        r#"{{"ok":true,"entries":[{{"path":"/good.txt","size":10,"mtime_ms":1000,"hydrated":false,"folder":false}},{{"path":"/{long_name}","size":20,"mtime_ms":2000,"hydrated":false,"folder":false}}]}}"#
    );
    let jvm = FakeJvm::spawn(replies(&[("hydration.list", &list_reply)])).await;

    let ipc = ReconnectingIpcClient::connect(&jvm.socket_path).await.unwrap();
    let fs = UnidriveFs::new(Arc::new(Mutex::new(ipc)));

    let tempdir = tempfile::tempdir().unwrap();
    let mount_path = tempdir.path().to_path_buf();

    let mut mount_options = fuse3::MountOptions::default();
    mount_options.fs_name("unidrive-test").nonempty(false);

    let mount_handle = fuse3::raw::Session::new(mount_options)
        .mount_with_unprivileged(fs, &mount_path)
        .await
        .expect("mount with unprivileged should succeed in FUSE-enabled env");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mp = mount_path.clone();
    let entries = tokio::task::spawn_blocking(move || {
        match std::fs::read_dir(&mp) {
            Ok(it) => it
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect::<Vec<_>>(),
            Err(e) => vec![format!("READDIR_ERROR: {e}")],
        }
    })
    .await
    .unwrap();

    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    // Only the representable entry is visible; the over-NAME_MAX one is skipped,
    // and the listing did not error out as a whole.
    assert_eq!(entries, vec!["good.txt".to_string()], "got: {entries:?}");
}

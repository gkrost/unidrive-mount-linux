//! stat shows the provider's modified time for entries that are untouched
//! locally, and the local time for hydrated / locally changed ones.
//!
//! Requires a FUSE-enabled environment, like `getattr_readdir.rs`.

use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::time::Duration;
use support::fake_jvm::{replies, FakeJvm};
use tokio::sync::Mutex;
use unidrive_mount::fuse_fs::UnidriveFs;
use unidrive_mount::reconnect::ReconnectingIpcClient;
mod support;

const LOCAL_MS: i64 = 1_791_553_945_352;
const REMOTE_MS: i64 = 1_791_369_542_165;

/// Mounts a fake engine serving `entries_json` for `hydration.list` and
/// returns the stat mtime in milliseconds of each name in `names`.
async fn stat_mtimes_ms(entries_json: &str, names: &[&str]) -> Vec<i64> {
    let reply = format!(r#"{{"ok":true,"entries":[{entries_json}]}}"#);
    let jvm = FakeJvm::spawn(replies(&[("hydration.list", reply.as_str())])).await;
    let ipc = ReconnectingIpcClient::connect(&jvm.socket_path).await.unwrap();
    let fs = UnidriveFs::new(Arc::new(Mutex::new(ipc)));
    let tempdir = tempfile::tempdir().unwrap();
    let mount_path = tempdir.path().to_path_buf();
    let mut opts = fuse3::MountOptions::default();
    opts.fs_name("unidrive-test").nonempty(false);
    let handle = fuse3::raw::Session::new(opts)
        .mount_with_unprivileged(fs, &mount_path)
        .await
        .expect("mount with unprivileged should succeed in FUSE-enabled env");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    let mp = mount_path.clone();
    let out = tokio::task::spawn_blocking(move || {
        names
            .iter()
            .map(|n| {
                let m = std::fs::metadata(mp.join(n)).expect("stat");
                m.mtime() * 1000 + m.mtime_nsec() / 1_000_000
            })
            .collect::<Vec<i64>>()
    })
    .await
    .unwrap();
    let _ = handle.unmount().await;
    jvm.shutdown().await;
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_hydrated_file_stat_shows_remote_modified_time() {
    let e = format!(
        r#"{{"path":"/a.txt","size":1,"mtime_ms":{LOCAL_MS},"hydrated":false,"folder":false,"remote_modified_ms":{REMOTE_MS},"pending_upload":false}}"#
    );
    assert_eq!(stat_mtimes_ms(&e, &["a.txt"]).await, vec![REMOTE_MS]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hydrated_file_stat_shows_local_time() {
    let e = format!(
        r#"{{"path":"/a.txt","size":1,"mtime_ms":{LOCAL_MS},"hydrated":true,"folder":false,"remote_modified_ms":{REMOTE_MS},"pending_upload":false}}"#
    );
    assert_eq!(stat_mtimes_ms(&e, &["a.txt"]).await, vec![LOCAL_MS]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_upload_file_stat_shows_local_time() {
    let e = format!(
        r#"{{"path":"/a.txt","size":1,"mtime_ms":{LOCAL_MS},"hydrated":false,"folder":false,"remote_modified_ms":{REMOTE_MS},"pending_upload":true}}"#
    );
    assert_eq!(stat_mtimes_ms(&e, &["a.txt"]).await, vec![LOCAL_MS]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn folder_stat_shows_remote_modified_time() {
    let e = format!(
        r#"{{"path":"/d","size":0,"mtime_ms":{LOCAL_MS},"hydrated":false,"folder":true,"remote_modified_ms":{REMOTE_MS},"pending_upload":false}}"#
    );
    assert_eq!(stat_mtimes_ms(&e, &["d"]).await, vec![REMOTE_MS]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply_without_remote_modified_time_falls_back_to_local_time() {
    // Older engine: no remote_modified_ms / pending_upload keys at all; one
    // entry carries an explicit null.
    let e = format!(
        r#"{{"path":"/a.txt","size":1,"mtime_ms":{LOCAL_MS},"hydrated":false,"folder":false}},{{"path":"/d","size":0,"mtime_ms":{LOCAL_MS},"hydrated":false,"folder":true,"remote_modified_ms":null}}"#
    );
    assert_eq!(stat_mtimes_ms(&e, &["a.txt", "d"]).await, vec![LOCAL_MS, LOCAL_MS]);
}

/// The stat mtime of `name` under `mount_path`, in milliseconds. Off the async
/// runtime: a stat on the mount itself is served by the FUSE session there.
async fn stat_ms(mount_path: std::path::PathBuf, name: &str) -> i64 {
    let name = name.to_string();
    tokio::task::spawn_blocking(move || {
        let m = std::fs::metadata(mount_path.join(name)).expect("stat");
        m.mtime() * 1000 + m.mtime_nsec() / 1_000_000
    })
    .await
    .unwrap()
}

/// Invariant: a write through the mount drops the provider's time for that
/// inode, so the next `stat` reports the local time again instead of the cloud
/// time from the last listing. This is the in-memory half of the fix — the
/// other tests pin the list-reply half, where the engine's own flags decide.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_through_the_mount_shows_local_time_again() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache = cache_dir.path().join("a.cache");
    std::fs::write(&cache, b"a").unwrap();
    let cache = cache.to_str().unwrap().to_string();

    let list = format!(
        r#"{{"ok":true,"entries":[{{"path":"/a.txt","size":1,"mtime_ms":{LOCAL_MS},"hydrated":false,"folder":false,"remote_modified_ms":{REMOTE_MS},"pending_upload":false}}]}}"#
    );
    let open = format!(r#"{{"ok":true,"cache_path":"{cache}"}}"#);
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list.as_str()),
        ("hydration.open_read", open.as_str()),
        ("hydration.open_write", open.as_str()),
        ("hydration.close_handle", r#"{"ok":true}"#),
    ]))
    .await;

    let ipc = ReconnectingIpcClient::connect(&jvm.socket_path).await.unwrap();
    let fs = UnidriveFs::new(Arc::new(Mutex::new(ipc)));
    let tempdir = tempfile::tempdir().unwrap();
    let mount_path = tempdir.path().to_path_buf();
    let mut opts = fuse3::MountOptions::default();
    opts.fs_name("unidrive-test").nonempty(false);
    let handle = fuse3::raw::Session::new(opts)
        .mount_with_unprivileged(fs, &mount_path)
        .await
        .expect("mount with unprivileged should succeed in FUSE-enabled env");
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(stat_ms(mount_path.clone(), "a.txt").await, REMOTE_MS);

    let mp = mount_path.clone();
    tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(mp.join("a.txt"))
            .expect("open a.txt for write");
        f.write_all(b"b").expect("write one byte");
        f.sync_all().expect("fsync");
    })
    .await
    .unwrap();

    let after = stat_ms(mount_path.clone(), "a.txt").await;
    let _ = handle.unmount().await;
    jvm.shutdown().await;
    assert_eq!(after, LOCAL_MS, "a write through the mount keeps the local time");
}

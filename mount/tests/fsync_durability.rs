//! FUSE fsync cloud-durability integration test.
//!
//! Mounts a `UnidriveFs` backed by a `FakeJvm`. fsync(2) on a dirty handle
//! must commit the bytes to the cloud (fire `hydration.open_write`) and
//! AWAIT the result before the syscall returns — giving an fsync-ing app a
//! real cloud-durability guarantee, not just a local-FD flush.
//!
//! Two orthogonal invariants, one test each:
//!   1. fsync_fires_open_write_and_awaits — success path: open_write is
//!      observed before fsync returns, dirty is cleared so the following
//!      RELEASE does NOT re-fire open_write but DOES fire close_handle.
//!   2. fsync_returns_eio_when_upload_fails — failure path: the JVM rejects
//!      open_write and the fsync(2) syscall surfaces the error to userland
//!      rather than swallowing it (the key behavioural difference from the
//!      silent RELEASE path).

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use unidrive_mount::fake_jvm::FakeJvm;
use unidrive_mount::fuse_fs::UnidriveFs;
use unidrive_mount::reconnect::ReconnectingIpcClient;

fn replies(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fsync_fires_open_write_and_awaits() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("foo.cache");
    {
        let mut f = std::fs::File::create(&cache_path).unwrap();
        f.write_all(b"").unwrap();
    }
    let cache_path_str = cache_path.to_str().unwrap();

    let open_write_begin_reply = format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let open_write_reply = format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let list_reply = r#"{"ok":true,"entries":[{"path":"/foo.txt","size":0,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#.to_string();

    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply.as_str()),
        ("hydration.open_write_begin", open_write_begin_reply.as_str()),
        ("hydration.open_write", open_write_reply.as_str()),
        ("hydration.close_handle", r#"{"ok":true}"#),
    ]))
    .await;

    let ipc = ReconnectingIpcClient::connect(&jvm.socket_path).await.unwrap();
    let fs = UnidriveFs::new(Arc::new(Mutex::new(ipc)));

    let tempdir = tempfile::tempdir().unwrap();
    let mount_path = tempdir.path().to_path_buf();

    let mut mount_options = fuse3::MountOptions::default();
    mount_options.fs_name("unidrive-test").nonempty(false);

    let mount_handle = fuse3::raw::Session::new(mount_options)
        .mount_with_unprivileged(fs, &mount_path)
        .await
        .expect("mount with unprivileged should succeed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Write then fsync, returning the still-OPEN file handle so RELEASE has
    // not happened yet. The moment this blocking task returns, fsync(2) has
    // returned to userland — so a traffic snapshot taken here must already
    // contain open_write, proving fsync awaited the upload rather than
    // deferring it to RELEASE.
    let mp = mount_path.clone();
    let open_file = tokio::task::spawn_blocking(move || {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(mp.join("foo.txt"))
            .expect("open foo.txt for write");
        f.write_all(b"hi\n").expect("write hi");
        f.sync_all().expect("fsync must succeed");
        f // keep open: RELEASE deferred until we drop it below
    })
    .await
    .unwrap();

    let traffic_at_fsync_return = jvm.recorded_requests().await;
    assert!(
        traffic_at_fsync_return
            .iter()
            .any(|r| r.contains(r#""verb":"hydration.open_write""#)),
        "open_write must be observed BEFORE fsync(2) returned: {traffic_at_fsync_return:?}"
    );

    // Now drop the handle -> FUSE RELEASE. Do it on a blocking thread; the
    // kernel close path can block on the FUSE event loop.
    tokio::task::spawn_blocking(move || drop(open_file))
        .await
        .unwrap();

    // Let RELEASE drain.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    // dirty was cleared by fsync, so the post-fsync RELEASE must NOT re-fire
    // open_write — exactly one open_write total (the fsync one).
    let open_write_count = recorded
        .iter()
        .filter(|r| r.contains(r#""verb":"hydration.open_write""#))
        .count();
    assert_eq!(
        open_write_count, 1,
        "expected exactly one open_write (fsync's); RELEASE must not re-upload: {recorded:?}"
    );

    // close_handle MUST still fire at RELEASE regardless of dirty state —
    // the JVM open-set entry needs releasing.
    let close_count = recorded
        .iter()
        .filter(|r| r.contains(r#""verb":"hydration.close_handle""#))
        .count();
    assert_eq!(
        close_count, 1,
        "RELEASE must fire close_handle even when dirty was cleared by fsync: {recorded:?}"
    );

    let cache_bytes = std::fs::read(&cache_path).expect("read cache after write");
    assert_eq!(cache_bytes, b"hi\n", "cache file content mismatch");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fsync_returns_eio_when_upload_fails() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("foo.cache");
    {
        let mut f = std::fs::File::create(&cache_path).unwrap();
        f.write_all(b"").unwrap();
    }
    let cache_path_str = cache_path.to_str().unwrap();

    let open_write_begin_reply = format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let list_reply = r#"{"ok":true,"entries":[{"path":"/foo.txt","size":0,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#.to_string();

    // The JVM rejects the upload. fsync must surface this, not swallow it.
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply.as_str()),
        ("hydration.open_write_begin", open_write_begin_reply.as_str()),
        ("hydration.open_write", r#"{"ok":false,"error":"upload_failed"}"#),
        ("hydration.close_handle", r#"{"ok":true}"#),
    ]))
    .await;

    let ipc = ReconnectingIpcClient::connect(&jvm.socket_path).await.unwrap();
    let fs = UnidriveFs::new(Arc::new(Mutex::new(ipc)));

    let tempdir = tempfile::tempdir().unwrap();
    let mount_path = tempdir.path().to_path_buf();

    let mut mount_options = fuse3::MountOptions::default();
    mount_options.fs_name("unidrive-test").nonempty(false);

    let mount_handle = fuse3::raw::Session::new(mount_options)
        .mount_with_unprivileged(fs, &mount_path)
        .await
        .expect("mount with unprivileged should succeed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mp = mount_path.clone();
    let fsync_result: std::io::Result<()> = tokio::task::spawn_blocking(move || {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(mp.join("foo.txt"))?;
        f.write_all(b"hi\n")?;
        f.sync_all() // this is the fsync(2) under test
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    assert!(
        fsync_result.is_err(),
        "fsync(2) must return an error to userland when the cloud upload fails, not silently succeed"
    );
}

//! FUSE write-through integration test.
//!
//! Mounts a `UnidriveFs` backed by a `FakeJvm`. Pre-populates a cache file
//! the JVM hands back from `open_write_begin` (O_TRUNC open). Opens the
//! mount and writes new bytes via shell redirection
//! (`echo hi > <mount>/foo.txt`). On FUSE RELEASE the dirty handle must
//! issue `hydration.open_write` with the cache_path BEFORE
//! `hydration.close_handle`, and the cache file must contain the new bytes.
//!
//! Load-bearing per the Phase 2 plan: this verifies the spec's
//! "open_write IPC at FUSE RELEASE is the ONLY write-trigger" invariant.
//! After B2, write-opens with O_TRUNC route through open_write_begin (no
//! download); write-opens without O_TRUNC still use open_read.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use support::fake_jvm::{replies, FakeJvm};
use unidrive_mount::fuse_fs::UnidriveFs;
use unidrive_mount::reconnect::ReconnectingIpcClient;
mod support;


#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dirty_release_fires_open_write_then_close_handle() {
    // Pre-populate a cache file the JVM hands back from open_write_begin
    // (O_TRUNC open — no download). Start empty (size=0) so the kernel's
    // write goes through cleanly.
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

    let mp = mount_path.clone();
    tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(mp.join("foo.txt"))
            .expect("open foo.txt for write");
        f.write_all(b"hi\n").expect("write hi");
        f.sync_all().expect("fsync");
        // drop closes -> FUSE RELEASE
    })
    .await
    .unwrap();

    // Give the kernel time to issue RELEASE and the IPC client to drain.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    // Assert cache file holds the new bytes.
    let cache_bytes = std::fs::read(&cache_path).expect("read cache after write");
    assert_eq!(cache_bytes, b"hi\n", "cache file content mismatch");

    // open_write must appear in the recorded traffic and must precede
    // close_handle.
    let open_write_idx = recorded
        .iter()
        .position(|r| r.contains(r#""verb":"hydration.open_write""#))
        .unwrap_or_else(|| panic!("expected hydration.open_write in recorded: {recorded:?}"));
    let close_idx = recorded
        .iter()
        .position(|r| r.contains(r#""verb":"hydration.close_handle""#))
        .unwrap_or_else(|| panic!("expected hydration.close_handle in recorded: {recorded:?}"));
    assert!(
        open_write_idx < close_idx,
        "open_write must precede close_handle. recorded={recorded:?}"
    );

    // The open_write request must carry remote path and cache_path.
    let req = &recorded[open_write_idx];
    assert!(req.contains(r#""path":"/foo.txt""#), "open_write missing path: {req}");
    assert!(
        req.contains(&format!(r#""cache_path":"{cache_path_str}""#)),
        "open_write missing cache_path: {req}"
    );
}

/// Invariant: a writable open WITHOUT O_TRUNC routes through `hydration.open_read`
/// (JVM downloads the existing content), NOT through `hydration.open_write_begin`.
/// This pins the routing branch in the open handler: O_TRUNC → open_write_begin;
/// everything else writable → open_read. If this test is removed or weakened the
/// invariant silently regresses and non-truncating writes would bypass hydration.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_truncating_write_open_uses_open_read() {
    // Pre-populate a cache file the JVM hands back from open_read (existing
    // content — the JVM has already downloaded it).
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("bar.cache");
    {
        let mut f = std::fs::File::create(&cache_path).unwrap();
        f.write_all(b"original\n").unwrap();
    }
    let cache_path_str = cache_path.to_str().unwrap();

    let open_read_reply = format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let open_write_reply = format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let list_reply = r#"{"ok":true,"entries":[{"path":"/bar.txt","size":9,"mtime_ms":1000000,"hydrated":true,"folder":false}]}"#.to_string();

    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply.as_str()),
        ("hydration.open_read", open_read_reply.as_str()),
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

    let mp = mount_path.clone();
    tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        // O_WRONLY without O_TRUNC — must route through open_read.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            // explicitly NOT .truncate(true)
            .open(mp.join("bar.txt"))
            .expect("open bar.txt for write without truncate");
        f.write_all(b"x").expect("write one byte");
        // drop closes -> FUSE RELEASE
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    // Core assertion: open_read must have fired, open_write_begin must NOT have.
    assert!(
        recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_read""#)),
        "expected hydration.open_read for non-truncating write open: {recorded:?}"
    );
    assert!(
        !recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_write_begin""#)),
        "hydration.open_write_begin must NOT fire for non-truncating write open: {recorded:?}"
    );

    // close_handle fires unconditionally (benign no-op path not exercised here —
    // open_read did register a JVM open-set entry, so this is a real release).
    assert!(
        recorded.iter().any(|r| r.contains(r#""verb":"hydration.close_handle""#)),
        "expected hydration.close_handle at RELEASE: {recorded:?}"
    );
}

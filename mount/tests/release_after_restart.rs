//! An edit whose release meets a daemon that restarted is still uploaded.
//!
//! The request connection is dead after the daemon's restart, so the first
//! `open_write` at release failed with an I/O error; it was never sent again,
//! the edit sat in the cache until the next mount start, and the `close_handle`
//! that followed was the only call that reconnected.
//!
//! Requires a FUSE-enabled environment, like `write_through.rs`.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use support::fake_jvm::{replies, FakeJvm};
use tokio::sync::Mutex;
use unidrive_mount::fuse_fs::UnidriveFs;
use unidrive_mount::reconnect::ReconnectingIpcClient;
mod support;

fn engine_replies(cache: &std::path::Path) -> std::collections::HashMap<String, String> {
    let cache_reply = format!(r#"{{"ok":true,"cache_path":"{}"}}"#, cache.display());
    replies(&[
        (
            "hydration.list",
            r#"{"ok":true,"entries":[{"path":"/a.txt","size":3,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#,
        ),
        ("hydration.open_read", cache_reply.as_str()),
        ("hydration.open_write", cache_reply.as_str()),
        ("hydration.close_handle", r#"{"ok":true}"#),
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dirty_release_after_a_daemon_restart_still_uploads() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache = cache_dir.path().join("a.cache");
    std::fs::write(&cache, b"abc").unwrap();
    let socket_dir = tempfile::tempdir().unwrap();
    let socket = socket_dir.path().join("ipc.sock");

    let v1 = FakeJvm::spawn_at(socket.clone(), engine_replies(&cache)).await;
    let ipc = ReconnectingIpcClient::connect_with(&socket, Duration::from_millis(20), Duration::from_secs(10))
        .await
        .unwrap();
    let fs = UnidriveFs::new(Arc::new(Mutex::new(ipc)));
    let mount_dir = tempfile::tempdir().unwrap();
    let mp = mount_dir.path().to_path_buf();
    let mut opts = fuse3::MountOptions::default();
    opts.fs_name("unidrive-test").nonempty(false);
    let mount = fuse3::raw::Session::new(opts)
        .mount_with_unprivileged(fs, &mp)
        .await
        .expect("mount with unprivileged should succeed in FUSE-enabled env");
    tokio::time::sleep(Duration::from_millis(100)).await;

    // An edit in progress: the file is open and written.
    let path = mp.join("a.txt");
    let file = tokio::task::spawn_blocking(move || {
        let mut f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.write_all(b"x").unwrap();
        f
    })
    .await
    .unwrap();

    // The daemon restarts: every connection to the old one is gone.
    v1.shutdown().await;
    let v2 = FakeJvm::spawn_at(socket.clone(), engine_replies(&cache)).await;

    tokio::task::spawn_blocking(move || drop(file)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let recorded = v2.recorded_requests().await;
    let _ = mount.unmount().await;
    v2.shutdown().await;
    assert!(
        recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_write""#) && r.contains(r#""path":"/a.txt""#)),
        "the restarted daemon never got the upload: {recorded:?}"
    );
}

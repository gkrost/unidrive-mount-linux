//! Read-open failure path.
//!
//! Reproduces the diagnosed read-path EIO: the JVM `open_read` drives a
//! hydrate-on-cache-miss download; when that download fails (expired auth,
//! 404, network), the JVM replies `{"ok":false,"error":"<message>"}`. The
//! co-daemon must surface that to userspace as EIO — and, since the fix, log
//! the IpcError before mapping so the EIO is diagnosable rather than silent.
//!
//! This test pins the user-visible invariant (read-open of a file whose
//! hydration failed returns EIO, not a panic / hang / wrong errno). The log
//! line is exercised on this path but not asserted here (no capturing
//! subscriber); its presence is guarded by the source containing the
//! `tracing::warn!(... "open_read failed")` call on the read-open branch.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use support::fake_jvm::{replies, FakeJvm};
use unidrive_mount::fuse_fs::UnidriveFs;
use unidrive_mount::reconnect::ReconnectingIpcClient;
mod support;


#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_open_returns_eio_when_open_read_download_fails() {
    // FakeJvm lists the file (so it shows up in readdir/getattr) but fails the
    // hydrate-on-read with a server error, mirroring a failed OneDrive download.
    let list_reply = r#"{"ok":true,"entries":[{"path":"/WTF.kdbx","size":722309,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#;
    let open_read_err = r#"{"ok":false,"error":"download failed: 404 Not Found"}"#;

    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply),
        ("hydration.open_read", open_read_err),
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
    let err = tokio::task::spawn_blocking(move || {
        // open() for read drives open_read, which the fake fails.
        std::fs::File::open(mp.join("WTF.kdbx"))
    })
    .await
    .unwrap()
    .expect_err("opening a file whose hydration fails must error, not succeed");

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    // EIO is the contract for a generic IpcError::ServerError on the read path.
    assert_eq!(
        err.raw_os_error(),
        Some(libc::EIO),
        "expected EIO for failed open_read download, got {err:?}"
    );

    // The JVM must have actually seen the open_read attempt.
    assert!(
        recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_read""#)),
        "expected hydration.open_read in recorded requests: {recorded:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_read_not_found_maps_to_enoent() {
    // A read that is still not-found after the JVM-side download re-resolve means
    // the remote item is genuinely gone. The JVM serialises that as the stable
    // typed token `not_found`; the co-daemon must map it to ENOENT, not the
    // catch-all EIO, so userspace sees "no such file" rather than "I/O error".
    let list_reply = r#"{"ok":true,"entries":[{"path":"/gone.kdbx","size":722309,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#;
    let open_read_err = r#"{"ok":false,"error":"not_found"}"#;

    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply),
        ("hydration.open_read", open_read_err),
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
    let err = tokio::task::spawn_blocking(move || std::fs::File::open(mp.join("gone.kdbx")))
        .await
        .unwrap()
        .expect_err("opening a genuinely-gone file must error, not succeed");

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    // ENOENT is the contract for the `not_found` token on the read path.
    assert_eq!(
        err.raw_os_error(),
        Some(libc::ENOENT),
        "expected ENOENT for a not_found open_read, got {err:?}"
    );

    assert!(
        recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_read""#)),
        "expected hydration.open_read in recorded requests: {recorded:?}"
    );
}

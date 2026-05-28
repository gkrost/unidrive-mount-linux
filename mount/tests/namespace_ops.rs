//! T4 + T5 from unidrive/docs/dev/specs/hydration-namespace-verbs-design.md §3.9,
//! plus the R3 follow-up (parent_not_found -> ENOENT).
//!
//! T4 pins the happy path: a {"ok":true} reply from the JVM yields exit 0
//! from the FUSE mkdir op.
//! T5 pins the errno mapping: a {"ok":false,"error":"not_empty"} reply from
//! the JVM yields ENOTEMPTY at the kernel boundary.
//! The mkdir/parent-not-found test pins the spec R3 closure: a
//! {"ok":false,"error":"parent_not_found"} reply from the JVM yields ENOENT
//! at the kernel boundary, matching POSIX mkdir(2) semantics.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use unidrive_mount::fake_jvm::{replies, FakeJvm};
use unidrive_mount::fuse_fs::UnidriveFs;
use unidrive_mount::reconnect::ReconnectingIpcClient;


/// Base set of canned replies required for the kernel to resolve the root
/// inode and allow namespace operations on it.
fn base_replies() -> Vec<(&'static str, &'static str)> {
    vec![
        ("hydration.list", r#"{"ok":true,"entries":[]}"#),
        ("hydration.close_handle", r#"{"ok":true}"#),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mkdir_round_trip_returns_zero_on_jvm_ok() {
    let jvm = FakeJvm::spawn(replies(
        &[
            base_replies(),
            vec![("hydration.mkdir", r#"{"ok":true}"#)],
        ]
        .concat(),
    ))
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
        .expect("mount should succeed");

    tokio::time::sleep(Duration::from_millis(200)).await;

    let mp = mount_path.clone();
    let mkdir_result = tokio::task::spawn_blocking(move || {
        std::fs::create_dir(mp.join("newfolder"))
    })
    .await
    .unwrap();

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    assert!(mkdir_result.is_ok(), "mkdir(newfolder) must succeed when JVM replies ok:true");

    // Verify the JVM saw a hydration.mkdir request with the path.
    let mkdir_req = recorded.iter().find(|r| r.contains(r#""verb":"hydration.mkdir""#));
    assert!(mkdir_req.is_some(), "expected hydration.mkdir in recorded requests; got: {recorded:?}");
    let req = mkdir_req.unwrap();
    assert!(req.contains(r#""path":"/newfolder""#), "mkdir path mismatch: {req}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rmdir_returns_enotempty_when_jvm_signals_not_empty() {
    // Provide a list reply that shows "full" as an existing folder so the
    // kernel can look it up and issue rmdir on it.
    let list_reply = r#"{"ok":true,"entries":[{"path":"/full","size":0,"mtime_ms":1000000,"hydrated":false,"folder":true}]}"#;
    let jvm = FakeJvm::spawn(replies(
        &[
            vec![("hydration.list", list_reply)],
            vec![("hydration.close_handle", r#"{"ok":true}"#)],
            vec![("hydration.rmdir", r#"{"ok":false,"error":"not_empty"}"#)],
        ]
        .concat(),
    ))
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
        .expect("mount should succeed");

    tokio::time::sleep(Duration::from_millis(200)).await;

    let mp = mount_path.clone();
    let rmdir_result = tokio::task::spawn_blocking(move || {
        std::fs::remove_dir(mp.join("full"))
    })
    .await
    .unwrap();

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    assert!(rmdir_result.is_err(), "rmdir of non-empty dir must fail");
    let err = rmdir_result.unwrap_err();
    // Linux strerror(ENOTEMPTY) is "Directory not empty".
    let err_str = err.to_string().to_lowercase();
    assert!(
        err_str.contains("directory not empty"),
        "rmdir must surface ENOTEMPTY ('Directory not empty'); got: {err}"
    );

    let rmdir_req = recorded.iter().find(|r| r.contains(r#""verb":"hydration.rmdir""#));
    assert!(rmdir_req.is_some(), "expected hydration.rmdir in recorded requests; got: {recorded:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mkdir_returns_enoent_when_jvm_signals_parent_not_found() {
    let jvm = FakeJvm::spawn(replies(
        &[
            base_replies(),
            vec![("hydration.mkdir", r#"{"ok":false,"error":"parent_not_found"}"#)],
        ]
        .concat(),
    ))
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
        .expect("mount should succeed");

    tokio::time::sleep(Duration::from_millis(200)).await;

    let mp = mount_path.clone();
    let mkdir_result = tokio::task::spawn_blocking(move || {
        std::fs::create_dir(mp.join("orphan"))
    })
    .await
    .unwrap();

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    assert!(mkdir_result.is_err(), "mkdir under missing parent must fail");
    let err = mkdir_result.unwrap_err();
    // ENOENT surfaces as ErrorKind::NotFound; strerror is "No such file or directory".
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::NotFound,
        "mkdir must surface ENOENT (NotFound) when JVM signals parent_not_found; got: {err}"
    );

    let mkdir_req = recorded.iter().find(|r| r.contains(r#""verb":"hydration.mkdir""#));
    assert!(mkdir_req.is_some(), "expected hydration.mkdir in recorded requests; got: {recorded:?}");
}

//! Integration tests for the FUSE rename op.
//!
//! Round-trip happy path pins that a {"ok":true} reply from the JVM
//! `hydration.rename` verb yields exit 0 from the FUSE rename op.
//! The two errno-mapping tests pin the wire-error -> kernel-errno
//! translation: {"error":"new_path_exists"} -> EEXIST, and
//! {"error":"old_path_not_found"} -> ENOENT.
//! `mv_preserves_inode_via_pathmap` pins the POSIX inode-preservation
//! invariant — the kernel observes the same inode for the rename
//! source and the rename destination, mediated by `PathMap::rename`.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
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
async fn rename_round_trip_returns_zero_on_jvm_ok() {
    // List reply must show "a.txt" so the kernel can resolve it for rename.
    let list_reply = r#"{"ok":true,"entries":[{"path":"/a.txt","size":3,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#;
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply),
        ("hydration.close_handle", r#"{"ok":true}"#),
        ("hydration.rename", r#"{"ok":true}"#),
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
        .expect("mount should succeed");

    tokio::time::sleep(Duration::from_millis(200)).await;

    let mp = mount_path.clone();
    let rename_result = tokio::task::spawn_blocking(move || {
        std::fs::rename(mp.join("a.txt"), mp.join("b.txt"))
    })
    .await
    .unwrap();

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    assert!(rename_result.is_ok(), "rename(a.txt -> b.txt) must succeed when JVM replies ok:true; got: {rename_result:?}");

    let rename_req = recorded.iter().find(|r| r.contains(r#""verb":"hydration.rename""#));
    assert!(rename_req.is_some(), "expected hydration.rename in recorded requests; got: {recorded:?}");
    let req = rename_req.unwrap();
    assert!(req.contains(r#""old_path":"/a.txt""#), "rename old_path mismatch: {req}");
    assert!(req.contains(r#""new_path":"/b.txt""#), "rename new_path mismatch: {req}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_returns_eexist_when_jvm_signals_new_path_exists() {
    // Show two files in the listing so the kernel can resolve both source and dest.
    let list_reply = r#"{"ok":true,"entries":[{"path":"/a.txt","size":3,"mtime_ms":1000000,"hydrated":false,"folder":false},{"path":"/b.txt","size":3,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#;
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply),
        ("hydration.close_handle", r#"{"ok":true}"#),
        ("hydration.rename", r#"{"ok":false,"error":"new_path_exists"}"#),
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
        .expect("mount should succeed");

    tokio::time::sleep(Duration::from_millis(200)).await;

    let mp = mount_path.clone();
    // Use renameat2 with RENAME_NOREPLACE so the kernel routes straight to
    // our FUSE rename without first stat-then-replace dance. std::fs::rename
    // on Linux uses renameat2 with no flags, which would let the kernel
    // attempt to overlay; we want to verify the JVM's refusal propagates.
    // Plain rename still goes through our FUSE op — the kernel does not
    // unlink the destination on our behalf for FUSE filesystems.
    let rename_result = tokio::task::spawn_blocking(move || {
        std::fs::rename(mp.join("a.txt"), mp.join("b.txt"))
    })
    .await
    .unwrap();

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    assert!(rename_result.is_err(), "rename onto existing path must fail when JVM signals new_path_exists");
    let err = rename_result.unwrap_err();
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::AlreadyExists,
        "rename must surface EEXIST (AlreadyExists) when JVM signals new_path_exists; got: {err}"
    );

    let rename_req = recorded.iter().find(|r| r.contains(r#""verb":"hydration.rename""#));
    assert!(rename_req.is_some(), "expected hydration.rename in recorded requests; got: {recorded:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_returns_enoent_when_jvm_signals_old_path_not_found() {
    // List shows "ghost.txt" so the kernel can address it, but the JVM
    // signals old_path_not_found (e.g. a sync deleted the row between the
    // kernel's stat and our IPC call).
    let list_reply = r#"{"ok":true,"entries":[{"path":"/ghost.txt","size":3,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#;
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply),
        ("hydration.close_handle", r#"{"ok":true}"#),
        ("hydration.rename", r#"{"ok":false,"error":"old_path_not_found"}"#),
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
        .expect("mount should succeed");

    tokio::time::sleep(Duration::from_millis(200)).await;

    let mp = mount_path.clone();
    let rename_result = tokio::task::spawn_blocking(move || {
        std::fs::rename(mp.join("ghost.txt"), mp.join("b.txt"))
    })
    .await
    .unwrap();

    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    assert!(rename_result.is_err(), "rename of vanished source must fail");
    let err = rename_result.unwrap_err();
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::NotFound,
        "rename must surface ENOENT (NotFound) when JVM signals old_path_not_found; got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mv_preserves_inode_via_pathmap() {
    // POSIX semantics: the inode of the renamed file is preserved across
    // the rename. The kernel observes the same inode number before and
    // after, mediated by PathMap::rename. This test wires the JVM to
    // happy-path the rename, then calls stat() before and after to
    // compare the inode numbers the kernel reports.
    let list_reply = r#"{"ok":true,"entries":[{"path":"/src","size":42,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#;
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply),
        ("hydration.close_handle", r#"{"ok":true}"#),
        ("hydration.rename", r#"{"ok":true}"#),
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
        .expect("mount should succeed");

    tokio::time::sleep(Duration::from_millis(200)).await;

    let mp = mount_path.clone();
    let (src_inode, dst_inode) = tokio::task::spawn_blocking(move || {
        let src = std::fs::metadata(mp.join("src")).expect("stat src");
        let src_ino = src.ino();
        std::fs::rename(mp.join("src"), mp.join("dst")).expect("rename");
        let dst = std::fs::metadata(mp.join("dst")).expect("stat dst");
        let dst_ino = dst.ino();
        (src_ino, dst_ino)
    })
    .await
    .unwrap();

    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    assert_eq!(
        src_inode, dst_inode,
        "POSIX rename must preserve inode: src ino {src_inode} != dst ino {dst_inode}",
    );
}

//! Cache-file eviction on FUSE unlink/rmdir.
//!
//! Per `../unidrive/docs/dev/specs/hydration-namespace-verbs-design.md` §4 R5 / NG4:
//! when a hydrated path is deleted via the mount, the cache copy at
//! `<cache_root>/<rel>` should be evicted best-effort. NotFound is silent
//! (a never-hydrated path is harmless); other I/O errors log but don't fail
//! the user's `rm`.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use unidrive_mount::fake_jvm::{replies, FakeJvm};
use unidrive_mount::fuse_fs::UnidriveFs;
use unidrive_mount::reconnect::ReconnectingIpcClient;


#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unlink_removes_hydrated_cache_file() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_file = cache_dir.path().join("foo.txt");
    std::fs::write(&cache_file, b"hydrated bytes").unwrap();

    let list_reply = r#"{"ok":true,"entries":[{"path":"/foo.txt","size":14,"mtime_ms":1000000,"hydrated":true,"folder":false}]}"#;
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply),
        ("hydration.close_handle", r#"{"ok":true}"#),
        ("hydration.unlink", r#"{"ok":true}"#),
    ]))
    .await;

    let ipc = ReconnectingIpcClient::connect(&jvm.socket_path).await.unwrap();
    let fs = UnidriveFs::new(Arc::new(Mutex::new(ipc)))
        .with_cache_root(cache_dir.path().to_path_buf());

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
    let rm_result = tokio::task::spawn_blocking(move || {
        std::fs::remove_file(mp.join("foo.txt"))
    })
    .await
    .unwrap();

    // Snapshot the cache-file presence BEFORE unmount so any async cleanup
    // can't paper over a failure.
    let cache_file_after = cache_file.exists();

    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    assert!(rm_result.is_ok(), "rm /foo.txt must succeed: {rm_result:?}");
    assert!(
        !cache_file_after,
        "cache file at {cache_file:?} must be evicted after successful unlink"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unlink_tolerates_missing_cache_file() {
    let cache_dir = tempfile::tempdir().unwrap();
    // Deliberately DO NOT create <cache>/foo.txt — simulate a never-hydrated
    // path. The unlink must still succeed.

    let list_reply = r#"{"ok":true,"entries":[{"path":"/foo.txt","size":0,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#;
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply),
        ("hydration.close_handle", r#"{"ok":true}"#),
        ("hydration.unlink", r#"{"ok":true}"#),
    ]))
    .await;

    let ipc = ReconnectingIpcClient::connect(&jvm.socket_path).await.unwrap();
    let fs = UnidriveFs::new(Arc::new(Mutex::new(ipc)))
        .with_cache_root(cache_dir.path().to_path_buf());

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
    let rm_result = tokio::task::spawn_blocking(move || {
        std::fs::remove_file(mp.join("foo.txt"))
    })
    .await
    .unwrap();

    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    assert!(
        rm_result.is_ok(),
        "rm of never-hydrated path must succeed; NotFound on cache eviction must not surface as EIO. got: {rm_result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rmdir_removes_hydrated_cache_directory() {
    let cache_dir = tempfile::tempdir().unwrap();
    let folder_cache = cache_dir.path().join("folder");
    std::fs::create_dir(&folder_cache).unwrap();
    std::fs::write(folder_cache.join("child.bin"), b"hydrated child").unwrap();

    let list_reply = r#"{"ok":true,"entries":[{"path":"/folder","size":0,"mtime_ms":1000000,"hydrated":false,"folder":true}]}"#;
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply),
        ("hydration.close_handle", r#"{"ok":true}"#),
        ("hydration.rmdir", r#"{"ok":true}"#),
    ]))
    .await;

    let ipc = ReconnectingIpcClient::connect(&jvm.socket_path).await.unwrap();
    let fs = UnidriveFs::new(Arc::new(Mutex::new(ipc)))
        .with_cache_root(cache_dir.path().to_path_buf());

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
        std::fs::remove_dir(mp.join("folder"))
    })
    .await
    .unwrap();

    let folder_after = folder_cache.exists();

    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    assert!(rmdir_result.is_ok(), "rmdir /folder must succeed: {rmdir_result:?}");
    assert!(
        !folder_after,
        "cache directory at {folder_cache:?} (and its hydrated children) must be evicted after successful rmdir"
    );
}

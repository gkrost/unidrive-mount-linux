//! A directory rename takes everything below it along.
//!
//! `PathMap::rename` moved only the renamed entry, so a child's inode kept
//! naming the old path, and an open handle below the directory kept its old
//! remote path (and the cache path the engine had moved): the upload at release
//! named a path the engine no longer knew. Requires a FUSE-enabled environment.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use support::fake_jvm::{replies, FakeJvm};
use tokio::sync::Mutex;
use unidrive_mount::fuse_fs::UnidriveFs;
use unidrive_mount::reconnect::ReconnectingIpcClient;
mod support;

const LIST: &str = r#"{"ok":true,"entries":[{"path":"/d","size":0,"mtime_ms":1000000,"hydrated":false,"folder":true},{"path":"/d/a.txt","size":3,"mtime_ms":1000000,"hydrated":false,"folder":false},{"path":"/f.txt","size":3,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#;

/// A cache file laid out like the remote path below the cache folder.
fn cache_for(dir: &Path, rel: &str) -> PathBuf {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::File::create(&path).unwrap().write_all(b"abc").unwrap();
    path
}

/// A mount on a fake engine that lists `LIST` for every prefix and hands out
/// `cache` as the cache path of every open.
async fn mounted(cache: &Path) -> (FakeJvm, fuse3::raw::MountHandle, PathBuf, tempfile::TempDir) {
    let open = format!(r#"{{"ok":true,"cache_path":"{}"}}"#, cache.display());
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", LIST),
        ("hydration.open_read", open.as_str()),
        ("hydration.open_write", open.as_str()),
        ("hydration.close_handle", r#"{"ok":true}"#),
        ("hydration.rename", r#"{"ok":true}"#),
    ]))
    .await;
    let ipc = ReconnectingIpcClient::connect(&jvm.socket_path).await.unwrap();
    let fs = UnidriveFs::new(Arc::new(Mutex::new(ipc)));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let mut opts = fuse3::MountOptions::default();
    opts.fs_name("unidrive-test").nonempty(false);
    let handle = fuse3::raw::Session::new(opts)
        .mount_with_unprivileged(fs, &path)
        .await
        .expect("mount with unprivileged should succeed in FUSE-enabled env");
    tokio::time::sleep(Duration::from_millis(100)).await;
    (jvm, handle, path, dir)
}

fn open_write_request(recorded: &[String]) -> &String {
    recorded
        .iter()
        .find(|r| r.contains(r#""verb":"hydration.open_write""#))
        .unwrap_or_else(|| panic!("no open_write at release: {recorded:?}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_of_a_renamed_directory_is_opened_at_its_new_path() {
    let cache = tempfile::tempdir().unwrap();
    let a = cache_for(cache.path(), "d/a.txt");
    let (jvm, mount, mp, _dir) = mounted(&a).await;

    // The kernel keeps the child's dentry across the rename, so the open comes
    // in with the child's inode: the path it is handed to the engine under is
    // the one the path map holds for it.
    let body = tokio::task::spawn_blocking(move || {
        assert!(std::fs::metadata(mp.join("d").join("a.txt")).is_ok());
        std::fs::rename(mp.join("d"), mp.join("e")).expect("rename d to e");
        std::fs::read(mp.join("e").join("a.txt"))
    })
    .await
    .unwrap();

    let recorded = jvm.recorded_requests().await;
    let _ = mount.unmount().await;
    jvm.shutdown().await;
    assert!(body.is_ok(), "reading e/a.txt failed: {body:?}");
    let open = recorded
        .iter()
        .find(|r| r.contains(r#""verb":"hydration.open_read""#))
        .unwrap_or_else(|| panic!("no open_read: {recorded:?}"));
    assert!(open.contains(r#""path":"/e/a.txt""#), "open_read names the old path: {open}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handle_open_below_a_renamed_directory_uploads_to_the_new_path() {
    let cache = tempfile::tempdir().unwrap();
    let a = cache_for(cache.path(), "d/a.txt");
    let (jvm, mount, mp, _dir) = mounted(&a).await;

    tokio::task::spawn_blocking(move || {
        let mut f = std::fs::OpenOptions::new().write(true).open(mp.join("d").join("a.txt")).unwrap();
        std::fs::rename(mp.join("d"), mp.join("e")).expect("rename d to e");
        f.write_all(b"x").unwrap();
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let recorded = jvm.recorded_requests().await;
    let _ = mount.unmount().await;
    jvm.shutdown().await;
    let upload = open_write_request(&recorded);
    assert!(upload.contains(r#""path":"/e/a.txt""#), "upload names the old path: {upload}");
    assert!(upload.contains(r#"/e/a.txt""#) && !upload.contains("/d/a.txt"), "the cache path must follow: {upload}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handle_open_on_a_renamed_file_uploads_to_the_new_path() {
    let cache = tempfile::tempdir().unwrap();
    let f = cache_for(cache.path(), "f.txt");
    let (jvm, mount, mp, _dir) = mounted(&f).await;

    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::OpenOptions::new().write(true).open(mp.join("f.txt")).unwrap();
        std::fs::rename(mp.join("f.txt"), mp.join("g.txt")).expect("rename f to g");
        file.write_all(b"x").unwrap();
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let recorded = jvm.recorded_requests().await;
    let _ = mount.unmount().await;
    jvm.shutdown().await;
    let upload = open_write_request(&recorded);
    assert!(upload.contains(r#""path":"/g.txt""#), "upload names the old path: {upload}");
    assert!(upload.contains(r#"/g.txt""#) && !upload.contains("/f.txt"), "the cache path must follow: {upload}");
}

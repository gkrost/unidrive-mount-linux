//! Cold-read integration test.
//!
//! FakeJvm serves a `hydration.list` reply with one file, and a
//! `hydration.open_read` reply pointing at a tempfile with "hello" in it.
//! `cat <mount>/foo.txt` returns "hello" and we assert the JVM saw
//! `open_read` followed by `close_handle` (the latter from FUSE RELEASE).

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use unidrive_mount::fake_jvm::FakeJvm;
use unidrive_mount::fuse_fs::UnidriveFs;
use unidrive_mount::ipc::IpcClient;

fn replies(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cat_returns_cache_file_bytes_and_release_closes_handle() {
    // Set up the cache-file the FakeJvm will hand us back from open_read.
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("foo.cache");
    {
        let mut f = std::fs::File::create(&cache_path).unwrap();
        f.write_all(b"hello").unwrap();
    }
    let cache_path_str = cache_path.to_str().unwrap();

    let open_read_reply = format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let list_reply = r#"{"ok":true,"entries":[{"path":"/foo.txt","size":5,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#.to_string();

    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply.as_str()),
        ("hydration.open_read", open_read_reply.as_str()),
        ("hydration.close_handle", r#"{"ok":true}"#),
    ]))
    .await;

    let ipc = IpcClient::connect(&jvm.socket_path).await.unwrap();
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
    let body = tokio::task::spawn_blocking(move || {
        std::fs::read(mp.join("foo.txt"))
    })
    .await
    .unwrap()
    .expect("read foo.txt");

    // Give the kernel a tick to issue RELEASE.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    assert_eq!(body, b"hello", "file contents mismatch");

    // Order check: open_read must come before close_handle. There may be
    // intermediate list/getattr/etc. calls; we just assert relative order.
    let open_idx = recorded
        .iter()
        .position(|r| r.contains(r#""verb":"hydration.open_read""#))
        .expect("expected hydration.open_read in recorded requests");
    let close_idx = recorded
        .iter()
        .position(|r| r.contains(r#""verb":"hydration.close_handle""#))
        .expect("expected hydration.close_handle in recorded requests");
    assert!(
        open_idx < close_idx,
        "open_read must precede close_handle. recorded={recorded:?}"
    );

    // The open_read request must carry the remote path "/foo.txt".
    assert!(
        recorded[open_idx].contains(r#""path":"/foo.txt""#),
        "open_read missing path field: {}",
        recorded[open_idx]
    );
}

//! FUSE create/mknod integration tests.
//!
//! Pins the round-trip from a kernel-issued `open(O_CREAT)` (or shell `>` /
//! `touch`) through `hydration.create` and back. Three invariants:
//!
//! 1. Happy path: the JVM's {cache_path, handle_id} reply yields a writeable
//!    file in the mount, and the create_internal handle is tagged with the
//!    JVM-returned handle_id (so the upload-on-RELEASE path fires under the
//!    same id the JVM tracks).
//! 2. `path_exists` wire error round-trips to EEXIST at the kernel boundary.
//! 3. `touch /mount/foo` (zero writes between create and close) still fires
//!    `hydration.open_write` on release — the create-time `dirty=true`
//!    invariant required for POSIX `touch` to put an empty file on the cloud.

use std::collections::HashMap;
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
async fn create_round_trip_returns_cache_path_and_handle_id() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("foo.txt");
    // The Rust side opens with O_CREAT|O_TRUNC so the file doesn't need to
    // exist beforehand — but the JVM normally would have just created it.
    let cache_path_str = cache_path.to_str().unwrap();

    let create_reply = format!(
        r#"{{"ok":true,"cache_path":"{cache_path_str}","handle_id":"create-jvm-1"}}"#
    );
    let open_write_reply = format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);

    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", r#"{"ok":true,"entries":[]}"#),
        ("hydration.create", create_reply.as_str()),
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
        .expect("mount should succeed");

    tokio::time::sleep(Duration::from_millis(200)).await;

    let mp = mount_path.clone();
    let write_result = tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(mp.join("foo.txt"))?;
        f.write_all(b"hi\n")?;
        f.sync_all()?;
        Ok::<_, std::io::Error>(())
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    assert!(write_result.is_ok(), "create+write must succeed: {write_result:?}");

    // Cache file holds the written bytes.
    let cache_bytes = std::fs::read(&cache_path).expect("read cache after write");
    assert_eq!(cache_bytes, b"hi\n", "cache content mismatch");

    // hydration.create appears, carries the expected path, and is followed
    // by hydration.open_write referencing the JVM-supplied handle_id.
    let create_req = recorded
        .iter()
        .find(|r| r.contains(r#""verb":"hydration.create""#))
        .unwrap_or_else(|| panic!("expected hydration.create in recorded: {recorded:?}"));
    assert!(create_req.contains(r#""path":"/foo.txt""#), "create path mismatch: {create_req}");
    assert!(create_req.contains(r#""handle_id":"create-"#), "create handle_id prefix mismatch: {create_req}");

    let open_write_req = recorded
        .iter()
        .find(|r| r.contains(r#""verb":"hydration.open_write""#))
        .unwrap_or_else(|| panic!("expected hydration.open_write in recorded: {recorded:?}"));
    assert!(
        open_write_req.contains(r#""handle_id":"create-jvm-1""#),
        "open_write must reuse JVM-returned handle_id; got: {open_write_req}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_returns_eexist_when_jvm_signals_path_exists() {
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", r#"{"ok":true,"entries":[]}"#),
        ("hydration.create", r#"{"ok":false,"error":"path_exists"}"#),
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
        .expect("mount should succeed");

    tokio::time::sleep(Duration::from_millis(200)).await;

    let mp = mount_path.clone();
    let create_result = tokio::task::spawn_blocking(move || {
        // O_CREAT|O_EXCL guarantees the kernel routes through FUSE create
        // (not a lookup-then-open dance) and surfaces EEXIST verbatim from
        // the filesystem's reply.
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(mp.join("exists.txt"))
    })
    .await
    .unwrap();

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    assert!(create_result.is_err(), "create on existing path must fail");
    let err = create_result.unwrap_err();
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::AlreadyExists,
        "create must surface EEXIST (AlreadyExists) when JVM signals path_exists; got: {err}"
    );

    let create_req = recorded.iter().find(|r| r.contains(r#""verb":"hydration.create""#));
    assert!(create_req.is_some(), "expected hydration.create in recorded: {recorded:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn touch_creates_empty_file_and_release_uploads() {
    // touch foo == open(O_CREAT|O_WRONLY) -> close() with zero writes in
    // between. Without the create-time dirty bit, release would skip
    // hydration.open_write and the cloud would never see the empty file.
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("touched.txt");
    let cache_path_str = cache_path.to_str().unwrap();

    let create_reply = format!(
        r#"{{"ok":true,"cache_path":"{cache_path_str}","handle_id":"create-jvm-7"}}"#
    );
    let open_write_reply = format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);

    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", r#"{"ok":true,"entries":[]}"#),
        ("hydration.create", create_reply.as_str()),
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
        .expect("mount should succeed");

    tokio::time::sleep(Duration::from_millis(200)).await;

    let mp = mount_path.clone();
    let touch_result = tokio::task::spawn_blocking(move || {
        // Exactly the touch(1) syscall sequence: open(O_CREAT|O_WRONLY) then
        // close. No write between them.
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(mp.join("touched.txt"))?;
        drop(f);
        Ok::<_, std::io::Error>(())
    })
    .await
    .unwrap();

    // Give the kernel time to issue RELEASE and the IPC drain.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    assert!(touch_result.is_ok(), "touch must succeed: {touch_result:?}");

    // open_write MUST appear despite zero writes — the create-time dirty
    // bit is the load-bearing invariant under test.
    let open_write_idx = recorded
        .iter()
        .position(|r| r.contains(r#""verb":"hydration.open_write""#))
        .unwrap_or_else(|| panic!(
            "touch on a freshly-created file must fire hydration.open_write \
             (POSIX touch creates an empty file on the cloud). recorded={recorded:?}"
        ));
    let close_idx = recorded
        .iter()
        .position(|r| r.contains(r#""verb":"hydration.close_handle""#))
        .unwrap_or_else(|| panic!("expected hydration.close_handle in recorded: {recorded:?}"));
    assert!(
        open_write_idx < close_idx,
        "open_write must precede close_handle in the touch sequence. recorded={recorded:?}"
    );

    // The open_write must reuse the JVM-returned handle_id so the JVM
    // recognises the upload as belonging to the create's open-set entry.
    let open_write_req = &recorded[open_write_idx];
    assert!(
        open_write_req.contains(r#""handle_id":"create-jvm-7""#),
        "open_write must carry JVM-returned create handle_id: {open_write_req}"
    );
}

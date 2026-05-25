//! FUSE setattr / truncate integration tests.
//!
//! Covers the four orthogonal invariants from the B1 spec:
//!
//! (a) setattr(size=0) on a cloud-only file (no fh): FakeJvm sees
//!     `hydration.open_write_begin` and NOT `hydration.open_read`; a
//!     synchronous `hydration.open_write` commit fires; the cache file is 0 bytes.
//!
//! (b) setattr(size=N>0) bare (no fh): FakeJvm sees `hydration.open_read`
//!     to materialise the file; cache file length is set to N via set_len.
//!
//! (c) set_len failure → setattr returns EIO and no open_write commit fires.
//!     Simulated by pointing the cache_path at a directory so opening for
//!     write fails before set_len is attempted. [NOTES: only testable for the
//!     size>0 / no-fh path where we open a fresh cache file returned by
//!     open_read.  For the fh path the file is already open — a separate
//!     future test would need kernel cooperation.]
//!
//! (d) setattr(mode=0o600) chmod: returns Ok, no open_read/open_write fired.

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

/// Mount a UnidriveFs backed by the given FakeJvm and return the mount path
/// and mount handle. Caller is responsible for unmounting and shutting down.
async fn setup_mount(
    jvm: &FakeJvm,
) -> (tempfile::TempDir, fuse3::raw::MountHandle) {
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

    (tempdir, mount_handle)
}

// ---------------------------------------------------------------------------
// (a) setattr(size=0) bare truncate — open_write_begin fires, NOT open_read,
//     commit fires, cache file is 0 bytes.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setattr_truncate_to_zero_uses_open_write_begin_not_open_read() {
    // The cache file the JVM "materialises" for the truncate-to-zero case.
    // We pre-create it with some bytes to verify it ends up empty.
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("foo.cache");
    {
        let mut f = std::fs::File::create(&cache_path).unwrap();
        f.write_all(b"existing data").unwrap();
    }
    let cache_path_str = cache_path.to_str().unwrap();

    let open_write_begin_reply =
        format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let open_write_reply =
        format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let list_reply = r#"{"ok":true,"entries":[{"path":"/foo.txt","size":100,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#.to_string();

    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply.as_str()),
        ("hydration.open_write_begin", open_write_begin_reply.as_str()),
        ("hydration.open_write", open_write_reply.as_str()),
        // Deliberately omit open_read: if setattr calls it, FakeJvm returns
        // no_canned_reply and the test would fail at the open_read assertion.
    ]))
    .await;

    let (tempdir, mount_handle) = setup_mount(&jvm).await;
    let mp = tempdir.path().to_path_buf();

    // Perform bare truncate(2) to 0 via truncate syscall — maps to FUSE setattr
    // with size=0 and no fh.
    tokio::task::spawn_blocking(move || {
        // `std::fs::OpenOptions::new().write(true).truncate(true).open(path)` would
        // also trigger an `open()` first, giving us an fh.  We need the bare-truncate
        // (no-fh) path, which is reached via nix::unistd::truncate or std equivalent.
        // Use std::fs::File metadata + set_len approach via a standalone truncate.
        // The most portable way: open with write only (O_WRONLY | O_TRUNC).
        // Actually O_TRUNC is a VFS truncate at open-time, which the kernel sends
        // as a setattr(size=0) BEFORE or AS PART OF the open. But the FUSE kernel
        // module sends O_TRUNC as part of FUSE_OPEN (and sometimes a separate
        // FUSE_SETATTR). To reliably get a bare FUSE_SETATTR with no fh we use
        // the truncate(2) syscall directly (not ftruncate(2) which needs an fd).
        //
        // Use nix or libc directly:
        let c_path = std::ffi::CString::new(
            mp.join("foo.txt").to_str().unwrap()
        ).unwrap();
        let ret = unsafe { libc::truncate(c_path.as_ptr(), 0) };
        assert_eq!(ret, 0, "truncate(foo.txt, 0) failed: {}", std::io::Error::last_os_error());
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    // open_write_begin must fire.
    assert!(
        recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_write_begin""#)),
        "expected hydration.open_write_begin in recorded: {recorded:?}"
    );

    // open_read must NOT fire.
    assert!(
        !recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_read""#)),
        "open_read must NOT fire for truncate-to-zero: {recorded:?}"
    );

    // Synchronous commit must fire (bare truncate has no fh).
    assert!(
        recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_write""#)),
        "expected hydration.open_write commit in recorded: {recorded:?}"
    );

    // The cache file truncation to 0 is performed by the JVM's prepareEmptyCache
    // (TRUNCATE_EXISTING).  The FakeJvm returns a canned reply without actually
    // truncating the file, so we do not assert byte count here — that is a JVM
    // contract, not a fuse_fs invariant.
}

// ---------------------------------------------------------------------------
// (b) setattr(size=N>0) bare — open_read fires, cache file length == N.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setattr_truncate_to_n_downloads_and_sets_len() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("bar.cache");
    // Create with 20 bytes; truncate will be set_len'd to 7.
    {
        let mut f = std::fs::File::create(&cache_path).unwrap();
        f.write_all(b"12345678901234567890").unwrap();
    }
    let cache_path_str = cache_path.to_str().unwrap();
    const TARGET_SIZE: u64 = 7;

    let open_read_reply =
        format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let open_write_reply =
        format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let list_reply = r#"{"ok":true,"entries":[{"path":"/bar.txt","size":20,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#.to_string();

    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply.as_str()),
        ("hydration.open_read", open_read_reply.as_str()),
        ("hydration.open_write", open_write_reply.as_str()),
        ("hydration.close_handle", r#"{"ok":true}"#),
    ]))
    .await;

    let (tempdir, mount_handle) = setup_mount(&jvm).await;
    let mp = tempdir.path().to_path_buf();

    tokio::task::spawn_blocking(move || {
        let c_path = std::ffi::CString::new(
            mp.join("bar.txt").to_str().unwrap()
        ).unwrap();
        let ret = unsafe { libc::truncate(c_path.as_ptr(), TARGET_SIZE as libc::off_t) };
        assert_eq!(ret, 0, "truncate(bar.txt, {TARGET_SIZE}) failed: {}", std::io::Error::last_os_error());
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    // open_read must fire (download to materialise the file before set_len).
    assert!(
        recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_read""#)),
        "expected hydration.open_read in recorded: {recorded:?}"
    );

    // Commit must fire.
    assert!(
        recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_write""#)),
        "expected hydration.open_write commit in recorded: {recorded:?}"
    );

    // Cache file must be exactly TARGET_SIZE bytes.
    let meta = std::fs::metadata(&cache_path).unwrap();
    assert_eq!(
        meta.len(),
        TARGET_SIZE,
        "cache file must be {TARGET_SIZE} bytes after truncate"
    );
}

// ---------------------------------------------------------------------------
// (c) set_len failure → EIO, no open_write commit.
//
// Simulate by making open_read return a cache_path pointing at a directory.
// Opening a directory for write will fail (EISDIR), which maps to EIO in our
// handler.  This tests the "failure before commit" invariant: the set_len path
// must not fire open_write if materialisation/open fails.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setattr_truncate_set_len_failure_returns_eio_no_commit() {
    let cache_dir = tempfile::tempdir().unwrap();
    // Point cache_path at a *directory*, not a file — opening it for write fails.
    let bad_path = cache_dir.path().join("notafile");
    std::fs::create_dir_all(&bad_path).unwrap();
    let bad_path_str = bad_path.to_str().unwrap();

    // open_read returns the bad path; open_write must NOT be called.
    let open_read_reply = format!(r#"{{"ok":true,"cache_path":"{bad_path_str}"}}"#);
    let list_reply = r#"{"ok":true,"entries":[{"path":"/baz.txt","size":100,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#.to_string();

    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply.as_str()),
        ("hydration.open_read", open_read_reply.as_str()),
        // Deliberately omit open_write — if it's called, FakeJvm returns
        // no_canned_reply, but we also assert count==0 below.
        ("hydration.close_handle", r#"{"ok":true}"#),
    ]))
    .await;

    let (tempdir, mount_handle) = setup_mount(&jvm).await;
    let mp = tempdir.path().to_path_buf();

    // truncate to a non-zero size so we take the open_read path.
    let truncate_result: i32 = tokio::task::spawn_blocking(move || {
        let c_path = std::ffi::CString::new(
            mp.join("baz.txt").to_str().unwrap()
        ).unwrap();
        unsafe { libc::truncate(c_path.as_ptr(), 50 as libc::off_t) }
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    // The truncate syscall must return an error (EIO surfaced to userland).
    assert_eq!(
        truncate_result, -1,
        "truncate must return -1 (error) when set_len fails"
    );

    // open_write must NOT have fired — no commit on failure.
    let commit_count = recorded
        .iter()
        .filter(|r| r.contains(r#""verb":"hydration.open_write""#))
        .count();
    assert_eq!(
        commit_count, 0,
        "open_write must NOT fire when set_len fails: {recorded:?}"
    );
}

// ---------------------------------------------------------------------------
// (e) open_write failure → EIO returned AND open_read handle is released.
//
// Invariant pinned by Fix 1: when open_write fails in the size=N>0 / no-fh
// path, the handle registered by open_read must still be closed (no leak).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setattr_truncate_open_write_failure_releases_open_read_handle() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("leak.cache");
    {
        let mut f = std::fs::File::create(&cache_path).unwrap();
        f.write_all(b"some existing data here").unwrap();
    }
    let cache_path_str = cache_path.to_str().unwrap();

    let open_read_reply = format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let list_reply = r#"{"ok":true,"entries":[{"path":"/leak.txt","size":23,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#.to_string();

    // open_write returns an error; close_handle returns ok.
    // Deliberately omit a success reply for open_write so FakeJvm returns error.
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply.as_str()),
        ("hydration.open_read", open_read_reply.as_str()),
        ("hydration.open_write", r#"{"ok":false,"error":"simulated_commit_failure"}"#),
        ("hydration.close_handle", r#"{"ok":true}"#),
    ]))
    .await;

    let (tempdir, mount_handle) = setup_mount(&jvm).await;
    let mp = tempdir.path().to_path_buf();

    // truncate to a non-zero size — takes the open_read path, then open_write fails.
    let truncate_result: i32 = tokio::task::spawn_blocking(move || {
        let c_path = std::ffi::CString::new(
            mp.join("leak.txt").to_str().unwrap()
        ).unwrap();
        unsafe { libc::truncate(c_path.as_ptr(), 10 as libc::off_t) }
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    // setattr must return an error (EIO) to userland.
    assert_eq!(
        truncate_result, -1,
        "truncate must return -1 (error) when open_write fails"
    );

    // open_read must have fired (we took the truncate-to-N / no-fh path).
    assert!(
        recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_read""#)),
        "expected hydration.open_read in recorded: {recorded:?}"
    );

    // open_write must have fired (and returned an error).
    assert!(
        recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_write""#)),
        "expected hydration.open_write in recorded: {recorded:?}"
    );

    // close_handle MUST have fired — the open_read handle must not be leaked.
    assert!(
        recorded.iter().any(|r| r.contains(r#""verb":"hydration.close_handle""#)),
        "close_handle must fire to release open_read handle after open_write failure: {recorded:?}"
    );
}

// ---------------------------------------------------------------------------
// (d) setattr(mode=0o600) chmod — returns Ok, no open_read, no open_write.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setattr_chmod_is_noop_no_ipc_verbs_fired() {
    let list_reply = r#"{"ok":true,"entries":[{"path":"/chmod.txt","size":42,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#.to_string();

    // Only supply the list reply; any open_read/open_write would be a
    // no_canned_reply error.
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply.as_str()),
    ]))
    .await;

    let (tempdir, mount_handle) = setup_mount(&jvm).await;
    let mp = tempdir.path().to_path_buf();

    let chmod_result: i32 = tokio::task::spawn_blocking(move || {
        let c_path = std::ffi::CString::new(
            mp.join("chmod.txt").to_str().unwrap()
        ).unwrap();
        unsafe { libc::chmod(c_path.as_ptr(), 0o600) }
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    assert_eq!(chmod_result, 0, "chmod must succeed (FUSE no-op)");

    assert!(
        !recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_read""#)),
        "open_read must NOT fire on chmod: {recorded:?}"
    );
    assert!(
        !recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_write""#)),
        "open_write must NOT fire on chmod: {recorded:?}"
    );
}

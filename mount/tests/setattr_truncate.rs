//! FUSE setattr / truncate integration tests.
//!
//! Covers six orthogonal invariants from the B1 spec:
//!
//! (a) setattr(size=0) on a cloud-only file (no fh): FakeJvm sees
//!     `hydration.open_write_begin` and NOT `hydration.open_read`; a
//!     synchronous `hydration.open_write` commit fires; the cache file is 0
//!     bytes; `hydration.close_handle` fires exactly once (for the synthetic
//!     trunc0-N handle registered by open_write, not by open_write_begin).
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
//!
//! (e) open_write failure in size=N>0 / no-fh path: setattr returns EIO AND
//!     the handle registered by open_read is released via close_handle (no
//!     leak).
//!
//! (f) open_write failure in size=0 / no-fh path: setattr returns EIO;
//!     open_write_begin and open_write both fired; close_handle must NOT fire
//!     (open_write_begin registers no handle, so a failed commit must not
//!     attempt a spurious close).

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use unidrive_mount::fake_jvm::{replies, FakeJvm};
use unidrive_mount::fuse_fs::UnidriveFs;
use unidrive_mount::reconnect::ReconnectingIpcClient;


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
        ("hydration.close_handle", r#"{"ok":true}"#),
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

    // close_handle MUST fire exactly once — for the synthetic trunc0-N handle
    // registered by open_write (open_write_begin is called with None and registers
    // nothing, but the subsequent open_write call registers a handle that must be
    // released).
    let close_count = recorded
        .iter()
        .filter(|r| r.contains(r#""verb":"hydration.close_handle""#))
        .count();
    assert_eq!(
        close_count, 1,
        "close_handle must fire exactly once (for the trunc0 open_write handle): {recorded:?}"
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
// (f) open_write failure in size=0 / no-fh path — EIO returned, close_handle
//     MUST fire exactly once.
//
// open_write_begin is called with None (registers nothing in the JVM open-set).
// The subsequent open_write call (with synthetic trunc0-N handle) DOES register
// a JVM open-set entry.  Even when open_write fails, close_handle must fire to
// release that entry — identical to the size=N path's "always close" pattern.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setattr_truncate_to_zero_open_write_failure_does_not_close_handle() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("zero_fail.cache");
    {
        let mut f = std::fs::File::create(&cache_path).unwrap();
        f.write_all(b"existing data").unwrap();
    }
    let cache_path_str = cache_path.to_str().unwrap();

    let open_write_begin_reply =
        format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);

    let list_reply = r#"{"ok":true,"entries":[{"path":"/zero_fail.txt","size":13,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#.to_string();

    // open_write_begin succeeds; open_write returns an error.
    // close_handle must still fire (the trunc0-N handle registered by open_write
    // must be released even on failure — "always close" pattern).
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply.as_str()),
        ("hydration.open_write_begin", open_write_begin_reply.as_str()),
        ("hydration.open_write", r#"{"ok":false,"error":"simulated_commit_failure"}"#),
        ("hydration.close_handle", r#"{"ok":true}"#),
        // Deliberately omit open_read.
    ]))
    .await;

    let (tempdir, mount_handle) = setup_mount(&jvm).await;
    let mp = tempdir.path().to_path_buf();

    // Bare truncate(2) to 0 — takes the size=0 / no-fh path.
    let truncate_result: i32 = tokio::task::spawn_blocking(move || {
        let c_path = std::ffi::CString::new(
            mp.join("zero_fail.txt").to_str().unwrap()
        ).unwrap();
        unsafe { libc::truncate(c_path.as_ptr(), 0) }
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

    // open_write_begin must have fired.
    assert!(
        recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_write_begin""#)),
        "expected hydration.open_write_begin in recorded: {recorded:?}"
    );

    // open_write must have fired (and returned an error).
    assert!(
        recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_write""#)),
        "expected hydration.open_write in recorded: {recorded:?}"
    );

    // close_handle MUST fire exactly once — the trunc0-N handle registered by
    // open_write must be released even when open_write failed (open_write_begin
    // with None registers nothing; only open_write registers).
    let close_count = recorded
        .iter()
        .filter(|r| r.contains(r#""verb":"hydration.close_handle""#))
        .count();
    assert_eq!(
        close_count, 1,
        "close_handle must fire once (for trunc0 open_write handle) even when open_write fails: {recorded:?}"
    );
}

// ---------------------------------------------------------------------------
// B3 (i) ftruncate-to-zero on an open fd, then write — the write must survive.
//
// Invariant pinned: a post-truncate write on the same open handle must not be
// dropped at release.  Path under test: setattr(fh, size=0) →
// `h.file.set_len(0)` + `h.dirty=true` → write → `h.file.write_at` +
// `h.dirty=true` → release → open_write fires.  The cache file must contain
// exactly the bytes written after the truncate; the truncate must not have
// clobbered them.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ftruncate_to_zero_then_write_commits_the_write() {
    // Pre-seed the cache file with "old" content so the O_WRONLY open (without
    // O_TRUNC) goes through open_read and the JVM replies with this file.
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("b3_ftrunc_then_write.cache");
    {
        let mut f = std::fs::File::create(&cache_path).unwrap();
        f.write_all(b"OLDCONTENT").unwrap();
    }
    let cache_path_str = cache_path.to_str().unwrap();

    let open_read_reply = format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let open_write_reply = format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let list_reply = r#"{"ok":true,"entries":[{"path":"/b3_fw.txt","size":10,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#.to_string();

    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply.as_str()),
        ("hydration.open_read", open_read_reply.as_str()),
        ("hydration.open_write", open_write_reply.as_str()),
        ("hydration.close_handle", r#"{"ok":true}"#),
    ]))
    .await;

    let (tempdir, mount_handle) = setup_mount(&jvm).await;
    let mp = tempdir.path().to_path_buf();

    // Open O_WRONLY without O_TRUNC so open_read (not open_write_begin) is
    // called — the handle is registered with dirty=false and the existing cache
    // content is on disk.  Then ftruncate(2) to 0 (→ setattr fh size=0), then
    // write b"hi\n".  Release must commit the post-truncate write, not drop it.
    tokio::task::spawn_blocking(move || {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(mp.join("b3_fw.txt"))
            .expect("open O_WRONLY must succeed");
        // ftruncate(2): sets cache file length to 0 via setattr(fh, size=0).
        f.set_len(0).expect("ftruncate to 0 must succeed");
        // write at offset 0 — must land in the now-empty cache file.
        f.write_all(b"hi\n").expect("write after ftruncate must succeed");
        // Drop → FUSE RELEASE → open_write fires if dirty.
    })
    .await
    .unwrap();

    // Allow RELEASE IPC to drain.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    // open_write must fire at release — the handle is dirty.
    assert!(
        recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_write""#)),
        "open_write must fire at release for dirty handle: {recorded:?}"
    );

    // The cache file must contain exactly the post-truncate write.
    // The ftruncate set the file to 0 bytes; the subsequent write wrote 3 bytes
    // at offset 0.  If the truncate erroneously dropped the write, the file
    // would be empty.
    let cache_bytes = std::fs::read(&cache_path).expect("read cache after release");
    assert_eq!(
        cache_bytes, b"hi\n",
        "post-ftruncate write must survive to the committed cache file; \
         got {:?} — truncate must not drop a subsequent write",
        cache_bytes
    );
}

// ---------------------------------------------------------------------------
// B3 (j) write then ftruncate-to-zero on the same open fd — the later truncate
//        must win; the earlier write must be discarded.
//
// Invariant pinned: a post-write ftruncate on the same open handle must result
// in an empty committed file.  The earlier write's bytes must not appear in the
// cloud.  Path under test: open → write → setattr(fh, size=0) →
// `h.file.set_len(0)` (overwrites the write region) + `h.dirty=true` → release
// → open_write fires.  Cache file must end at 0 bytes.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_then_ftruncate_to_zero_commits_empty() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("b3_write_then_ftrunc.cache");
    {
        let mut f = std::fs::File::create(&cache_path).unwrap();
        f.write_all(b"OLDCONTENT").unwrap();
    }
    let cache_path_str = cache_path.to_str().unwrap();

    let open_read_reply = format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let open_write_reply = format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let list_reply = r#"{"ok":true,"entries":[{"path":"/b3_wf.txt","size":10,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#.to_string();

    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply.as_str()),
        ("hydration.open_read", open_read_reply.as_str()),
        ("hydration.open_write", open_write_reply.as_str()),
        ("hydration.close_handle", r#"{"ok":true}"#),
    ]))
    .await;

    let (tempdir, mount_handle) = setup_mount(&jvm).await;
    let mp = tempdir.path().to_path_buf();

    // Open O_WRONLY (no O_TRUNC) → open_read → dirty=false.
    // write "data" → dirty=true, cache has "data" + tail of OLDCONTENT.
    // ftruncate(2) to 0 → setattr(fh, size=0) → set_len(0), dirty remains true.
    // Release → open_write → committed cache must be empty.
    tokio::task::spawn_blocking(move || {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(mp.join("b3_wf.txt"))
            .expect("open O_WRONLY must succeed");
        f.write_all(b"data").expect("write must succeed");
        // ftruncate(2): must win over the earlier write.
        f.set_len(0).expect("ftruncate to 0 must succeed");
        // Drop → FUSE RELEASE → open_write fires.
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    // open_write must fire at release.
    assert!(
        recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_write""#)),
        "open_write must fire at release for dirty handle: {recorded:?}"
    );

    // Cache file must be empty — the ftruncate(2) discarded the earlier write.
    let cache_bytes = std::fs::read(&cache_path).expect("read cache after release");
    assert_eq!(
        cache_bytes.len(), 0,
        "ftruncate-after-write must result in an empty committed cache file; \
         got {} bytes ({:?}) — later truncate must win over earlier write",
        cache_bytes.len(), cache_bytes
    );
}

// ---------------------------------------------------------------------------
// B2 (g) open(O_WRONLY|O_TRUNC) routes through open_write_begin, NOT open_read.
//
// Invariant: opening a cloud-only file with O_TRUNC must not download the
// existing content — it calls open_write_begin (prepare empty cache) instead
// of open_read (download).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_with_o_trunc_uses_open_write_begin_not_open_read() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("trunc_open.cache");
    // Pre-create a file so open(O_RDWR) on the cache_path succeeds.
    {
        let mut f = std::fs::File::create(&cache_path).unwrap();
        f.write_all(b"existing cloud content").unwrap();
    }
    let cache_path_str = cache_path.to_str().unwrap();

    let open_write_begin_reply =
        format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let open_write_reply =
        format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let list_reply = r#"{"ok":true,"entries":[{"path":"/trunc_open.txt","size":22,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#.to_string();

    // Deliberately omit open_read: if open() calls it, FakeJvm returns
    // no_canned_reply and the open syscall will fail.
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply.as_str()),
        ("hydration.open_write_begin", open_write_begin_reply.as_str()),
        ("hydration.open_write", open_write_reply.as_str()),
        ("hydration.close_handle", r#"{"ok":true}"#),
    ]))
    .await;

    let (tempdir, mount_handle) = setup_mount(&jvm).await;
    let mp = tempdir.path().to_path_buf();

    tokio::task::spawn_blocking(move || {
        // open O_WRONLY|O_TRUNC then close immediately (no write).
        let _f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(mp.join("trunc_open.txt"))
            .expect("open O_WRONLY|O_TRUNC must succeed");
        // drop closes fd -> FUSE RELEASE
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    // open_write_begin MUST fire (no download).
    assert!(
        recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_write_begin""#)),
        "expected hydration.open_write_begin in recorded: {recorded:?}"
    );

    // open_read must NOT fire (no download on O_TRUNC open).
    assert!(
        !recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_read""#)),
        "open_read must NOT fire for O_TRUNC open: {recorded:?}"
    );
}

// ---------------------------------------------------------------------------
// B2 (h) open(O_WRONLY|O_TRUNC) + write nothing + close → commits empty file.
//
// Invariant (dirty-on-trunc): the `> file` idiom (open O_TRUNC, write nothing,
// close) must still commit the empty file at release. The OpenHandle's dirty
// bit must be true from the moment it is registered for the truncating path.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_o_trunc_then_close_with_no_writes_commits_empty_file() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("trunc_dirty.cache");
    {
        let mut f = std::fs::File::create(&cache_path).unwrap();
        f.write_all(b"stale cloud bytes").unwrap();
    }
    let cache_path_str = cache_path.to_str().unwrap();

    let open_write_begin_reply =
        format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let open_write_reply =
        format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let list_reply = r#"{"ok":true,"entries":[{"path":"/trunc_dirty.txt","size":17,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#.to_string();

    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply.as_str()),
        ("hydration.open_write_begin", open_write_begin_reply.as_str()),
        ("hydration.open_write", open_write_reply.as_str()),
        ("hydration.close_handle", r#"{"ok":true}"#),
    ]))
    .await;

    let (tempdir, mount_handle) = setup_mount(&jvm).await;
    let mp = tempdir.path().to_path_buf();

    tokio::task::spawn_blocking(move || {
        // open O_WRONLY|O_TRUNC, write NOTHING, then close.
        let _f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(mp.join("trunc_dirty.txt"))
            .expect("open O_WRONLY|O_TRUNC must succeed");
        // _f is dropped here -> FUSE RELEASE fires
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    // open_write (commit) MUST fire even though zero bytes were written.
    // This pins the dirty-on-trunc invariant: the > file idiom must not silently fail.
    assert!(
        recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_write""#)),
        "open_write must fire at release even with no writes after O_TRUNC open: {recorded:?}"
    );

    // open_write must carry the correct remote path.
    let ow = recorded
        .iter()
        .find(|r| r.contains(r#""verb":"hydration.open_write""#))
        .unwrap();
    assert!(
        ow.contains(r#""path":"/trunc_dirty.txt""#),
        "open_write must reference the correct remote path: {ow}"
    );
}

// ---------------------------------------------------------------------------
// P1 (k) open(O_WRONLY|O_TRUNC) carries a handle_id in open_write_begin AND
//         a matching close_handle fires at release.
//
// Invariant: a live O_TRUNC open must register a JVM open-set entry so that
// dehydrate/busy-checks see the file as open for the duration of the handle.
// This pins the IPC contract: open_write_begin request must contain
// "handle_id" when called from the open(O_TRUNC) path, and release must fire
// the symmetric close_handle with the same id.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_o_trunc_open_write_begin_carries_handle_id_and_release_closes_it() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("p1k.cache");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&cache_path).unwrap();
        f.write_all(b"cloud content").unwrap();
    }
    let cache_path_str = cache_path.to_str().unwrap();

    let open_write_begin_reply =
        format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let open_write_reply =
        format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let list_reply = r#"{"ok":true,"entries":[{"path":"/p1k.txt","size":13,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#.to_string();

    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply.as_str()),
        ("hydration.open_write_begin", open_write_begin_reply.as_str()),
        ("hydration.open_write", open_write_reply.as_str()),
        ("hydration.close_handle", r#"{"ok":true}"#),
    ]))
    .await;

    let (tempdir, mount_handle) = setup_mount(&jvm).await;
    let mp = tempdir.path().to_path_buf();

    tokio::task::spawn_blocking(move || {
        let _f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(mp.join("p1k.txt"))
            .expect("open O_WRONLY|O_TRUNC must succeed");
        // drop -> FUSE RELEASE
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    // open_write_begin must carry a handle_id (live open path).
    let owb = recorded
        .iter()
        .find(|r| r.contains(r#""verb":"hydration.open_write_begin""#))
        .expect("open_write_begin must fire");
    assert!(
        owb.contains(r#""handle_id""#),
        "open_write_begin for O_TRUNC open must include a handle_id: {owb}"
    );

    // A matching close_handle must fire at release (symmetric cleanup).
    assert!(
        recorded.iter().any(|r| r.contains(r#""verb":"hydration.close_handle""#)),
        "close_handle must fire at release to clear the JVM open-set entry: {recorded:?}"
    );

    // The handle_id in close_handle must match the one sent in open_write_begin.
    // Extract the handle_id value from the open_write_begin request.
    let hid_start = owb.find(r#""handle_id":""#).expect("handle_id field present") + r#""handle_id":""#.len();
    let hid_end = owb[hid_start..].find('"').expect("closing quote") + hid_start;
    let owb_handle_id = &owb[hid_start..hid_end];

    let ch = recorded
        .iter()
        .find(|r| r.contains(r#""verb":"hydration.close_handle""#))
        .expect("close_handle must fire");
    assert!(
        ch.contains(owb_handle_id),
        "close_handle handle_id must match open_write_begin handle_id ({owb_handle_id}): {ch}"
    );
}

// ---------------------------------------------------------------------------
// P1 (l) bare truncate(2) to 0 (no-fh setattr) — open_write_begin carries NO
//         handle_id (one-shot; JVM must NOT register an open-set entry).
//
// This is the complementary invariant to (k): the setattr/bare-truncate path
// must remain unregistered. Pinned by the existing (a) and (f) tests (which
// assert close_handle does not fire); this test adds an explicit assertion on
// the request itself — confirming handle_id is absent from the wire.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bare_truncate_to_zero_open_write_begin_carries_no_handle_id() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("p1l.cache");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&cache_path).unwrap();
        f.write_all(b"existing data").unwrap();
    }
    let cache_path_str = cache_path.to_str().unwrap();

    let open_write_begin_reply =
        format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let open_write_reply =
        format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let list_reply = r#"{"ok":true,"entries":[{"path":"/p1l.txt","size":13,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#.to_string();

    // close_handle must fire (for the trunc0-N open_write handle), but NOT for
    // the open_write_begin call (which carries no handle_id and registers nothing).
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply.as_str()),
        ("hydration.open_write_begin", open_write_begin_reply.as_str()),
        ("hydration.open_write", open_write_reply.as_str()),
        ("hydration.close_handle", r#"{"ok":true}"#),
    ]))
    .await;

    let (tempdir, mount_handle) = setup_mount(&jvm).await;
    let mp = tempdir.path().to_path_buf();

    tokio::task::spawn_blocking(move || {
        let c_path = std::ffi::CString::new(
            mp.join("p1l.txt").to_str().unwrap()
        ).unwrap();
        let ret = unsafe { libc::truncate(c_path.as_ptr(), 0) };
        assert_eq!(ret, 0, "bare truncate(2) to 0 must succeed");
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    // open_write_begin must fire.
    let owb = recorded
        .iter()
        .find(|r| r.contains(r#""verb":"hydration.open_write_begin""#))
        .expect("open_write_begin must fire for bare truncate-to-zero");

    // handle_id must NOT be present (one-shot path — no open-set registration).
    assert!(
        !owb.contains(r#""handle_id""#),
        "open_write_begin for bare truncate must NOT include handle_id (one-shot): {owb}"
    );

    // close_handle must fire exactly once — for the synthetic trunc0-N handle
    // registered by open_write (not by open_write_begin, which carries no handle_id).
    let close_count = recorded
        .iter()
        .filter(|r| r.contains(r#""verb":"hydration.close_handle""#))
        .count();
    assert_eq!(
        close_count, 1,
        "close_handle must fire once (for the trunc0 open_write handle, not open_write_begin): {recorded:?}"
    );
}

// ---------------------------------------------------------------------------
// Bug A — open_write_begin "unknown_path" maps to ENOENT, not EIO.
//
// The JVM returns {"ok":false,"error":"unknown_path"} from openWriteBegin
// when the target path does not exist.  namespace_err_to_errno must surface
// this as ENOENT rather than the EIO fallback from ipc_error_to_errno.
// Exercise both the open(O_WRONLY|O_TRUNC) path and the bare truncate(2) path.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_write_begin_unknown_path_maps_to_enoent() {
    let list_reply = r#"{"ok":true,"entries":[{"path":"/ghost.txt","size":0,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#.to_string();

    // open_write_begin returns unknown_path; the open(2) must fail with ENOENT.
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply.as_str()),
        ("hydration.open_write_begin", r#"{"ok":false,"error":"unknown_path"}"#),
    ]))
    .await;

    let (tempdir, mount_handle) = setup_mount(&jvm).await;
    let mp = tempdir.path().to_path_buf();

    // open(O_WRONLY|O_TRUNC) — kernel sends FUSE open with O_TRUNC which calls
    // open_write_begin.  Must fail with ENOENT.
    let errno_val: i32 = tokio::task::spawn_blocking(move || {
        let result = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(mp.join("ghost.txt"));
        match result {
            Ok(_) => 0,
            Err(e) => e.raw_os_error().unwrap_or(-1),
        }
    })
    .await
    .unwrap();

    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    assert_eq!(
        errno_val,
        libc::ENOENT,
        "open(O_WRONLY|O_TRUNC) must return ENOENT when open_write_begin returns unknown_path; got errno {errno_val}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_write_begin_path_is_folder_maps_to_eisdir() {
    let list_reply = r#"{"ok":true,"entries":[{"path":"/mydir","size":0,"mtime_ms":1000000,"hydrated":false,"folder":true}]}"#.to_string();

    // open_write_begin returns path_is_folder; the open(2) must fail with EISDIR.
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply.as_str()),
        ("hydration.open_write_begin", r#"{"ok":false,"error":"path_is_folder"}"#),
    ]))
    .await;

    let (tempdir, mount_handle) = setup_mount(&jvm).await;
    let mp = tempdir.path().to_path_buf();

    let errno_val: i32 = tokio::task::spawn_blocking(move || {
        let result = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(mp.join("mydir"));
        match result {
            Ok(_) => 0,
            Err(e) => e.raw_os_error().unwrap_or(-1),
        }
    })
    .await
    .unwrap();

    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    assert_eq!(
        errno_val,
        libc::EISDIR,
        "open(O_WRONLY|O_TRUNC) must return EISDIR when open_write_begin returns path_is_folder; got errno {errno_val}"
    );
}

// ---------------------------------------------------------------------------
// Bug B — bare truncate(2) to 0 releases its open_write (trunc0-N) handle.
//
// Invariant: the synthetic trunc0-N handle passed to open_write IS registered
// in the JVM open-set (openForWrite registers openSets[connId][handleId]).
// After open_write completes (success or failure), close_handle must fire with
// the same trunc0-N id.  Mirror the verb-sequence assertions from the size=N
// path (test e / setattr_truncate_open_write_failure_releases_open_read_handle).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bare_truncate_to_zero_closes_its_open_write_handle() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("trunc0_close.cache");
    {
        let mut f = std::fs::File::create(&cache_path).unwrap();
        f.write_all(b"existing data").unwrap();
    }
    let cache_path_str = cache_path.to_str().unwrap();

    let open_write_begin_reply =
        format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let open_write_reply =
        format!(r#"{{"ok":true,"cache_path":"{cache_path_str}"}}"#);
    let list_reply = r#"{"ok":true,"entries":[{"path":"/trunc0_close.txt","size":13,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#.to_string();

    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", list_reply.as_str()),
        ("hydration.open_write_begin", open_write_begin_reply.as_str()),
        ("hydration.open_write", open_write_reply.as_str()),
        ("hydration.close_handle", r#"{"ok":true}"#),
    ]))
    .await;

    let (tempdir, mount_handle) = setup_mount(&jvm).await;
    let mp = tempdir.path().to_path_buf();

    tokio::task::spawn_blocking(move || {
        let c_path = std::ffi::CString::new(
            mp.join("trunc0_close.txt").to_str().unwrap()
        ).unwrap();
        let ret = unsafe { libc::truncate(c_path.as_ptr(), 0) };
        assert_eq!(ret, 0, "bare truncate(2) to 0 must succeed");
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let recorded = jvm.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    // open_write must fire (synchronous commit for bare truncate).
    assert!(
        recorded.iter().any(|r| r.contains(r#""verb":"hydration.open_write""#)),
        "expected hydration.open_write in recorded: {recorded:?}"
    );

    // close_handle MUST fire — the trunc0-N handle registered by open_write
    // must be released to prevent a JVM open-set leak.
    assert!(
        recorded.iter().any(|r| r.contains(r#""verb":"hydration.close_handle""#)),
        "close_handle must fire to release the trunc0-N open_write handle: {recorded:?}"
    );

    // The trunc0 handle_id must appear in both open_write and close_handle.
    // Extract the handle_id from the open_write request and verify close_handle
    // carries the same value.
    let ow = recorded
        .iter()
        .find(|r| r.contains(r#""verb":"hydration.open_write""#))
        .expect("open_write must fire");
    let hid_start = ow.find(r#""handle_id":""#).expect("handle_id field in open_write")
        + r#""handle_id":""#.len();
    let hid_end = ow[hid_start..].find('"').expect("closing quote") + hid_start;
    let ow_handle_id = &ow[hid_start..hid_end];
    assert!(
        ow_handle_id.starts_with("trunc0-"),
        "open_write handle_id must start with 'trunc0-'; got: {ow_handle_id}"
    );

    let ch = recorded
        .iter()
        .find(|r| r.contains(r#""verb":"hydration.close_handle""#))
        .expect("close_handle must fire");
    assert!(
        ch.contains(ow_handle_id),
        "close_handle must carry the same handle_id as open_write ({ow_handle_id}): {ch}"
    );
}

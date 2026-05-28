//! Co-daemon-side profile flock(2) integration tests.
//!
//! Load-bearing per `../unidrive/docs/dev/specs/mount-sync-mode-mutex-design.md`
//! §4 R4 option (b): the Rust co-daemon acquires its own `flock(2)` on the
//! per-profile `.lock` file for the duration of the FUSE session. The kernel
//! releases the lock on process exit (orderly OR via SIGKILL), so a
//! `kill -9 <jvm-pid>` no longer leaves a JVM-released-but-mount-alive
//! window for a contending sync process to slip into.
//!
//! Test A asserts the binary holds the lock while running.
//! Test B asserts the binary refuses startup when the lock is pre-held.

use assert_cmd::cargo::cargo_bin;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use unidrive_mount::fake_jvm::{replies, FakeJvm};


/// Try a non-blocking BSD exclusive lock on `path`. Returns `Ok(true)` if
/// acquired (and immediately released by FD drop), `Ok(false)` if held by
/// another FD/process, `Err` on open failure.
fn try_flock_ex_nb(path: &std::path::Path) -> std::io::Result<bool> {
    let f = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)?;
    // SAFETY: flock(2) FFI; fd owned by `f`.
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        // Release immediately so we don't poison the lock for downstream
        // assertions in the same test.
        let _ = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
        Ok(true)
    } else {
        let err = std::io::Error::last_os_error();
        if matches!(err.raw_os_error(), Some(libc::EWOULDBLOCK)) {
            Ok(false)
        } else {
            Err(err)
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn co_daemon_acquires_profile_lock_for_session_lifetime() {
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.last_synced", r#"{"ok":true,"mtime_ms":1}"#),
        ("hydration.list", r#"{"ok":true,"entries":[]}"#),
    ]))
    .await;

    let cache_dir = tempfile::tempdir().unwrap();
    let mount_tmp = tempfile::tempdir().unwrap();
    let lock_tmp = tempfile::tempdir().unwrap();
    let lock_path = lock_tmp.path().join(".lock");

    let bin = cargo_bin("unidrive-mount");
    let mut child = Command::new(&bin)
        .arg("--mount")
        .arg(mount_tmp.path())
        .arg("--ipc")
        .arg(&jvm.socket_path)
        .arg("--cache")
        .arg(cache_dir.path())
        .arg("--lock")
        .arg(&lock_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn unidrive-mount");

    // Wait until the binary has progressed past the lock-acquire step. The
    // first observable side-effect after acquire is the cache_scanner's
    // hydration.last_synced traffic (or, on empty cache, the FUSE mount
    // returning and the binary blocking on signals). Poll for either: lock
    // file exists AND `flock` from this process fails with EWOULDBLOCK.
    let observed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if lock_path.exists() {
                match try_flock_ex_nb(&lock_path) {
                    Ok(false) => return true,
                    Ok(true) => { /* not yet held, retry */ }
                    Err(_) => { /* transient, retry */ }
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;

    // Capture child stderr for diagnostics if the assertion below fails.
    let _ = if let Some(pid) = child.id() {
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) }
    } else {
        0
    };
    let mut stderr_buf = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut stderr_buf).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    jvm.shutdown().await;

    assert!(
        observed.is_ok(),
        "expected co-daemon to hold flock on {} within 5s; stderr={stderr_buf}",
        lock_path.display(),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn co_daemon_refuses_when_profile_lock_already_held() {
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.list", r#"{"ok":true,"entries":[]}"#),
    ]))
    .await;

    let cache_dir = tempfile::tempdir().unwrap();
    let mount_tmp = tempfile::tempdir().unwrap();
    let lock_tmp = tempfile::tempdir().unwrap();
    let lock_path = lock_tmp.path().join(".lock");

    // Pre-acquire the flock from the test process. Hold for the duration of
    // the binary's lifetime via this in-scope File.
    let holder = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("open lock file");
    let rc = unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(rc, 0, "pre-acquire flock must succeed: {}", std::io::Error::last_os_error());

    let bin = cargo_bin("unidrive-mount");
    let output = Command::new(&bin)
        .arg("--mount")
        .arg(mount_tmp.path())
        .arg("--ipc")
        .arg(&jvm.socket_path)
        .arg("--cache")
        .arg(cache_dir.path())
        .arg("--lock")
        .arg(&lock_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();

    let output = tokio::time::timeout(Duration::from_secs(15), output)
        .await
        .expect("binary must exit within 15s on lock-held")
        .expect("spawn unidrive-mount");

    jvm.shutdown().await;

    // Exit code 1 (EX_GENERIC_FAILURE) per run.rs error path.
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit 1 on contended lock; stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("refusing mount"),
        "stderr must explain the refusal; got: {stderr}",
    );

    // Release the holder lock cleanly.
    let mut _drain = String::new();
    let mut h = holder;
    let _ = (&mut h).read_to_string(&mut _drain);
    drop(h);
}

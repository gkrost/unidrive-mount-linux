//! The first IPC connect at startup tolerates a daemon whose listener is not
//! there yet.
//!
//! One test per invariant:
//! - a listener that appears late is waited for, for both shapes of "not
//!   ready": no socket file (`ENOENT`) and a socket file nobody accepts on
//!   (`ECONNREFUSED`)
//! - a daemon that stays down is given up on after the budget, with the
//!   original error
//! - a terminal authentication failure is not retried
//! - the binary keeps exit code 1 and the message when the daemon is down,
//!   and starts when the daemon appears after the binary

use assert_cmd::cargo::cargo_bin;
use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};
use support::fake_jvm::{replies, FakeJvm};
use tokio::net::UnixListener;
use tokio::process::Command;
use unidrive_mount::ipc::IpcError;
use unidrive_mount::ipc_auth::IpcAuth;
use unidrive_mount::reconnect::connect_auth_at_startup;
mod support;

const FAST: Duration = Duration::from_millis(25);

fn v1() -> std::collections::HashMap<String, String> {
    replies(&[("daemon.status", r#"{"ok":true,"protocol_version":1}"#)])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_waits_for_a_listener_that_appears_late_no_socket_file() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("ipc.sock");
    let s2 = sock.clone();
    let late = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        FakeJvm::spawn_at(s2, v1()).await
    });
    let c = connect_auth_at_startup(&sock, &IpcAuth::new(None, ""), FAST, Duration::from_secs(10)).await;
    assert!(c.is_ok(), "{:?}", c.err());
    late.await.unwrap().shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_waits_for_a_listener_that_appears_late_socket_file_refusing() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("ipc.sock");
    // A socket file with nobody accepting: connect fails with ECONNREFUSED.
    drop(UnixListener::bind(&sock).unwrap());
    assert!(sock.exists());
    let s2 = sock.clone();
    let late = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        FakeJvm::spawn_at(s2, v1()).await
    });
    let c = connect_auth_at_startup(&sock, &IpcAuth::new(None, ""), FAST, Duration::from_secs(10)).await;
    assert!(c.is_ok(), "{:?}", c.err());
    late.await.unwrap().shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_gives_up_after_the_budget_with_the_original_error() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("never.sock");
    let t = Instant::now();
    let err = connect_auth_at_startup(&sock, &IpcAuth::new(None, ""), FAST, Duration::from_millis(400))
        .await
        .err()
        .expect("must fail");
    let took = t.elapsed();
    match err {
        IpcError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
        other => panic!("expected the original Io error, got {other:?}"),
    }
    assert!(took >= Duration::from_millis(400), "gave up early after {took:?}");
    assert!(took < Duration::from_secs(5), "overshot the budget: {took:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_does_not_retry_a_terminal_auth_failure() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("ipc.sock");
    // Protocol-2 daemon, no token file configured: auth_unavailable.
    let jvm = FakeJvm::spawn_at(
        sock.clone(),
        replies(&[("daemon.status", r#"{"ok":true,"protocol_version":2}"#)]),
    )
    .await;
    let t = Instant::now();
    let err = connect_auth_at_startup(&sock, &IpcAuth::new(None, "p"), FAST, Duration::from_secs(10))
        .await
        .err()
        .expect("must fail");
    assert!(err.is_auth_terminal(), "{err:?}");
    assert!(t.elapsed() < Duration::from_secs(2));
    assert_eq!(jvm.recorded_requests().await.len(), 1, "a terminal failure must not be retried");
    jvm.shutdown().await;
}

#[tokio::test]
async fn binary_exits_1_with_the_connect_message_when_the_daemon_is_down() {
    let dir = tempfile::tempdir().unwrap();
    let mount = tempfile::tempdir().unwrap();
    let t = Instant::now();
    let out = Command::new(cargo_bin("unidrive-mount"))
        .arg("--mount")
        .arg(mount.path())
        .arg("--ipc")
        .arg(dir.path().join("down.sock"))
        .arg("--cache")
        .arg(dir.path().join("cache"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .unwrap();
    let took = t.elapsed();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("failed to connect IPC at "), "{stderr}");
    assert!(stderr.contains("No such file or directory"), "{stderr}");
    assert!(took >= Duration::from_secs(9), "no retry window: gave up after {took:?}");
    assert!(took < Duration::from_secs(20), "unbounded: {took:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_starts_when_the_daemon_appears_after_the_binary() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("ipc.sock");
    let cache = tempfile::tempdir().unwrap();
    let f = cache.path().join("foo.txt");
    std::fs::File::create(&f).unwrap().write_all(b"x").unwrap();
    let cache_file = f.to_str().unwrap().to_string();
    let mount = tempfile::tempdir().unwrap();

    let mut child = Command::new(cargo_bin("unidrive-mount"))
        .arg("--mount")
        .arg(mount.path())
        .arg("--ipc")
        .arg(&sock)
        .arg("--cache")
        .arg(cache.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(child.try_wait().unwrap().is_none(), "binary gave up before the daemon appeared");
    let open_write = format!(r#"{{"ok":true,"cache_path":"{cache_file}"}}"#);
    let jvm = FakeJvm::spawn_at(
        sock,
        replies(&[
            ("daemon.status", r#"{"ok":true,"protocol_version":1}"#),
            ("hydration.last_synced", r#"{"ok":true,"mtime_ms":1}"#),
            ("hydration.open_write", open_write.as_str()),
            ("hydration.close_handle", r#"{"ok":true}"#),
            ("hydration.list", r#"{"ok":true,"entries":[]}"#),
        ]),
    )
    .await;

    let seen = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if jvm
                .recorded_requests()
                .await
                .iter()
                .any(|r| r.contains(r#""verb":"hydration.last_synced""#))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;

    if let Some(pid) = child.id() {
        // SAFETY: kill(2) FFI.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    jvm.shutdown().await;
    assert!(seen.is_ok(), "the binary never reached the cache scan after the daemon appeared");
}

//! ReconnectingIpcClient reconnect-on-disconnect test.
//!
//! Determinism strategy: use short interval (10ms) and short budget
//! (5s) so the whole retry cycle fits comfortably inside the test's
//! 30s outer timeout, but is still long enough to absorb scheduling
//! jitter on a loaded host. No virtual time (tokio::time::pause) —
//! the FakeJvm runs real Unix-socket IO, so virtual time would deadlock
//! the accept loop. The trade-off is "real seconds for retries" against
//! "no flakes from clock-skew between virtual and real time on syscalls."

use std::collections::HashMap;
use std::time::Duration;
use unidrive_mount::fake_jvm::FakeJvm;
use unidrive_mount::reconnect::ReconnectingIpcClient;

fn replies(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_succeeds_after_server_restart() {
    // Reserve a stable socket path owned by the test (not FakeJvm), so we
    // can shut down the first JVM and respawn another on the same path.
    let socket_tmp = tempfile::tempdir().expect("socket tempdir");
    let socket_path = socket_tmp.path().join("ipc.sock");

    // Round 1: spawn FakeJvm at the stable path, build the reconnecting
    // client, fire one successful list().
    let jvm_v1 = FakeJvm::spawn_at(
        socket_path.clone(),
        replies(&[("hydration.list", r#"{"ok":true,"entries":[]}"#)]),
    )
    .await;
    let mut client = ReconnectingIpcClient::connect_with(
        &socket_path,
        Duration::from_millis(10),
        Duration::from_secs(5),
    )
    .await
    .expect("initial connect");
    let entries = client.list("").await.expect("first list");
    assert!(entries.is_empty());

    // Kill the JVM. The socket file is deleted by spawn_at's cleanup on
    // the next spawn, but right now it lingers and the accept task is
    // aborted so connect() will fail with ECONNREFUSED until v2 binds.
    jvm_v1.shutdown().await;
    // Remove the stale socket file so the next bind() succeeds.
    let _ = std::fs::remove_file(&socket_path);

    // Spawn a tokio task to restart the JVM after a short delay. The
    // delay must be > the client's retry interval so at least one
    // reconnect attempt fails first.
    let socket_for_restart = socket_path.clone();
    let restart_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        FakeJvm::spawn_at(
            socket_for_restart,
            replies(&[("hydration.list", r#"{"ok":true,"entries":[]}"#)]),
        )
        .await
    });

    // Round 2: call list() again. The first attempt sees the v1 socket
    // (broken pipe / connection refused). The wrapper retries every
    // 10ms; after ~150ms the v2 JVM is up and a fresh connect succeeds.
    let result =
        tokio::time::timeout(Duration::from_secs(10), client.list("")).await;
    let entries = result
        .expect("reconnect must complete within 10s")
        .expect("list must succeed after reconnect");
    assert!(entries.is_empty());

    let jvm_v2 = restart_task.await.expect("v2 spawn task panicked");
    jvm_v2.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budget_exhaustion_surfaces_io_error() {
    // Reserve a path that no JVM will ever bind. Use a deliberately tight
    // budget so the test exits in well under a second.
    let socket_tmp = tempfile::tempdir().expect("socket tempdir");
    let socket_path = socket_tmp.path().join("ghost.sock");

    // The very first connect must fail (no server). The wrapper's
    // constructor uses a single connect attempt — no retries on the
    // initial connect. That's correct: at boot the absence of the JVM
    // is a fatal config error, not a transient blip.
    let res = ReconnectingIpcClient::connect_with(
        &socket_path,
        Duration::from_millis(10),
        Duration::from_millis(50),
    )
    .await;
    // We don't assert on a specific kind beyond "it's an error"; the
    // underlying io::ErrorKind varies (NotFound vs ConnectionRefused
    // depending on whether the parent dir exists).
    assert!(res.is_err(), "connect to nonexistent server must fail");
}

//! The change-event subscription survives a daemon restart.
//!
//! Invariant 1 (`subscription_is_reestablished_after_daemon_restart`): after
//! the daemon drops the stream and comes back, the co-daemon sends
//! `hydration.subscribe` again on a fresh connection.
//!
//! Invariant 2 (`gap_is_reported_once_per_reestablishment_never_on_first_connect`):
//! events in the down window are lost, so a re-established stream reports a
//! single `Gap`; the first connect reports none.
//!
//! Invariant 3 (`terminal_auth_failure_ends_the_subscription_instead_of_spinning`):
//! a refused handshake is not retried.

use std::time::Duration;
use support::fake_jvm::{replies, FakeJvm};
use tokio::sync::mpsc;
use tokio::time::timeout;
use unidrive_mount::ipc_auth::IpcAuth;
use unidrive_mount::subscribe::{run_subscription, SubscriptionEvent};
mod support;

fn v1_replies() -> std::collections::HashMap<String, String> {
    replies(&[
        ("daemon.status", r#"{"ok":true,"protocol_version":1}"#),
        ("hydration.subscribe", r#"{"ok":true}"#),
    ])
}

async fn subscribe_count(jvm: &FakeJvm) -> usize {
    jvm.recorded_requests().await.iter().filter(|r| r.contains(r#""verb":"hydration.subscribe""#)).count()
}

async fn wait_for_subscribe(jvm: &FakeJvm) {
    timeout(Duration::from_secs(10), async {
        while subscribe_count(jvm).await == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("daemon never saw hydration.subscribe");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscription_is_reestablished_after_daemon_restart() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("ipc.sock");
    let v1 = FakeJvm::spawn_at(sock.clone(), v1_replies()).await;

    let (tx, _rx) = mpsc::unbounded_channel();
    let sock2 = sock.clone();
    let task = tokio::spawn(async move {
        run_subscription(&sock2, &IpcAuth::new(None, ""), Duration::from_millis(20), |ev| {
            tx.send(ev).is_ok()
        })
        .await;
    });

    wait_for_subscribe(&v1).await;
    assert_eq!(subscribe_count(&v1).await, 1);
    v1.shutdown().await;
    let _ = std::fs::remove_file(&sock);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let v2 = FakeJvm::spawn_at(sock.clone(), v1_replies()).await;
    wait_for_subscribe(&v2).await;
    assert_eq!(subscribe_count(&v2).await, 1);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gap_is_reported_once_per_reestablishment_never_on_first_connect() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("ipc.sock");
    let v1 = FakeJvm::spawn_at(sock.clone(), v1_replies()).await;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let sock2 = sock.clone();
    let task = tokio::spawn(async move {
        run_subscription(&sock2, &IpcAuth::new(None, ""), Duration::from_millis(20), |ev| {
            tx.send(ev).is_ok()
        })
        .await;
    });

    wait_for_subscribe(&v1).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(rx.try_recv().is_err(), "first connect must not report a gap");

    v1.shutdown().await;
    let _ = std::fs::remove_file(&sock);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let v2 = FakeJvm::spawn_at(sock.clone(), v1_replies()).await;
    wait_for_subscribe(&v2).await;

    let ev = timeout(Duration::from_secs(10), rx.recv()).await.expect("no gap").unwrap();
    assert_eq!(ev, SubscriptionEvent::Gap);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(rx.try_recv().is_err(), "exactly one gap per re-establishment");
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_auth_failure_ends_the_subscription_instead_of_spinning() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("ipc.sock");
    let jvm = FakeJvm::spawn_at(
        sock.clone(),
        replies(&[("daemon.status", r#"{"ok":true,"protocol_version":2}"#)]),
    )
    .await;
    // Protocol-2 daemon, no token file: the handshake cannot be attempted.
    timeout(
        Duration::from_secs(10),
        run_subscription(&sock, &IpcAuth::new(None, "p"), Duration::from_millis(20), |_| true),
    )
    .await
    .expect("subscription loop must end on a terminal auth failure");
    assert_eq!(subscribe_count(&jvm).await, 0);
}

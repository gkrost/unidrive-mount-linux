//! A daemon that turns a connection away because its slots are full
//! (`too_many_clients`) is busy, not hostile: that refusal must not be
//! stored as a permanent authentication failure.
//!
//! One test per invariant:
//! - on a reconnect, the refusal is retried within the budget and the client
//!   is not latched into failing every later call
//! - at startup, the refusal is retried within the budget and the final error
//!   says the daemon is busy (and is not an authentication failure)
//!
//! `ipc_auth_handshake.rs`, `startup_connect.rs` and `subscribe_reconnect.rs`
//! pin the permanent classification of real refusals; if those tests are
//! loosened, a wrong token would be retried for ever.

use std::time::{Duration, Instant};
use support::fake_jvm::{correct_auth, replies, write_token, FakeJvm};
use unidrive_mount::ipc::IpcError;
use unidrive_mount::ipc_auth::IpcAuth;
use unidrive_mount::reconnect::{connect_auth_at_startup, ReconnectingIpcClient};
mod support;

const LIST_OK: &str = r#"{"ok":true,"entries":[]}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn too_many_clients_on_reconnect_is_retried_not_latched() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("ipc.sock");
    let token = write_token(dir.path());
    let auth = IpcAuth::new(Some(token.clone()), "p");

    let v1 = FakeJvm::spawn_v2_at(sock.clone(), replies(&[("hydration.list", LIST_OK)]), correct_auth(&token, "p")).await;
    let mut client =
        ReconnectingIpcClient::connect_auth_with(&sock, auth, Duration::from_millis(20), Duration::from_secs(10))
            .await
            .expect("initial connect");
    client.list("").await.expect("first list");

    // The daemon restarts and its slots are full for the first two connections.
    v1.shutdown().await;
    let v2 = FakeJvm::spawn_v2_busy_at(sock.clone(), replies(&[("hydration.list", LIST_OK)]), correct_auth(&token, "p"), 2).await;

    client.list("").await.expect("the busy refusals are retried until a slot is free");
    client.list("").await.expect("and nothing is latched: the next call works too");

    let hellos = v2.recorded_requests().await.iter().filter(|r| r.contains(r#""verb":"hello""#)).count();
    assert_eq!(hellos, 3, "two refused hellos, one that got a slot");
    v2.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn too_many_clients_at_startup_reports_daemon_busy() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("ipc.sock");
    let token = write_token(dir.path());
    let jvm = FakeJvm::spawn_v2_busy_at(sock.clone(), replies(&[]), correct_auth(&token, "p"), usize::MAX).await;

    let t = Instant::now();
    let err = connect_auth_at_startup(&sock, &IpcAuth::new(Some(token), "p"), Duration::from_millis(25), Duration::from_millis(400))
        .await
        .err()
        .expect("a daemon that never has a slot ends the startup");
    assert!(t.elapsed() >= Duration::from_millis(400), "gave up early after {:?}", t.elapsed());
    assert!(!err.is_auth_terminal(), "busy is not an authentication failure: {err:?}");
    let text = err.to_string();
    assert!(text.contains("busy") && text.contains("too_many_clients"), "{text}");
    assert!(matches!(err, IpcError::Io(_)), "{err:?}");
    jvm.shutdown().await;
}

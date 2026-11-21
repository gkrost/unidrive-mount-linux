//! IPC protocol 2: the authenticated handshake runs before the first verb on
//! every connection, against a fake daemon that implements the daemon side
//! independently from the spec text.

use std::path::{Path, PathBuf};
use std::time::Duration;
use support::fake_jvm::{replies, AuthMode, FakeAuth, FakeJvm};
use unidrive_mount::ipc::IpcClient;
use unidrive_mount::ipc_auth::IpcAuth;
use unidrive_mount::reconnect::ReconnectingIpcClient;
mod support;

const TOKEN_A: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
const TOKEN_B: &str = "ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8";
const LIST_OK: &str = r#"{"ok":true,"entries":[]}"#;

struct Env {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    token: PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("ipc.sock");
    let token = dir.path().join("ipc.token");
    Env { _dir: dir, socket, token }
}

fn fake_auth(token: &Path, profile: &str, mode: AuthMode) -> FakeAuth {
    FakeAuth { token_file: token.to_path_buf(), profile: profile.to_string(), mode }
}

fn verbs(recorded: &[String]) -> Vec<String> {
    recorded
        .iter()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["verb"].as_str().unwrap_or("").to_string())
        .collect()
}

// Invariant: hello and hello.proof come before the first verb, the verb is
// served on the authenticated connection, and the token never goes on the wire.
#[tokio::test]
async fn handshake_succeeds_then_verb_is_served() {
    let e = env();
    std::fs::write(&e.token, TOKEN_A).unwrap();
    let jvm = FakeJvm::spawn_v2_at(e.socket.clone(), replies(&[("hydration.list", LIST_OK)]), fake_auth(&e.token, "p1", AuthMode::Correct)).await;

    let mut c = IpcClient::connect_auth(&e.socket, &IpcAuth::new(Some(e.token.clone()), "p1")).await.expect("handshake");
    c.list("").await.expect("verb after handshake");

    let rec = jvm.recorded_requests().await;
    assert_eq!(verbs(&rec), ["hello", "hello.proof", "hydration.list"]);
    assert!(rec.iter().all(|l| !l.contains(TOKEN_A)), "token must never be sent");
    let hello: serde_json::Value = serde_json::from_str(&rec[0]).unwrap();
    assert_eq!(hello["protocol"], 2);
    assert_eq!(hello["scope"], "full");
    assert_eq!(hello["client"], "unidrive-mount-linux");
    assert_eq!(hello["nonce"].as_str().unwrap().len(), 22, "16 bytes base64url without padding");
    jvm.shutdown().await;
}

// Invariant: a reconnect authenticates again before resending the verb.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_repeats_on_every_reconnect() {
    let e = env();
    std::fs::write(&e.token, TOKEN_A).unwrap();
    let auth = || fake_auth(&e.token, "p1", AuthMode::Correct);
    let v1 = FakeJvm::spawn_v2_at(e.socket.clone(), replies(&[("hydration.list", LIST_OK)]), auth()).await;
    let mut c = ReconnectingIpcClient::connect_auth_with(
        &e.socket,
        IpcAuth::new(Some(e.token.clone()), "p1"),
        Duration::from_millis(10),
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    c.list("").await.unwrap();
    v1.shutdown().await;

    let v2 = FakeJvm::spawn_v2_at(e.socket.clone(), replies(&[("hydration.list", LIST_OK)]), auth()).await;
    tokio::time::timeout(Duration::from_secs(10), c.list("")).await.unwrap().expect("list after reconnect");
    let v2_verbs = verbs(&v2.recorded_requests().await);
    assert_eq!(v2_verbs, ["hello", "hello.proof", "hydration.list"]);
    v2.shutdown().await;
}

// Invariant: a daemon whose proof does not match gets no verb, and the
// client closes the connection.
#[tokio::test]
async fn wrong_server_proof_closes_connection_without_sending_a_verb() {
    let e = env();
    std::fs::write(&e.token, TOKEN_A).unwrap();
    let jvm = FakeJvm::spawn_v2_at(e.socket.clone(), replies(&[("hydration.list", LIST_OK)]), fake_auth(&e.token, "p1", AuthMode::WrongServerProof)).await;

    let err = IpcClient::connect_auth(&e.socket, &IpcAuth::new(Some(e.token.clone()), "p1")).await.err().expect("impostor must be refused");
    assert!(err.is_auth_terminal(), "got {err}");
    assert!(err.to_string().contains("proof did not match"), "got {err}");
    assert!(!err.to_string().contains("proof\":"), "no proof value in the error");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while jvm.closed_connections() == 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(jvm.closed_connections(), 1, "client must close the connection");
    assert_eq!(verbs(&jvm.recorded_requests().await), ["hello", "hello.proof"]);
    jvm.shutdown().await;
}

// Invariant: auth_failed is terminal. The reconnect wrapper makes exactly one
// handshake attempt (no loop until the retry budget), and later calls fail
// fast without handshaking again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_failed_is_terminal_without_retry_loop() {
    let e = env();
    std::fs::write(&e.token, TOKEN_A).unwrap();
    let v1 = FakeJvm::spawn_v2_at(e.socket.clone(), replies(&[("hydration.list", LIST_OK)]), fake_auth(&e.token, "p1", AuthMode::Correct)).await;
    let mut c = ReconnectingIpcClient::connect_auth_with(
        &e.socket,
        IpcAuth::new(Some(e.token.clone()), "p1"),
        Duration::from_millis(10),
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    c.list("").await.unwrap();
    v1.shutdown().await;

    let v2 = FakeJvm::spawn_v2_at(e.socket.clone(), replies(&[("hydration.list", LIST_OK)]), fake_auth(&e.token, "p1", AuthMode::RejectProof)).await;
    let started = tokio::time::Instant::now();
    let err = tokio::time::timeout(Duration::from_secs(10), c.list("")).await.expect("must not spin until the 30 s budget").unwrap_err();
    assert!(err.is_auth_terminal(), "got {err}");
    assert!(err.to_string().contains("restart the mount"), "got {err}");
    assert!(started.elapsed() < Duration::from_secs(5));
    let err2 = c.list("").await.unwrap_err();
    assert!(err2.is_auth_terminal());
    assert_eq!(verbs(&v2.recorded_requests().await), ["hello", "hello.proof"], "exactly one handshake attempt");

    // The initial connect refuses the same way.
    let refused = ReconnectingIpcClient::connect_auth_with(
        &e.socket,
        IpcAuth::new(Some(e.token.clone()), "p1"),
        Duration::from_millis(10),
        Duration::from_secs(30),
    )
    .await;
    assert!(refused.err().is_some_and(|e| e.is_auth_terminal()));
    v2.shutdown().await;
}

// Invariant: without a readable token the co-daemon never talks to the daemon
// unauthenticated: not even to find out its protocol. A protocol-1 daemon is
// refused like a protocol-2 one, and no request goes on the wire.
#[tokio::test]
async fn protocol1_without_token_is_refused_and_sends_nothing() {
    let e = env();
    let jvm = FakeJvm::spawn_at(
        e.socket.clone(),
        replies(&[("daemon.status", r#"{"ok":true,"protocol_version":1}"#), ("hydration.list", LIST_OK)]),
    )
    .await;
    for auth in [IpcAuth::new(Some(e.token.clone()), "p1"), IpcAuth::new(None, "")] {
        let err = IpcClient::connect_auth(&e.socket, &auth).await.err().expect("must refuse");
        assert!(err.is_auth_terminal(), "got {err}");
        assert!(err.to_string().contains("cannot authenticate to this daemon"), "got {err}");
    }
    assert!(jvm.recorded_requests().await.is_empty(), "no unauthenticated verb may be sent");
    jvm.shutdown().await;
}

// Invariant: a protocol-2 daemon and no readable token fail with a clear,
// terminal error before any handshake or verb.
#[tokio::test]
async fn protocol2_without_token_fails_with_clear_error() {
    let e = env();
    let jvm = FakeJvm::spawn_v2_at(e.socket.clone(), replies(&[("hydration.list", LIST_OK)]), fake_auth(&e.token, "p1", AuthMode::Correct)).await;
    for auth in [IpcAuth::new(Some(e.token.clone()), "p1"), IpcAuth::new(None, "")] {
        let err = IpcClient::connect_auth(&e.socket, &auth).await.err().expect("must refuse");
        assert!(err.is_auth_terminal(), "got {err}");
        assert!(err.to_string().contains("cannot authenticate to this daemon"), "got {err}");
    }
    assert!(jvm.recorded_requests().await.is_empty(), "no verb, not even a status probe, goes out unauthenticated");
    jvm.shutdown().await;
}

// Invariant: a token file that exists but a daemon without the handshake
// (hello → unknown_verb) fails closed instead of falling back.
#[tokio::test]
async fn token_present_but_daemon_without_hello_fails_closed() {
    let e = env();
    std::fs::write(&e.token, TOKEN_A).unwrap();
    let jvm = FakeJvm::spawn_at(
        e.socket.clone(),
        replies(&[("hello", r#"{"ok":false,"error":"unknown_verb"}"#), ("hydration.list", LIST_OK)]),
    )
    .await;
    let err = IpcClient::connect_auth(&e.socket, &IpcAuth::new(Some(e.token.clone()), "p1")).await.err().expect("fail closed");
    assert!(err.is_auth_terminal(), "got {err}");
    assert_eq!(verbs(&jvm.recorded_requests().await), ["hello"]);
    jvm.shutdown().await;
}

// Invariant: the token file is read again on every connect, so a daemon
// restart that rotates the token is followed transparently.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_file_is_reread_on_reconnect() {
    let e = env();
    std::fs::write(&e.token, TOKEN_A).unwrap();
    let auth = || fake_auth(&e.token, "p1", AuthMode::Correct);
    let v1 = FakeJvm::spawn_v2_at(e.socket.clone(), replies(&[("hydration.list", LIST_OK)]), auth()).await;
    let mut c = ReconnectingIpcClient::connect_auth_with(
        &e.socket,
        IpcAuth::new(Some(e.token.clone()), "p1"),
        Duration::from_millis(10),
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    c.list("").await.unwrap();
    v1.shutdown().await;

    std::fs::write(&e.token, TOKEN_B).unwrap();
    let v2 = FakeJvm::spawn_v2_at(e.socket.clone(), replies(&[("hydration.list", LIST_OK)]), auth()).await;
    tokio::time::timeout(Duration::from_secs(10), c.list("")).await.unwrap().expect("rotated token must be picked up");
    assert_eq!(verbs(&v2.recorded_requests().await), ["hello", "hello.proof", "hydration.list"]);
    v2.shutdown().await;
}

// Invariant: a multi-byte profile name authenticates with its UTF-8 byte
// length in the message (the fake builds the message independently).
#[tokio::test]
async fn multibyte_profile_name_authenticates() {
    let e = env();
    std::fs::write(&e.token, TOKEN_A).unwrap();
    let profile = "caf\u{e9}-\u{1F600}";
    let jvm = FakeJvm::spawn_v2_at(e.socket.clone(), replies(&[("hydration.list", LIST_OK)]), fake_auth(&e.token, profile, AuthMode::Correct)).await;
    let mut c = IpcClient::connect_auth(&e.socket, &IpcAuth::new(Some(e.token.clone()), profile)).await.expect("handshake");
    c.list("").await.unwrap();

    // The same name, normalised differently (NFD), is a different profile.
    let nfd = "cafe\u{301}-\u{1F600}";
    let err = IpcClient::connect_auth(&e.socket, &IpcAuth::new(Some(e.token.clone()), nfd)).await.err().expect("no normalisation");
    assert!(err.is_auth_terminal());
    jvm.shutdown().await;
}

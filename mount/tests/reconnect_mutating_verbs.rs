//! Tests for the non-idempotent-verb replay guard in ReconnectingIpcClient.
//!
//! Invariant 1 (`mutating_verb_is_not_replayed_after_io_post_send`):
//!   When a mutating verb (rename, unlink, rmdir, mkdir, create, open_write,
//!   open_write_begin) is sent to the JVM and the connection drops before a
//!   reply arrives, the wrapper must NOT re-send the request on the same or a
//!   new connection. The outcome of the first send is unknown (the JVM may
//!   have already acted on it), so a silent replay would corrupt state.
//!
//! Invariant 2 (`idempotent_read_is_retried_across_reconnect`):
//!   Idempotent reads (list, open_read, last_synced, hydrate, dehydrate) are
//!   safe to replay and MUST be retried across a reconnect so that transient
//!   daemon restarts don't surface spurious EIO to userland.
//!
//! Fixture design: instead of FakeJvm (which always writes a canned reply),
//! these tests drive a minimal inline server via raw tokio UDS tasks to get
//! fine-grained control over when the connection is closed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use support::fake_jvm::{replies, FakeJvm};
use unidrive_mount::ipc::IpcError;
use unidrive_mount::reconnect::ReconnectingIpcClient;
mod support;


/// A server that handles exactly N requests normally (writing canned replies),
/// then on request N+1 it reads the line (simulating "acted on the request")
/// and drops the connection WITHOUT writing a reply. The request body of the
/// dropped call is recorded so tests can verify whether a second attempt was
/// made.
struct DropAfterNServer {
    socket_path: std::path::PathBuf,
    received: Arc<Mutex<Vec<String>>>,
    _tempdir: tempfile::TempDir,
}

impl DropAfterNServer {
    /// `normal_replies`: verb → reply JSON for normal (non-dropped) requests.
    /// `drop_after`: how many requests to answer normally before dropping.
    async fn spawn(
        normal_replies: HashMap<String, String>,
        drop_after: usize,
    ) -> Self {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let socket_path = tempdir.path().join("drop-server.sock");
        let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let received_clone = Arc::clone(&received);
        let path_clone = socket_path.clone();

        let listener = UnixListener::bind(&path_clone).expect("bind UDS");
        let replies = Arc::new(normal_replies);

        tokio::spawn(async move {
            let mut served_total: usize = 0;
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let received = Arc::clone(&received_clone);
                let replies = Arc::clone(&replies);
                // Capture how many requests have been fully answered so far at
                // the time this connection is accepted.
                let served_before_this_conn = served_total;
                // We only expect one connection for the drop scenario, so track
                // state across the single connection's loop.
                let drop_after_copy = drop_after;
                drop(tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let mut reader = BufReader::new(r);
                    let mut served_on_conn: usize = 0;
                    loop {
                        let mut line = String::new();
                        let n = match reader.read_line(&mut line).await {
                            Ok(n) => n,
                            Err(_) => return,
                        };
                        if n == 0 {
                            return;
                        }
                        let trimmed = line.trim_end_matches('\n').to_string();
                        received.lock().await.push(trimmed.clone());

                        let total_so_far = served_before_this_conn + served_on_conn;
                        if total_so_far >= drop_after_copy {
                            // Simulate "acted on request, then closed without reply"
                            // by simply dropping `w` (and returning, closing `r`).
                            return;
                        }

                        // Normal reply.
                        let verb = extract_verb(&trimmed);
                        let reply = match verb.as_deref().and_then(|v| replies.get(v)) {
                            Some(r) => r.clone(),
                            None => r#"{"ok":false,"error":"no_canned_reply"}"#.to_string(),
                        };
                        let mut out = reply.into_bytes();
                        out.push(b'\n');
                        if w.write_all(&out).await.is_err() {
                            return;
                        }
                        if w.flush().await.is_err() {
                            return;
                        }
                        served_on_conn += 1;
                        // Update `served_total` — because we've moved `served_before_this_conn`
                        // into this closure, we can't mutate the outer. That's fine: in these
                        // tests there's only one connection active at a time and we don't need
                        // cross-connection tracking after the drop.
                    }
                }));
                // We only use one connection in the drop scenario.
                // Accept loop exits naturally when the listener drops.
                served_total = drop_after; // prevent any further "normal" replies
            }
        });

        DropAfterNServer { socket_path, received, _tempdir: tempdir }
    }

    async fn received_requests(&self) -> Vec<String> {
        self.received.lock().await.clone()
    }
}

fn extract_verb(line: &str) -> Option<String> {
    let key = "\"verb\"";
    let k = line.find(key)?;
    let after_key = &line[k + key.len()..];
    let colon = after_key.find(':')?;
    let after_colon = &after_key[colon + 1..];
    let q1 = after_colon.find('"')?;
    let after_q1 = &after_colon[q1 + 1..];
    let q2 = after_q1.find('"')?;
    Some(after_q1[..q2].to_string())
}

// ──────────────────────────────────────────────────────────────────────────────
// Invariant 1: mutating verb must NOT be replayed after Io-post-send
// ──────────────────────────────────────────────────────────────────────────────

/// The JVM receives the `rename` request (simulating it was acted upon), then
/// drops the connection without replying. The wrapper must:
///   - Surface an error to the caller (not silently succeed),
///   - NOT re-send the rename on a new connection (no second attempt in
///     recorded_requests).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutating_verb_is_not_replayed_after_io_post_send() {
    // Server answers 0 normal requests then drops on the first mutating verb.
    let server = DropAfterNServer::spawn(HashMap::new(), 0).await;

    let mut client = ReconnectingIpcClient::connect_with(
        &server.socket_path,
        Duration::from_millis(10),
        Duration::from_secs(5),
    )
    .await
    .expect("initial connect");

    // rename is non-idempotent: if first send was acted on, replaying would
    // surface old_path_not_found (spurious ENOENT).
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.rename("/a/old.txt", "/a/new.txt"),
    )
    .await
    .expect("rename must not hang — must return promptly with an error");

    // Must be an error (not silent success on a second attempt).
    let err = result.expect_err("rename must return an error when the connection drops after the request is sent");
    assert!(matches!(err, IpcError::Io(_)), "expected IpcError::Io, got {err:?}");

    // Critical: exactly ONE rename request must have been received by the server.
    // Two would mean the wrapper replayed the mutation.
    let received = server.received_requests().await;
    let rename_count = received
        .iter()
        .filter(|r| r.contains(r#""verb":"hydration.rename""#))
        .count();
    assert_eq!(
        rename_count, 1,
        "rename must be sent exactly once — replay would be a second attempt; got requests: {received:?}"
    );
}

/// Same invariant for `unlink` — another non-idempotent verb that would return
/// ENOENT on replay if the first attempt already deleted the path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutating_verb_unlink_is_not_replayed_after_io_post_send() {
    let server = DropAfterNServer::spawn(HashMap::new(), 0).await;

    let mut client = ReconnectingIpcClient::connect_with(
        &server.socket_path,
        Duration::from_millis(10),
        Duration::from_secs(5),
    )
    .await
    .expect("initial connect");

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.unlink("/a/file.txt"),
    )
    .await
    .expect("unlink must not hang");

    let err = result.expect_err("unlink must return an error");
    assert!(matches!(err, IpcError::Io(_)), "expected IpcError::Io, got {err:?}");

    let received = server.received_requests().await;
    let count = received
        .iter()
        .filter(|r| r.contains(r#""verb":"hydration.unlink""#))
        .count();
    assert_eq!(count, 1, "unlink must be sent exactly once; got: {received:?}");
}

/// Same invariant for `mkdir`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutating_verb_mkdir_is_not_replayed_after_io_post_send() {
    let server = DropAfterNServer::spawn(HashMap::new(), 0).await;

    let mut client = ReconnectingIpcClient::connect_with(
        &server.socket_path,
        Duration::from_millis(10),
        Duration::from_secs(5),
    )
    .await
    .expect("initial connect");

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.mkdir("/newdir"),
    )
    .await
    .expect("mkdir must not hang");

    let err = result.expect_err("mkdir must return an error");
    assert!(matches!(err, IpcError::Io(_)), "expected IpcError::Io, got {err:?}");

    let received = server.received_requests().await;
    let count = received
        .iter()
        .filter(|r| r.contains(r#""verb":"hydration.mkdir""#))
        .count();
    assert_eq!(count, 1, "mkdir must be sent exactly once; got: {received:?}");
}

/// Same invariant for `rmdir`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutating_verb_rmdir_is_not_replayed_after_io_post_send() {
    let server = DropAfterNServer::spawn(HashMap::new(), 0).await;

    let mut client = ReconnectingIpcClient::connect_with(
        &server.socket_path,
        Duration::from_millis(10),
        Duration::from_secs(5),
    )
    .await
    .expect("initial connect");

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.rmdir("/olddir"),
    )
    .await
    .expect("rmdir must not hang");

    let err = result.expect_err("rmdir must return an error");
    assert!(matches!(err, IpcError::Io(_)), "expected IpcError::Io, got {err:?}");

    let received = server.received_requests().await;
    let count = received
        .iter()
        .filter(|r| r.contains(r#""verb":"hydration.rmdir""#))
        .count();
    assert_eq!(count, 1, "rmdir must be sent exactly once; got: {received:?}");
}

/// Same invariant for `create`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutating_verb_create_is_not_replayed_after_io_post_send() {
    let server = DropAfterNServer::spawn(HashMap::new(), 0).await;

    let mut client = ReconnectingIpcClient::connect_with(
        &server.socket_path,
        Duration::from_millis(10),
        Duration::from_secs(5),
    )
    .await
    .expect("initial connect");

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.create("h1", "/new/file.txt"),
    )
    .await
    .expect("create must not hang");

    let err = result.expect_err("create must return an error");
    assert!(matches!(err, IpcError::Io(_)), "expected IpcError::Io, got {err:?}");

    let received = server.received_requests().await;
    let count = received
        .iter()
        .filter(|r| r.contains(r#""verb":"hydration.create""#))
        .count();
    assert_eq!(count, 1, "create must be sent exactly once; got: {received:?}");
}

/// Same invariant for `open_write`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutating_verb_open_write_is_not_replayed_after_io_post_send() {
    let server = DropAfterNServer::spawn(HashMap::new(), 0).await;

    let mut client = ReconnectingIpcClient::connect_with(
        &server.socket_path,
        Duration::from_millis(10),
        Duration::from_secs(5),
    )
    .await
    .expect("initial connect");

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.open_write("h1", "/a/file.txt", "/tmp/cache/file.txt"),
    )
    .await
    .expect("open_write must not hang");

    let err = result.expect_err("open_write must return an error");
    assert!(matches!(err, IpcError::Io(_)), "expected IpcError::Io, got {err:?}");

    let received = server.received_requests().await;
    // Match "hydration.open_write" but not "hydration.open_write_begin".
    let count = received
        .iter()
        .filter(|r| r.contains(r#""verb":"hydration.open_write""#) && !r.contains("open_write_begin"))
        .count();
    assert_eq!(count, 1, "open_write must be sent exactly once; got: {received:?}");
}

/// Same invariant for `open_write_begin`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutating_verb_open_write_begin_is_not_replayed_after_io_post_send() {
    let server = DropAfterNServer::spawn(HashMap::new(), 0).await;

    let mut client = ReconnectingIpcClient::connect_with(
        &server.socket_path,
        Duration::from_millis(10),
        Duration::from_secs(5),
    )
    .await
    .expect("initial connect");

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.open_write_begin("/a/file.txt", None),
    )
    .await
    .expect("open_write_begin must not hang");

    let err = result.expect_err("open_write_begin must return an error");
    assert!(matches!(err, IpcError::Io(_)), "expected IpcError::Io, got {err:?}");

    let received = server.received_requests().await;
    let count = received
        .iter()
        .filter(|r| r.contains(r#""verb":"hydration.open_write_begin""#))
        .count();
    assert_eq!(count, 1, "open_write_begin must be sent exactly once; got: {received:?}");
}

// ──────────────────────────────────────────────────────────────────────────────
// Invariant 2: idempotent reads ARE retried across a reconnect
// ──────────────────────────────────────────────────────────────────────────────

/// `list` (idempotent read) must be transparently retried after a reconnect.
/// Regression guard: don't break the existing legitimate retry behavior while
/// fixing the mutation replay bug.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idempotent_read_is_retried_across_reconnect() {
    // Use stable socket path so we can respawn the server.
    let socket_tmp = tempfile::tempdir().expect("socket tempdir");
    let socket_path = socket_tmp.path().join("ipc.sock");

    // v1: serves one `list` successfully, then shuts down.
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

    // First call succeeds.
    let first = client.list("").await.expect("first list must succeed");
    assert!(first.is_empty());

    // Kill v1.
    jvm_v1.shutdown().await;
    let _ = std::fs::remove_file(&socket_path);

    // Arm v2 after a short delay (longer than retry interval so at least one
    // attempt fails with ECONNREFUSED before v2 is ready).
    let socket_for_restart = socket_path.clone();
    let restart_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        FakeJvm::spawn_at(
            socket_for_restart,
            replies(&[("hydration.list", r#"{"ok":true,"entries":[]}"#)]),
        )
        .await
    });

    // Second list must succeed via reconnect — no error must surface to caller.
    let second = tokio::time::timeout(Duration::from_secs(10), client.list(""))
        .await
        .expect("list must complete within 10s after reconnect")
        .expect("list must succeed — idempotent reads must be retried on reconnect");
    assert!(second.is_empty());

    let jvm_v2 = restart_task.await.expect("v2 spawn task panicked");
    jvm_v2.shutdown().await;
}

/// `open_read` (idempotent: it either opens successfully or fails; a re-open
/// on reconnect is safe) must also be retried across a reconnect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idempotent_open_read_is_retried_across_reconnect() {
    let socket_tmp = tempfile::tempdir().expect("socket tempdir");
    let socket_path = socket_tmp.path().join("ipc.sock");

    let open_read_reply = r#"{"ok":true,"cache_path":"/tmp/test-cache/foo.txt"}"#;

    let jvm_v1 = FakeJvm::spawn_at(
        socket_path.clone(),
        replies(&[("hydration.open_read", open_read_reply)]),
    )
    .await;

    let mut client = ReconnectingIpcClient::connect_with(
        &socket_path,
        Duration::from_millis(10),
        Duration::from_secs(5),
    )
    .await
    .expect("initial connect");

    // Warm-up: successful round-trip to confirm connection is live.
    let first = client.open_read("h1", "/foo.txt").await.expect("first open_read must succeed");
    assert_eq!(first.cache_path, std::path::Path::new("/tmp/test-cache/foo.txt"));

    jvm_v1.shutdown().await;
    let _ = std::fs::remove_file(&socket_path);

    let socket_for_restart = socket_path.clone();
    let restart_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        FakeJvm::spawn_at(
            socket_for_restart,
            replies(&[("hydration.open_read", open_read_reply)]),
        )
        .await
    });

    let second = tokio::time::timeout(Duration::from_secs(10), client.open_read("h1", "/foo.txt"))
        .await
        .expect("open_read must complete within 10s after reconnect")
        .expect("open_read must succeed — idempotent reads must be retried on reconnect");
    assert_eq!(second.cache_path, std::path::Path::new("/tmp/test-cache/foo.txt"));

    let jvm_v2 = restart_task.await.expect("v2 spawn task panicked");
    jvm_v2.shutdown().await;
}

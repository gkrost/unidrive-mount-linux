use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Mutex;

/// Test-only fake JVM IPC server. Binds a UDS at a unique temp path,
/// accepts one connection at a time, reads NDJSON request lines, looks
/// up the verb in the supplied replies map, writes the reply line
/// followed by `\n`, and records each received request for assertion.
///
/// Wire framing matches the canonical contract documented in
/// `../unidrive/core/app/sync/src/main/kotlin/org/krost/unidrive/sync/IpcServer.kt`:
/// newline-terminated JSON lines, one request per line, one reply per line.
pub struct FakeJvm {
    pub socket_path: PathBuf,
    accept_task: tokio::task::JoinHandle<()>,
    connection_tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    recorded: Arc<Mutex<Vec<String>>>,
    _tempdir: Option<tempfile::TempDir>,
}

impl FakeJvm {
    /// Bind a UDS at a temp path and start accepting. `replies` is a map of
    /// verb-name → static reply line (no trailing newline; we append one).
    pub async fn spawn(replies: HashMap<String, String>) -> Self {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let socket_path = tempdir.path().join("fake-jvm.sock");
        Self::spawn_inner(socket_path, Some(tempdir), replies).await
    }

    /// Bind a UDS at a caller-supplied path. The caller owns the directory
    /// the socket lives in (e.g. a TempDir the test holds open across
    /// multiple spawn cycles, for reconnect tests). If a stale socket
    /// file exists at the path, it is removed first.
    pub async fn spawn_at(socket_path: PathBuf, replies: HashMap<String, String>) -> Self {
        // Best-effort cleanup of stale socket file from a previous spawn.
        let _ = std::fs::remove_file(&socket_path);
        Self::spawn_inner(socket_path, None, replies).await
    }

    async fn spawn_inner(
        socket_path: PathBuf,
        tempdir: Option<tempfile::TempDir>,
        replies: HashMap<String, String>,
    ) -> Self {
        let listener = UnixListener::bind(&socket_path).expect("bind UDS");

        let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded_clone = Arc::clone(&recorded);
        let replies = Arc::new(replies);
        let connection_tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let connection_tasks_clone = Arc::clone(&connection_tasks);

        let accept_task = tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let recorded = Arc::clone(&recorded_clone);
                let replies = Arc::clone(&replies);
                let h = tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let mut reader = BufReader::new(r);
                    loop {
                        let mut line = String::new();
                        let n = match reader.read_line(&mut line).await {
                            Ok(n) => n,
                            Err(_) => return,
                        };
                        if n == 0 {
                            return; // client closed
                        }
                        let trimmed = line.trim_end_matches('\n').to_string();
                        let verb = extract_verb(&trimmed);
                        recorded.lock().await.push(trimmed.clone());
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
                    }
                });
                connection_tasks_clone.lock().await.push(h);
            }
        });

        FakeJvm {
            socket_path,
            accept_task,
            connection_tasks,
            recorded,
            _tempdir: tempdir,
        }
    }

    pub async fn recorded_requests(&self) -> Vec<String> {
        self.recorded.lock().await.clone()
    }

    pub async fn shutdown(self) {
        self.accept_task.abort();
        let _ = self.accept_task.await;
        // Abort any in-flight connection tasks too — without this, an
        // already-connected client keeps talking to this "shut-down" fake,
        // which doesn't match how a real JVM process kill closes all client
        // connections.
        let handles = {
            let mut g = self.connection_tasks.lock().await;
            std::mem::take(&mut *g)
        };
        for h in handles {
            h.abort();
            let _ = h.await;
        }
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

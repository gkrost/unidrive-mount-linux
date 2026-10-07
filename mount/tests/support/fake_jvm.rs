use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    closed: Arc<AtomicUsize>,
    _tempdir: Option<tempfile::TempDir>,
}

/// How a protocol-2 fake answers the handshake.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AuthMode {
    /// Verify the client proof, answer with the correct server proof.
    Correct,
    /// Accept the client proof but answer with a server proof made with
    /// another key (an impostor daemon).
    WrongServerProof,
    /// Refuse every `hello.proof` with `auth_failed`.
    RejectProof,
}

/// Protocol-2 behaviour of the fake. The token file is read at every
/// `hello.proof`, like a daemon that rotates its token at start.
#[derive(Clone, Debug)]
pub struct FakeAuth {
    pub token_file: PathBuf,
    pub profile: String,
    pub mode: AuthMode,
}

// Independent of the client code under test: the message is built here
// from the spec text, so a shared bug cannot make both sides agree.
fn fake_hmac(key: &[u8], msg: &str) -> Vec<u8> {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).unwrap();
    m.update(msg.as_bytes());
    m.finalize().into_bytes().to_vec()
}

fn fake_key(auth: &FakeAuth) -> Vec<u8> {
    let text = std::fs::read_to_string(&auth.token_file).expect("fake daemon reads its token");
    URL_SAFE_NO_PAD.decode(text.trim()).expect("fake token is base64url")
}

#[derive(Default)]
struct ConnAuth {
    authed: bool,
    pending: Option<(String, String)>,
}

fn auth_step(
    auth: &FakeAuth,
    st: &mut ConnAuth,
    req: &serde_json::Value,
    verb: Option<&str>,
    conn_no: usize,
) -> Option<String> {
    if st.authed {
        return None;
    }
    Some(match verb {
        Some("daemon.status") => {
            r#"{"ok":true,"protocol_version":2,"engine_version":"fake","auth_required":true}"#.to_string()
        }
        Some("hello") => {
            let ok = req["protocol"].as_u64() == Some(2)
                && req["scope"].as_str() == Some("full")
                && req["client"].as_str().is_some_and(|c| !c.is_empty() && c.len() <= 64);
            match (ok, req["nonce"].as_str()) {
                (true, Some(nonce)) => {
                    let mut sn = [0u8; 16];
                    sn[..8].copy_from_slice(&(conn_no as u64).to_be_bytes());
                    let snonce = URL_SAFE_NO_PAD.encode(sn);
                    st.pending = Some((nonce.to_string(), snonce.clone()));
                    serde_json::json!({"ok":true,"step":1,"snonce":snonce}).to_string()
                }
                _ => r#"{"ok":false,"error":"auth_failed"}"#.to_string(),
            }
        }
        Some("hello.proof") => {
            let Some((nonce, snonce)) = st.pending.take() else {
                return Some(r#"{"ok":false,"error":"auth_failed"}"#.to_string());
            };
            let key = fake_key(auth);
            let p = &auth.profile;
            let cmsg = format!("unidrive-ipc-v2|client|full|{}:{p}|{nonce}|{snonce}", p.len());
            let expected = URL_SAFE_NO_PAD.encode(fake_hmac(&key, &cmsg));
            let got = req["proof"].as_str().unwrap_or("");
            if auth.mode == AuthMode::RejectProof || got != expected {
                return Some(r#"{"ok":false,"error":"auth_failed"}"#.to_string());
            }
            st.authed = true;
            let skey = if auth.mode == AuthMode::WrongServerProof { vec![0xAA; 32] } else { key };
            let smsg = format!("unidrive-ipc-v2|server|full|{}:{p}|{nonce}|{snonce}|{got}", p.len());
            let sp = URL_SAFE_NO_PAD.encode(fake_hmac(&skey, &smsg));
            serde_json::json!({"ok":true,"scope":"full","protocol_version":2,"proof":sp}).to_string()
        }
        _ => r#"{"ok":false,"error":"auth_required"}"#.to_string(),
    })
}

impl FakeJvm {
    /// Bind a UDS at a temp path and start accepting. `replies` is a map of
    /// verb-name → static reply line (no trailing newline; we append one).
    pub async fn spawn(replies: HashMap<String, String>) -> Self {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let socket_path = tempdir.path().join("fake-jvm.sock");
        Self::spawn_inner(socket_path, Some(tempdir), replies, None).await
    }

    /// A protocol-2 fake at a caller-supplied path: every verb but `hello`,
    /// `hello.proof` and `daemon.status` gets `auth_required` until the
    /// connection has authenticated.
    pub async fn spawn_v2_at(
        socket_path: PathBuf,
        replies: HashMap<String, String>,
        auth: FakeAuth,
    ) -> Self {
        let _ = std::fs::remove_file(&socket_path);
        Self::spawn_inner(socket_path, None, replies, Some(auth)).await
    }

    /// Bind a UDS at a caller-supplied path. The caller owns the directory
    /// the socket lives in (e.g. a TempDir the test holds open across
    /// multiple spawn cycles, for reconnect tests). If a stale socket
    /// file exists at the path, it is removed first.
    pub async fn spawn_at(socket_path: PathBuf, replies: HashMap<String, String>) -> Self {
        // Best-effort cleanup of stale socket file from a previous spawn.
        let _ = std::fs::remove_file(&socket_path);
        Self::spawn_inner(socket_path, None, replies, None).await
    }

    async fn spawn_inner(
        socket_path: PathBuf,
        tempdir: Option<tempfile::TempDir>,
        replies: HashMap<String, String>,
        auth: Option<FakeAuth>,
    ) -> Self {
        let auth = Arc::new(auth);
        let closed = Arc::new(AtomicUsize::new(0));
        let closed_clone = Arc::clone(&closed);
        let listener = UnixListener::bind(&socket_path).expect("bind UDS");

        let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded_clone = Arc::clone(&recorded);
        let replies = Arc::new(replies);
        let connection_tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let connection_tasks_clone = Arc::clone(&connection_tasks);

        let accept_task = tokio::spawn(async move {
            let mut conn_no = 0usize;
            loop {
                conn_no += 1;
                let (stream, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let recorded = Arc::clone(&recorded_clone);
                let replies = Arc::clone(&replies);
                let auth = Arc::clone(&auth);
                let closed = Arc::clone(&closed_clone);
                let h = tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let mut reader = BufReader::new(r);
                    let mut st = ConnAuth::default();
                    loop {
                        let mut line = String::new();
                        let n = match reader.read_line(&mut line).await {
                            Ok(n) => n,
                            Err(_) => return,
                        };
                        if n == 0 {
                            closed.fetch_add(1, Ordering::SeqCst);
                            return; // client closed
                        }
                        let trimmed = line.trim_end_matches('\n').to_string();
                        let verb = extract_verb(&trimmed);
                        recorded.lock().await.push(trimmed.clone());
                        let gated = match auth.as_ref() {
                            Some(a) => {
                                let req: serde_json::Value =
                                    serde_json::from_str(&trimmed).unwrap_or_default();
                                auth_step(a, &mut st, &req, verb.as_deref(), conn_no)
                            }
                            None => None,
                        };
                        let reply = match gated {
                            Some(r) => r,
                            None => match verb.as_deref().and_then(|v| replies.get(v)) {
                                Some(r) => r.clone(),
                                None => r#"{"ok":false,"error":"no_canned_reply"}"#.to_string(),
                            },
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
            closed,
            _tempdir: tempdir,
        }
    }

    /// Number of connections the client has closed so far.
    pub fn closed_connections(&self) -> usize {
        self.closed.load(Ordering::SeqCst)
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

/// Convenience helper: convert a slice of (verb, reply) pairs into the
/// `HashMap<String, String>` that `FakeJvm::spawn` expects.
pub fn replies(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
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

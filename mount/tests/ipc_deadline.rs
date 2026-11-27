//! A daemon that accepts a connection and then does not answer must turn into
//! an error, not into a client that waits for ever.
//!
//! One test per invariant:
//! - a silent daemon ends a request with `Timeout` after the deadline, and the
//!   connection is not used again (a late reply would be read as the next
//!   request's)
//! - the reconnecting client drops the timed-out connection and the next call
//!   goes out on a fresh one
//! - the verbs that transfer bytes have no deadline: a slow download is not a
//!   dead daemon
//!
//! If the second test is removed or loosened, one stalled reply can be handed
//! to the next caller as its own.

use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use unidrive_mount::ipc::{IpcClient, IpcError};
use unidrive_mount::reconnect::ReconnectingIpcClient;

const DEADLINE: Duration = Duration::from_millis(200);

/// Serve `listener`: connection number `n` answers every request after
/// `delay(n)`, or never when that is `None`.
fn serve(listener: UnixListener, delay: impl Fn(usize) -> Option<Duration> + Send + Sync + 'static) {
    tokio::spawn(async move {
        let delay = std::sync::Arc::new(delay);
        let mut n = 0usize;
        while let Ok((stream, _)) = listener.accept().await {
            n += 1;
            let delay = delay.clone();
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut reader = BufReader::new(r);
                let mut line = String::new();
                while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                    let verb = if line.contains("hydration.open_read") { "open_read" } else { "list" };
                    line.clear();
                    let Some(d) = delay(n) else { continue };
                    tokio::time::sleep(d).await;
                    let reply: &[u8] = if verb == "open_read" {
                        b"{\"ok\":true,\"cache_path\":\"/tmp/x\"}\n"
                    } else {
                        b"{\"ok\":true,\"entries\":[]}\n"
                    };
                    if w.write_all(reply).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
}

fn bind(dir: &tempfile::TempDir) -> (std::path::PathBuf, UnixListener) {
    let sock = dir.path().join("ipc.sock");
    let l = UnixListener::bind(&sock).unwrap();
    (sock, l)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_silent_daemon_ends_the_request_with_a_timeout_and_the_connection_is_not_reused() {
    let dir = tempfile::tempdir().unwrap();
    let (sock, l) = bind(&dir);
    serve(l, |_| None);
    let mut c = IpcClient::connect(&sock).await.unwrap();
    c.set_request_deadline(DEADLINE);

    let t = Instant::now();
    let err = c.list("").await.err().expect("a silent daemon must not be waited for for ever");
    assert!(matches!(err, IpcError::Timeout(_)), "{err:?}");
    assert!(t.elapsed() >= DEADLINE && t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());

    let t = Instant::now();
    let err = c.list("").await.err().expect("the timed-out connection is not used again");
    assert!(matches!(err, IpcError::Io(_)), "{err:?}");
    assert!(t.elapsed() < Duration::from_millis(100), "must fail at once, took {:?}", t.elapsed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_reconnecting_client_drops_a_timed_out_connection_and_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let (sock, l) = bind(&dir);
    // The first connection is silent, every later one answers.
    serve(l, |n| if n == 1 { None } else { Some(Duration::ZERO) });
    let mut c = ReconnectingIpcClient::connect(&sock).await.unwrap();
    c.set_request_deadline(DEADLINE);

    let err = c.list("").await.err().expect("first call times out");
    assert!(matches!(err, IpcError::Timeout(_)), "{err:?}");
    let entries = c.list("").await.expect("the next call starts on a fresh connection");
    assert!(entries.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transfer_verb_has_no_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let (sock, l) = bind(&dir);
    serve(l, |_| Some(DEADLINE * 3));
    let mut c = IpcClient::connect(&sock).await.unwrap();
    c.set_request_deadline(DEADLINE);

    let r = c.open_read("h1", "/big.bin").await.expect("a slow download is not a dead daemon");
    assert_eq!(r.cache_path, std::path::PathBuf::from("/tmp/x"));
}

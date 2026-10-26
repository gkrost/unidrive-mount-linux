//! A reply (or event) line over the 4 MiB inbound bound must not leave its
//! remainder in the socket: the next request would read it as its own reply.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use unidrive_mount::ipc::{IpcClient, IpcError};
use unidrive_mount::reconnect::ReconnectingIpcClient;

const OVER_LIMIT: usize = 5 * 1024 * 1024;

// A failed assertion must not print a 4 MiB reply.
fn brief<T: std::fmt::Debug>(v: &T) -> String {
    format!("{v:?}").chars().take(200).collect()
}

/// A daemon that answers its first request on a connection with `first`
/// and every later one with `{"ok":true,"cache_path":"/cache/second"}`.
/// Returns the socket path and the number of connections accepted.
async fn spawn_daemon(first: Vec<u8>) -> (PathBuf, Arc<AtomicUsize>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let accepted_in_task = Arc::clone(&accepted);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { return };
            accepted_in_task.fetch_add(1, Ordering::SeqCst);
            let first = first.clone();
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut reader = BufReader::new(r);
                let mut n = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    n += 1;
                    let reply: &[u8] = if n == 1 {
                        &first
                    } else {
                        b"{\"ok\":true,\"cache_path\":\"/cache/second\"}\n"
                    };
                    if w.write_all(reply).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (socket, accepted, dir)
}

fn oversized_json_line() -> Vec<u8> {
    let mut line = br#"{"ok":true,"cache_path":""#.to_vec();
    line.resize(OVER_LIMIT, b'a');
    line.extend_from_slice(b"\"}\n");
    line
}

// The cut at 4 MiB falls inside a two-byte character.
fn oversized_multibyte_line() -> Vec<u8> {
    let mut line = br#"{"ok":true,"cache_path":"x"#.to_vec();
    while line.len() < OVER_LIMIT {
        line.extend_from_slice("\u{00e4}".as_bytes());
    }
    line.extend_from_slice(b"\"}\n");
    line
}

#[tokio::test]
async fn reply_over_the_bound_is_discarded_and_the_next_reply_is_its_own() {
    let (socket, accepted, _dir) = spawn_daemon(oversized_json_line()).await;
    let mut client = IpcClient::connect(&socket).await.unwrap();

    let first = client.open_read("h-1", "/x").await;
    assert!(matches!(first, Err(IpcError::Malformed(_))), "got {}", brief(&first));

    let second = client.open_read("h-2", "/y").await.expect("second reply must be its own");
    assert_eq!(second.cache_path, std::path::Path::new("/cache/second"));
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reply_cut_inside_a_multibyte_character_is_discarded_the_same_way() {
    let (socket, _accepted, _dir) = spawn_daemon(oversized_multibyte_line()).await;
    let mut client = IpcClient::connect(&socket).await.unwrap();

    let first = client.open_read("h-1", "/x").await;
    assert!(matches!(first, Err(IpcError::Malformed(_))), "got {}", brief(&first));

    let second = client.open_read("h-2", "/y").await.expect("second reply must be its own");
    assert_eq!(second.cache_path, std::path::Path::new("/cache/second"));
}

// An idempotent verb is retried on an `Io` error: an oversized reply must not
// be one, or `list` would re-fetch the same 5 MiB for ever.
#[tokio::test]
async fn reconnecting_client_does_not_retry_an_oversized_reply() {
    let (socket, accepted, _dir) = spawn_daemon(oversized_multibyte_line()).await;
    let mut client = ReconnectingIpcClient::connect(&socket).await.unwrap();

    let first = tokio::time::timeout(Duration::from_secs(10), client.open_read("h-1", "/x"))
        .await
        .expect("an oversized reply must end the call, not loop");
    assert!(matches!(first, Err(IpcError::Malformed(_))), "got {}", brief(&first));

    let second = client.open_read("h-2", "/y").await.expect("second reply must be its own");
    assert_eq!(second.cache_path, std::path::Path::new("/cache/second"));
    assert_eq!(accepted.load(Ordering::SeqCst), 1, "the connection must not have been rebuilt");
}

#[tokio::test]
async fn event_over_the_bound_is_an_error_and_the_next_event_is_read_whole() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        w.write_all(b"{\"ok\":true}\n").await.unwrap();
        let mut big = br#"{"event":"x","pad":""#.to_vec();
        big.resize(OVER_LIMIT, b'a');
        big.extend_from_slice(b"\"}\n");
        w.write_all(&big).await.unwrap();
        w.write_all(b"{\"event\":\"next\"}\n").await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let mut client = IpcClient::connect(&socket).await.unwrap();
    client.subscribe().await.unwrap();

    let first = client.read_event_line().await;
    assert!(first.is_err(), "an event over the bound must not be handed out cut: {}", brief(&first));

    let next = client.read_event_line().await.expect("the next event must be read whole");
    assert_eq!(next, r#"{"event":"next"}"#);
}

//! Cold opens through the mount run side by side.
//!
//! The fake daemon holds every `hydration.open_read` reply until three of them are pending at once, so the
//! reads below only succeed when the mount has three opens in flight together; with a single connection the
//! first open would wait for itself and the daemon answers `rendezvous_timeout` (an I/O error to the reader).
//!
//! One test per invariant:
//! - `cold_opens_of_different_files_are_in_flight_together`: with read lanes the three reads all complete.
//! - `without_read_lanes_cold_opens_are_serialised`: the control, proving the rendezvous detects the old
//!   behaviour. If it passes with lanes disabled, the first test no longer proves anything.
//! - `metadata_calls_are_answered_while_cold_opens_are_held`: a held download does not hold up `stat` of
//!   another file, which uses the main connection.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::fake_jvm::{replies, FakeJvm, Rendezvous};
use tokio::sync::Mutex;
use unidrive_mount::fuse_fs::UnidriveFs;
use unidrive_mount::read_lanes::ReadLanes;
use unidrive_mount::reconnect::ReconnectingIpcClient;
mod support;

const FILES: [&str; 3] = ["a.txt", "b.txt", "c.txt"];

struct Rig {
    jvm: FakeJvm,
    mount: fuse3::raw::MountHandle,
    mount_path: PathBuf,
    _dirs: Vec<tempfile::TempDir>,
}

/// A mount over a fake daemon that lists the three files and holds their `open_read` replies until `n` are
/// pending together (`wait` at most). Each file's cache copy holds its own name.
async fn rig(lanes: usize, n: usize, wait: Duration) -> Rig {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("any.cache");
    std::fs::File::create(&cache_path).unwrap().write_all(b"cold").unwrap();
    let open_read = format!(r#"{{"ok":true,"cache_path":"{}"}}"#, cache_path.to_str().unwrap());
    let entries: Vec<String> = FILES
        .iter()
        .map(|f| format!(r#"{{"path":"/{f}","size":4,"mtime_ms":1000000,"hydrated":false,"folder":false}}"#))
        .collect();
    let list = format!(r#"{{"ok":true,"entries":[{}]}}"#, entries.join(","));

    let jvm = FakeJvm::spawn_rendezvous(
        replies(&[
            ("hydration.list", list.as_str()),
            ("hydration.open_read", open_read.as_str()),
            ("hydration.close_handle", r#"{"ok":true}"#),
        ]),
        Rendezvous::new("hydration.open_read", n, wait),
    )
    .await;

    let main = ReconnectingIpcClient::connect(&jvm.socket_path).await.unwrap();
    let mut clients = Vec::new();
    for _ in 0..lanes {
        clients.push(ReconnectingIpcClient::connect(&jvm.socket_path).await.unwrap());
    }
    let fs = UnidriveFs::new(Arc::new(Mutex::new(main))).with_read_lanes(ReadLanes::new(clients));

    let mount_dir = tempfile::tempdir().unwrap();
    let mount_path = mount_dir.path().to_path_buf();
    let mut options = fuse3::MountOptions::default();
    options.fs_name("unidrive-test").nonempty(false);
    let mount = fuse3::raw::Session::new(options)
        .mount_with_unprivileged(fs, &mount_path)
        .await
        .expect("mount with unprivileged should succeed");
    tokio::time::sleep(Duration::from_millis(100)).await;
    Rig { jvm, mount, mount_path, _dirs: vec![cache_dir, mount_dir] }
}

impl Rig {
    async fn finish(self) {
        let _ = self.mount.unmount().await;
        self.jvm.shutdown().await;
    }
}

/// Read the three files at once; each result is the bytes or the error text.
async fn read_all_at_once(mount_path: &PathBuf) -> Vec<Result<Vec<u8>, String>> {
    let tasks: Vec<_> = FILES
        .iter()
        .map(|f| {
            let p = mount_path.join(f);
            tokio::task::spawn_blocking(move || std::fs::read(p).map_err(|e| e.to_string()))
        })
        .collect();
    let mut out = Vec::new();
    for t in tasks {
        out.push(t.await.unwrap());
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_opens_of_different_files_are_in_flight_together() {
    let rig = rig(3, 3, Duration::from_secs(10)).await;
    let results = read_all_at_once(&rig.mount_path).await;
    let recorded = rig.jvm.recorded_requests().await;
    rig.finish().await;

    for (f, r) in FILES.iter().zip(&results) {
        assert_eq!(r.as_deref().ok(), Some(&b"cold"[..]), "{f}: {r:?}");
    }
    let opens = recorded.iter().filter(|r| r.contains(r#""verb":"hydration.open_read""#)).count();
    assert_eq!(opens, 3, "one open_read per file; recorded={recorded:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_read_lanes_cold_opens_are_serialised() {
    let rig = rig(0, 3, Duration::from_millis(1500)).await;
    let results = read_all_at_once(&rig.mount_path).await;
    rig.finish().await;

    assert!(
        results.iter().any(|r| r.is_err()),
        "with one shared connection the opens cannot meet; every read succeeded: {results:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_calls_are_answered_while_cold_opens_are_held() {
    // Two opens held for the whole wait (a third never arrives): the lanes are busy, the main connection is free.
    let rig = rig(2, 3, Duration::from_secs(4)).await;
    let held: Vec<_> = FILES[..2]
        .iter()
        .map(|f| {
            let p = rig.mount_path.join(f);
            tokio::task::spawn_blocking(move || std::fs::read(p))
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let p = rig.mount_path.join(FILES[2]);
    let t0 = Instant::now();
    let md = tokio::task::spawn_blocking(move || std::fs::metadata(p)).await.unwrap();
    let took = t0.elapsed();

    for h in held {
        let _ = h.await;
    }
    rig.finish().await;

    assert_eq!(md.expect("stat of a third file").len(), 4);
    assert!(took < Duration::from_secs(2), "stat waited behind the held opens: {took:?}");
}

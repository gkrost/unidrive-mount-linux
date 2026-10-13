//! statfs reports the account quota carried by `daemon.status`.
//!
//! One test per invariant:
//! - the quota fills blocks/bfree/bavail (`statfs_reports_the_account_quota`)
//! - an unknown quota falls back to the constants, for null values and for an
//!   absent object (`..._when_quota_values_are_null`, `..._when_status_has_no_quota`)
//! - an over-quota account reports no free space (`statfs_reports_zero_free_when_over_quota`)
//! - statfs never waits for the daemon (`statfs_does_not_block_on_a_silent_daemon`)
//! - the refresh is periodic, not per call (`quota_is_refreshed_at_the_interval`,
//!   `statfs_calls_do_not_hammer_the_daemon`)

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::fake_jvm::{replies, FakeJvm};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use unidrive_mount::fuse_fs::UnidriveFs;
use unidrive_mount::reconnect::ReconnectingIpcClient;
mod support;

const FALLBACK: u64 = 1 << 32;
const GIB: u64 = 1 << 30;

async fn mount(
    socket: &std::path::Path,
    interval: Option<Duration>,
) -> (fuse3::raw::MountHandle, tempfile::TempDir) {
    let ipc = ReconnectingIpcClient::connect(socket).await.unwrap();
    let mut fs = UnidriveFs::new(Arc::new(Mutex::new(ipc)));
    if let Some(i) = interval {
        fs = fs.with_quota_refresh_interval(i);
    }
    let dir = tempfile::tempdir().unwrap();
    let mut opts = fuse3::MountOptions::default();
    opts.fs_name("unidrive-test").nonempty(false);
    let h = fuse3::raw::Session::new(opts)
        .mount_with_unprivileged(fs, dir.path())
        .await
        .expect("mount with unprivileged should succeed in FUSE-enabled env");
    tokio::time::sleep(Duration::from_millis(100)).await;
    (h, dir)
}

async fn statvfs(path: PathBuf) -> libc::statvfs {
    tokio::task::spawn_blocking(move || {
        use std::ffi::CString;
        let c = CString::new(path.to_str().unwrap()).unwrap();
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::statvfs(c.as_ptr(), &mut st) }, 0, "statvfs failed");
        st
    })
    .await
    .unwrap()
}

/// Poll statvfs until `f_blocks` satisfies `want` (the refresh is asynchronous).
async fn statvfs_until(path: &std::path::Path, want: impl Fn(u64) -> bool) -> libc::statvfs {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let st = statvfs(path.to_path_buf()).await;
        if want(st.f_blocks) || Instant::now() > deadline {
            return st;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn status(quota: &str) -> std::collections::HashMap<String, String> {
    replies(&[("daemon.status", &format!(r#"{{"ok":true,"protocol_version":1{quota}}}"#))])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statfs_reports_the_account_quota() {
    let jvm = FakeJvm::spawn(status(
        r#","quota":{"used_bytes":429496729600,"total_bytes":751619276800,"fetched_at_ms":1,"stale":false,"error":null}"#,
    ))
    .await;
    let (h, dir) = mount(&jvm.socket_path, None).await;
    let st = statvfs_until(dir.path(), |b| b != FALLBACK).await;
    let _ = h.unmount().await;
    jvm.shutdown().await;

    assert_eq!(st.f_blocks, 700 * GIB / 4096);
    assert_eq!(st.f_bfree, 300 * GIB / 4096);
    assert_eq!(st.f_bavail, st.f_bfree);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statfs_falls_back_to_the_constants_when_quota_values_are_null() {
    let jvm = FakeJvm::spawn(status(
        r#","quota":{"used_bytes":null,"total_bytes":null,"fetched_at_ms":null,"stale":true,"error":"boom"}"#,
    ))
    .await;
    let (h, dir) = mount(&jvm.socket_path, None).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let st = statvfs(dir.path().to_path_buf()).await;
    let _ = h.unmount().await;
    jvm.shutdown().await;

    assert_eq!(st.f_blocks, FALLBACK);
    assert_eq!(st.f_bfree, FALLBACK);
    assert_eq!(st.f_bavail, FALLBACK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statfs_falls_back_to_the_constants_when_status_has_no_quota() {
    let jvm = FakeJvm::spawn(status("")).await;
    let (h, dir) = mount(&jvm.socket_path, None).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let st = statvfs(dir.path().to_path_buf()).await;
    let _ = h.unmount().await;
    jvm.shutdown().await;

    assert_eq!(st.f_blocks, FALLBACK);
    assert_eq!(st.f_bfree, FALLBACK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statfs_reports_zero_free_when_over_quota() {
    let jvm = FakeJvm::spawn(status(
        r#","quota":{"used_bytes":2147483648,"total_bytes":1073741824,"fetched_at_ms":1,"stale":false,"error":null}"#,
    ))
    .await;
    let (h, dir) = mount(&jvm.socket_path, None).await;
    let st = statvfs_until(dir.path(), |b| b != FALLBACK).await;
    let _ = h.unmount().await;
    jvm.shutdown().await;

    assert_eq!(st.f_blocks, GIB / 4096);
    assert_eq!(st.f_bfree, 0);
    assert_eq!(st.f_bavail, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statfs_does_not_block_on_a_silent_daemon() {
    // A daemon that accepts and reads but never answers.
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("silent.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    tokio::spawn(async move {
        let mut keep = Vec::new();
        while let Ok((s, _)) = listener.accept().await {
            let (r, w) = s.into_split();
            let mut reader = BufReader::new(r);
            tokio::spawn(async move {
                let mut l = String::new();
                while reader.read_line(&mut l).await.unwrap_or(0) > 0 {
                    l.clear();
                }
            });
            keep.push(w);
        }
    });
    let (h, dir) = mount(&sock, None).await;

    let t = Instant::now();
    let st = statvfs(dir.path().to_path_buf()).await;
    let took = t.elapsed();
    let _ = h.unmount().await;

    assert!(took < Duration::from_secs(2), "statfs waited {took:?} on the daemon");
    assert_eq!(st.f_blocks, FALLBACK);
}

async fn status_requests(jvm: &FakeJvm) -> usize {
    jvm.recorded_requests().await.iter().filter(|r| r.contains(r#""verb":"daemon.status""#)).count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_is_refreshed_at_the_interval() {
    let jvm = FakeJvm::spawn(status("")).await;
    let (h, dir) = mount(&jvm.socket_path, Some(Duration::from_millis(100))).await;
    for _ in 0..12 {
        statvfs(dir.path().to_path_buf()).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let n = status_requests(&jvm).await;
    let _ = h.unmount().await;
    jvm.shutdown().await;

    assert!(n >= 3, "expected periodic refreshes, saw {n} daemon.status requests");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statfs_calls_do_not_hammer_the_daemon() {
    let jvm = FakeJvm::spawn(status("")).await;
    let (h, dir) = mount(&jvm.socket_path, None).await;
    for _ in 0..50 {
        statvfs(dir.path().to_path_buf()).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let n = status_requests(&jvm).await;
    let _ = h.unmount().await;
    jvm.shutdown().await;

    assert_eq!(n, 1, "50 statfs calls within one interval must cost one daemon.status");
}

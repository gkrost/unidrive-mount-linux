//! LocalCache crash-recovery replay integration test.
//!
//! Load-bearing per the Phase 2 plan: the second half of the spec's
//! "every FUSE RELEASE that flushed writes to a cache file triggers a
//! matching open_write IPC, OR the next co-daemon restart's LocalCache
//! scan replays it" invariant.
//!
//! Pre-populates a cache file with mtime in the recent past (well after
//! the JVM's faked last_synced watermark). Launches the binary and
//! observes the FakeJvm's recorded traffic. The assertion: `open_write`
//! for the cache file appears in the recorded traffic, AND it appears
//! before any FUSE-derived IPC (`hydration.list` for `getattr` / lookups
//! triggered by user-space stat). The binary is shut down via SIGTERM
//! soon after the scan completes; the FUSE mount may or may not have
//! finished initialising — that's irrelevant to the load-bearing claim,
//! which is purely about the SCAN happening before any FUSE traffic.

use assert_cmd::cargo::cargo_bin;
use std::collections::HashMap;
use std::io::Write;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use unidrive_mount::fake_jvm::FakeJvm;

fn replies(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_replay_fires_open_write_before_fuse_traffic() {
    // Pre-populate the cache: <cache>/foo.txt with recent mtime.
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_file = cache_dir.path().join("foo.txt");
    {
        let mut f = std::fs::File::create(&cache_file).unwrap();
        f.write_all(b"recovered bytes").unwrap();
    }

    // Watermark = 1ms (Unix epoch + 1ms) so any modern mtime is newer.
    let cache_file_str = cache_file.to_str().unwrap();
    let list_reply = r#"{"ok":true,"entries":[]}"#.to_string();
    let open_write_reply = format!(r#"{{"ok":true,"cache_path":"{cache_file_str}"}}"#);

    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.last_synced", r#"{"ok":true,"mtime_ms":1}"#),
        ("hydration.open_write", open_write_reply.as_str()),
        ("hydration.list", list_reply.as_str()),
        ("hydration.close_handle", r#"{"ok":true}"#),
    ]))
    .await;

    let mount_tmp = tempfile::tempdir().unwrap();
    let mount_path = mount_tmp.path().to_path_buf();

    let bin = cargo_bin("unidrive-mount");
    let mut child = Command::new(&bin)
        .arg("--mount")
        .arg(&mount_path)
        .arg("--ipc")
        .arg(&jvm.socket_path)
        .arg("--cache")
        .arg(cache_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn unidrive-mount");

    // Give the binary time to: connect IPC -> scan-and-replay -> issue
    // open_write -> begin FUSE mount. The scan is synchronous against the
    // fake JVM (one round trip per cache file), so 500ms is enough on a
    // healthy host. Use tokio::time::timeout so a hang fails the test.
    let scan_observed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let recorded = jvm.recorded_requests().await;
            if recorded
                .iter()
                .any(|r| r.contains(r#""verb":"hydration.open_write""#))
            {
                return recorded;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("open_write must be observed within 5s");

    // SIGTERM the child so it unmounts cleanly. Don't wait forever.
    if let Some(pid) = child.id() {
        // SAFETY: kill(2) FFI.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;

    let recorded = jvm.recorded_requests().await;
    jvm.shutdown().await;

    // Load-bearing assertion 1: open_write appears in recorded traffic.
    let open_write_idx = recorded
        .iter()
        .position(|r| r.contains(r#""verb":"hydration.open_write""#))
        .unwrap_or_else(|| {
            panic!("expected hydration.open_write to fire from scan_and_replay: {recorded:?}")
        });

    // Load-bearing assertion 2: the open_write request carries the
    // expected remote path ("/foo.txt") and the cache_path.
    let req = &recorded[open_write_idx];
    assert!(
        req.contains(r#""path":"/foo.txt""#),
        "open_write must use remote path '/foo.txt': {req}"
    );
    assert!(
        req.contains(&format!(r#""cache_path":"{cache_file_str}""#)),
        "open_write must reference the cache file: {req}"
    );
    assert!(
        req.contains(r#""handle_id":"recovery-"#),
        "open_write must use recovery-N handle_id: {req}"
    );

    // Load-bearing assertion 3: last_synced for /foo.txt arrived BEFORE
    // the open_write. The scanner's contract is "query the watermark, then
    // decide to replay."
    let last_synced_idx = recorded
        .iter()
        .position(|r| r.contains(r#""verb":"hydration.last_synced""#))
        .unwrap_or_else(|| panic!("expected hydration.last_synced from scan: {recorded:?}"));
    assert!(
        last_synced_idx < open_write_idx,
        "last_synced must precede open_write. recorded={recorded:?}"
    );

    // Load-bearing assertion 4: NO hydration.list traffic arrived before
    // the scan completed. `list` is the JVM verb FUSE uses for readdir /
    // getattr / lookup against the directory tree — if any list arrived
    // before the open_write, the FUSE event loop was live when the scan
    // ran, which violates the "scan BEFORE mount" ordering.
    //
    // The scan_observed snapshot was taken the moment open_write was
    // recorded. Any `list` in that snapshot at an index BEFORE
    // open_write_idx is a true ordering violation.
    let first_list_in_scan_window = scan_observed
        .iter()
        .position(|r| r.contains(r#""verb":"hydration.list""#));
    if let Some(li) = first_list_in_scan_window {
        // Find open_write_idx within scan_observed (same snapshot).
        let owi = scan_observed
            .iter()
            .position(|r| r.contains(r#""verb":"hydration.open_write""#))
            .expect("scan_observed contains open_write by construction");
        assert!(
            li > owi,
            "hydration.list appeared before hydration.open_write in scan snapshot; \
             scan-and-replay must run BEFORE FUSE mount. snapshot={scan_observed:?}"
        );
    }
}

mod support;

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime};
use support::fake_jvm::{replies, FakeJvm};
use unidrive_mount::cache_scanner::scan_and_replay;
use unidrive_mount::ipc::IpcClient;

async fn client(jvm: &FakeJvm) -> IpcClient {
    IpcClient::connect(&jvm.socket_path).await.unwrap()
}

fn set_mtime(p: &Path, secs_in_past: u64) {
    let when = SystemTime::now() - Duration::from_secs(secs_in_past);
    let file = fs::File::options().write(true).open(p).unwrap();
    file.set_modified(when).unwrap();
}

#[tokio::test]
async fn empty_cache_returns_zero() {
    let cache_dir = tempfile::tempdir().unwrap();
    let jvm = FakeJvm::spawn(replies(&[])).await;
    let mut c = client(&jvm).await;
    let n = scan_and_replay(&mut c, cache_dir.path()).await.unwrap();
    assert_eq!(n, 0);
    jvm.shutdown().await;
}

#[tokio::test]
async fn missing_cache_dir_returns_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("does-not-exist");
    let jvm = FakeJvm::spawn(replies(&[])).await;
    let mut c = client(&jvm).await;
    let n = scan_and_replay(&mut c, &missing).await.unwrap();
    assert_eq!(n, 0);
    jvm.shutdown().await;
}

#[tokio::test]
async fn newer_than_watermark_triggers_replay() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_file = cache_dir.path().join("foo.txt");
    {
        let mut f = fs::File::create(&cache_file).unwrap();
        f.write_all(b"hi").unwrap();
    }
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.last_synced", r#"{"ok":true,"mtime_ms":1}"#),
        ("hydration.open_write", &format!(
            r#"{{"ok":true,"cache_path":"{}"}}"#,
            cache_file.to_str().unwrap()
        )),
    ]))
    .await;
    let mut c = client(&jvm).await;
    let n = scan_and_replay(&mut c, cache_dir.path()).await.unwrap();
    assert_eq!(n, 1);
    let recorded = jvm.recorded_requests().await;
    assert_eq!(
        recorded
            .iter()
            .filter(|r| r.contains(r#""verb":"hydration.open_write""#))
            .count(),
        1,
        "expected exactly one open_write call: {recorded:?}"
    );
    let ow = recorded
        .iter()
        .find(|r| r.contains(r#""verb":"hydration.open_write""#))
        .unwrap();
    assert!(ow.contains(r#""path":"/foo.txt""#), "open_write must use remote path: {ow}");
    assert!(ow.contains(r#""handle_id":"recovery-"#), "open_write must use recovery-N handle_id: {ow}");
    jvm.shutdown().await;
}

#[tokio::test]
async fn older_than_watermark_no_replay() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_file = cache_dir.path().join("foo.txt");
    {
        let mut f = fs::File::create(&cache_file).unwrap();
        f.write_all(b"hi").unwrap();
    }
    set_mtime(&cache_file, 3600);
    let future_ms: i64 = (SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64)
        + 60_000;
    let last_synced_reply = format!(r#"{{"ok":true,"mtime_ms":{future_ms}}}"#);
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.last_synced", last_synced_reply.as_str()),
        ("hydration.open_write", r#"{"ok":true,"cache_path":"/should/not/be/used"}"#),
    ]))
    .await;
    let mut c = client(&jvm).await;
    let n = scan_and_replay(&mut c, cache_dir.path()).await.unwrap();
    assert_eq!(n, 0);
    let recorded = jvm.recorded_requests().await;
    let ow_count = recorded
        .iter()
        .filter(|r| r.contains(r#""verb":"hydration.open_write""#))
        .count();
    assert_eq!(ow_count, 0, "no open_write should be issued: {recorded:?}");
    jvm.shutdown().await;
}

#[tokio::test]
async fn unknown_path_skips_silently() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_file = cache_dir.path().join("orphan.txt");
    {
        let mut f = fs::File::create(&cache_file).unwrap();
        f.write_all(b"hi").unwrap();
    }
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.last_synced", r#"{"ok":false,"error":"unknown_path"}"#),
        ("hydration.open_write", r#"{"ok":true,"cache_path":"/x"}"#),
    ]))
    .await;
    let mut c = client(&jvm).await;
    let n = scan_and_replay(&mut c, cache_dir.path()).await.unwrap();
    assert_eq!(n, 0);
    let recorded = jvm.recorded_requests().await;
    let ow_count = recorded
        .iter()
        .filter(|r| r.contains(r#""verb":"hydration.open_write""#))
        .count();
    assert_eq!(ow_count, 0, "Unknown reply must suppress replay: {recorded:?}");
    jvm.shutdown().await;
}

#[tokio::test]
async fn multiple_files_count_and_replay() {
    let cache_dir = tempfile::tempdir().unwrap();
    let sub = cache_dir.path().join("a").join("b");
    fs::create_dir_all(&sub).unwrap();
    let f1 = sub.join("foo.txt");
    let f2 = cache_dir.path().join("bar.txt");
    let f3 = cache_dir.path().join("old.txt");
    for f in [&f1, &f2, &f3] {
        let mut h = fs::File::create(f).unwrap();
        h.write_all(b"x").unwrap();
    }
    set_mtime(&f3, 1_000_000);
    let jvm = FakeJvm::spawn(replies(&[
        ("hydration.last_synced", r#"{"ok":true,"mtime_ms":1}"#),
        ("hydration.open_write", r#"{"ok":true,"cache_path":"/x"}"#),
    ]))
    .await;
    let mut c = client(&jvm).await;
    let n = scan_and_replay(&mut c, cache_dir.path()).await.unwrap();
    assert_eq!(n, 3, "all three files should replay against watermark=1");
    let recorded = jvm.recorded_requests().await;
    let ow_count = recorded
        .iter()
        .filter(|r| r.contains(r#""verb":"hydration.open_write""#))
        .count();
    assert_eq!(ow_count, 3, "expected 3 open_write calls: {recorded:?}");
    jvm.shutdown().await;
}

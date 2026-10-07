//! Names equal after NFC but different in code points.
//!
//! Remote paths are keyed in NFC, so `café.txt` (U+00E9) and `café.txt`
//! (U+0065 U+0301) map to one remote path. Through the mount they must stay
//! two different names: the second one never resolves to the first one's
//! entry, and creating, mkdir-ing or renaming onto it fails EEXIST without
//! any mutating verb reaching the JVM. Invariants, one test each:
//!
//! - create of an NFC-equal, different-code-point name fails EEXIST and sends
//!   no create/truncate/write verb (the existing entry is untouched);
//! - mkdir of such a name fails EEXIST and sends no mkdir;
//! - lookup of such a name is ENOENT, while the existing name still resolves;
//! - a lone decomposed name can be created, reopened and appended by the same
//!   name in one session;
//! - rename onto such a name fails EEXIST and sends no rename;
//! - the reverse direction (decomposed entry first, composed name second) is
//!   refused the same way for create and rename;
//! - identical-name lookup/open is unchanged, and Σ / ς (not NFC-equal) coexist.

use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use support::fake_jvm::{replies, FakeJvm};
use tokio::sync::Mutex;
use unidrive_mount::fuse_fs::UnidriveFs;
use unidrive_mount::reconnect::ReconnectingIpcClient;
mod support;

const CAFE_NFC: &str = "caf\u{00E9}.txt";
const CAFE_NFD: &str = "cafe\u{0301}.txt";

async fn mount(
    pairs: &[(&str, &str)],
) -> (FakeJvm, fuse3::raw::MountHandle, tempfile::TempDir, PathBuf) {
    let jvm = FakeJvm::spawn(replies(pairs)).await;
    let ipc = ReconnectingIpcClient::connect(&jvm.socket_path).await.unwrap();
    let fs = UnidriveFs::new(Arc::new(Mutex::new(ipc)));
    let tempdir = tempfile::tempdir().unwrap();
    let mount_path = tempdir.path().to_path_buf();
    let mut mount_options = fuse3::MountOptions::default();
    mount_options.fs_name("unidrive-test").nonempty(false);
    let handle = fuse3::raw::Session::new(mount_options)
        .mount_with_unprivileged(fs, &mount_path)
        .await
        .expect("mount should succeed");
    tokio::time::sleep(Duration::from_millis(200)).await;
    (jvm, handle, tempdir, mount_path)
}

fn file_entry(path: &str) -> String {
    format!(r#"{{"path":"{path}","size":5,"mtime_ms":1000000,"hydrated":false,"folder":false}}"#)
}

fn list_reply(entries: &[String]) -> String {
    format!(r#"{{"ok":true,"entries":[{}]}}"#, entries.join(","))
}

fn verbs_sent(recorded: &[String], verb: &str) -> usize {
    let needle = format!(r#""verb":"{verb}""#);
    recorded.iter().filter(|r| r.contains(&needle)).count()
}

fn open_for_write(p: PathBuf) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(p)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_of_nfc_equal_name_with_different_code_points_fails_eexist_and_leaves_existing_untouched() {
    let list = list_reply(&[file_entry(&format!("/{CAFE_NFC}")), file_entry("/K.txt")]);
    let (jvm, handle, _td, mp) = mount(&[
        ("hydration.list", list.as_str()),
        ("hydration.create", r#"{"ok":false,"error":"no_canned_reply"}"#),
    ])
    .await;

    let m = mp.clone();
    let (cafe, kelvin) = tokio::task::spawn_blocking(move || {
        (
            open_for_write(m.join(CAFE_NFD)).map(|_| ()),
            open_for_write(m.join("\u{212A}.txt")).map(|_| ()),
        )
    })
    .await
    .unwrap();

    let recorded = jvm.recorded_requests().await;
    let _ = handle.unmount().await;
    jvm.shutdown().await;

    for (label, r) in [("cafe+U+0301", cafe), ("U+212A", kelvin)] {
        let err = r.expect_err(&format!("{label}: create must fail"));
        assert_eq!(err.raw_os_error(), Some(libc::EEXIST), "{label}: want EEXIST, got {err:?}");
    }
    for verb in [
        "hydration.create",
        "hydration.open_write_begin",
        "hydration.open_read",
        "hydration.open_write",
    ] {
        assert_eq!(verbs_sent(&recorded, verb), 0, "{verb} must not be sent: {recorded:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mkdir_of_nfc_equal_name_with_different_code_points_fails_eexist() {
    let list = list_reply(&[r#"{"path":"/Å","size":0,"mtime_ms":1000000,"hydrated":false,"folder":true}"#.to_string()]);
    let (jvm, handle, _td, mp) = mount(&[
        ("hydration.list", list.as_str()),
        ("hydration.mkdir", r#"{"ok":true}"#),
    ])
    .await;

    let m = mp.clone();
    let r = tokio::task::spawn_blocking(move || std::fs::create_dir(m.join("A\u{030A}")))
        .await
        .unwrap();

    let recorded = jvm.recorded_requests().await;
    let _ = handle.unmount().await;
    jvm.shutdown().await;

    let err = r.expect_err("mkdir must fail");
    assert_eq!(err.raw_os_error(), Some(libc::EEXIST), "want EEXIST, got {err:?}");
    assert_eq!(verbs_sent(&recorded, "hydration.mkdir"), 0, "mkdir must not be sent: {recorded:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lookup_of_non_nfc_name_does_not_resolve_to_different_code_point_entry() {
    let list = list_reply(&[file_entry(&format!("/{CAFE_NFC}"))]);
    let (jvm, handle, _td, mp) = mount(&[("hydration.list", list.as_str())]).await;

    let m = mp.clone();
    let (nfd, nfc) = tokio::task::spawn_blocking(move || {
        (std::fs::metadata(m.join(CAFE_NFD)), std::fs::metadata(m.join(CAFE_NFC)))
    })
    .await
    .unwrap();

    let _ = handle.unmount().await;
    jvm.shutdown().await;

    let err = nfd.expect_err("decomposed name must not resolve to the composed entry");
    assert_eq!(err.raw_os_error(), Some(libc::ENOENT), "want ENOENT, got {err:?}");
    assert_eq!(nfc.expect("composed name must still resolve").len(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lone_decomposed_name_can_be_created_reopened_and_appended_by_same_name() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("cafe.cache");
    let cp = cache_path.to_str().unwrap();
    let create_reply = format!(r#"{{"ok":true,"cache_path":"{cp}","handle_id":"create-jvm-1"}}"#);
    let cache_reply = format!(r#"{{"ok":true,"cache_path":"{cp}"}}"#);
    let (jvm, handle, _td, mp) = mount(&[
        ("hydration.list", r#"{"ok":true,"entries":[]}"#),
        ("hydration.create", create_reply.as_str()),
        ("hydration.open_read", cache_reply.as_str()),
        ("hydration.open_write", cache_reply.as_str()),
        ("hydration.close_handle", r#"{"ok":true}"#),
    ])
    .await;

    let m = mp.clone();
    let result = tokio::task::spawn_blocking(move || {
        let p = m.join(CAFE_NFD);
        open_for_write(p.clone())?.write_all(b"a\n")?;
        let mut read_back = String::new();
        std::fs::File::open(&p)?.read_to_string(&mut read_back)?;
        std::fs::OpenOptions::new().append(true).open(&p)?.write_all(b"b\n")?;
        Ok::<_, std::io::Error>(read_back)
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;
    let recorded = jvm.recorded_requests().await;
    let _ = handle.unmount().await;
    jvm.shutdown().await;

    assert_eq!(result.expect("create/reopen/append by the same decomposed name"), "a\n");
    assert_eq!(std::fs::read(&cache_path).unwrap(), b"a\nb\n");
    let create_req = recorded
        .iter()
        .find(|r| r.contains(r#""verb":"hydration.create""#))
        .expect("hydration.create sent");
    assert!(
        create_req.contains(&format!(r#""path":"/{CAFE_NFC}""#)),
        "remote path must be NFC: {create_req}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_onto_nfc_equal_name_with_different_code_points_fails_eexist() {
    let list = list_reply(&[file_entry(&format!("/{CAFE_NFC}")), file_entry("/src.txt")]);
    let (jvm, handle, _td, mp) = mount(&[
        ("hydration.list", list.as_str()),
        ("hydration.rename", r#"{"ok":true}"#),
    ])
    .await;

    let m = mp.clone();
    let (r, src, dst) = tokio::task::spawn_blocking(move || {
        let r = std::fs::rename(m.join("src.txt"), m.join(CAFE_NFD));
        (r, std::fs::metadata(m.join("src.txt")), std::fs::metadata(m.join(CAFE_NFC)))
    })
    .await
    .unwrap();

    let recorded = jvm.recorded_requests().await;
    let _ = handle.unmount().await;
    jvm.shutdown().await;

    let err = r.expect_err("rename onto an NFC-equal name must fail");
    assert_eq!(err.raw_os_error(), Some(libc::EEXIST), "want EEXIST, got {err:?}");
    assert_eq!(verbs_sent(&recorded, "hydration.rename"), 0, "rename must not be sent: {recorded:?}");
    assert!(src.is_ok(), "source must remain: {src:?}");
    assert!(dst.is_ok(), "target must remain: {dst:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn composed_name_create_and_rename_onto_decomposed_entry_fail_eexist() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("cafe.cache");
    let cp = cache_path.to_str().unwrap();
    let create_reply = format!(r#"{{"ok":true,"cache_path":"{cp}","handle_id":"create-jvm-1"}}"#);
    let cache_reply = format!(r#"{{"ok":true,"cache_path":"{cp}"}}"#);
    let list = list_reply(&[file_entry("/src.txt")]);
    let (jvm, handle, _td, mp) = mount(&[
        ("hydration.list", list.as_str()),
        ("hydration.create", create_reply.as_str()),
        ("hydration.open_write", cache_reply.as_str()),
        ("hydration.close_handle", r#"{"ok":true}"#),
        ("hydration.rename", r#"{"ok":true}"#),
    ])
    .await;

    let m = mp.clone();
    let (first, second, renamed) = tokio::task::spawn_blocking(move || {
        let first = open_for_write(m.join(CAFE_NFD)).and_then(|mut f| f.write_all(b"member0"));
        let second = open_for_write(m.join(CAFE_NFC)).map(|_| ());
        let renamed = std::fs::rename(m.join("src.txt"), m.join(CAFE_NFC));
        (first, second, renamed)
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;
    let recorded = jvm.recorded_requests().await;
    let _ = handle.unmount().await;
    jvm.shutdown().await;

    first.expect("decomposed create must succeed");
    for (label, r) in [("create", second), ("rename", renamed)] {
        let err = r.expect_err(&format!("{label} onto the composed name must fail"));
        assert_eq!(err.raw_os_error(), Some(libc::EEXIST), "{label}: want EEXIST, got {err:?}");
    }
    assert_eq!(verbs_sent(&recorded, "hydration.create"), 1, "{recorded:?}");
    assert_eq!(verbs_sent(&recorded, "hydration.open_write_begin"), 0, "{recorded:?}");
    assert_eq!(verbs_sent(&recorded, "hydration.rename"), 0, "{recorded:?}");
    assert_eq!(std::fs::read(&cache_path).unwrap(), b"member0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identical_name_open_is_unchanged_and_sigma_variants_coexist() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("sigma.cache");
    std::fs::write(&cache_path, b"").unwrap();
    let cp = cache_path.to_str().unwrap();
    let create_reply = format!(r#"{{"ok":true,"cache_path":"{cp}","handle_id":"create-jvm-1"}}"#);
    let cache_reply = format!(r#"{{"ok":true,"cache_path":"{cp}"}}"#);
    let list = list_reply(&[file_entry("/\u{03A3}.txt"), file_entry(&format!("/{CAFE_NFC}"))]);
    let (jvm, handle, _td, mp) = mount(&[
        ("hydration.list", list.as_str()),
        ("hydration.create", create_reply.as_str()),
        ("hydration.open_write_begin", cache_reply.as_str()),
        ("hydration.open_write", cache_reply.as_str()),
        ("hydration.close_handle", r#"{"ok":true}"#),
    ])
    .await;

    let m = mp.clone();
    let (same, final_sigma, capital) = tokio::task::spawn_blocking(move || {
        let same = open_for_write(m.join(CAFE_NFC)).map(|_| ());
        let final_sigma = open_for_write(m.join("\u{03C2}.txt")).map(|_| ());
        let capital = std::fs::metadata(m.join("\u{03A3}.txt"));
        (same, final_sigma, capital)
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;
    let recorded = jvm.recorded_requests().await;
    let _ = handle.unmount().await;
    jvm.shutdown().await;

    same.expect("identical composed name must open the existing entry");
    final_sigma.expect("final sigma must be creatable next to capital sigma");
    assert_eq!(capital.expect("capital sigma must still resolve").len(), 5);
    let begin = recorded
        .iter()
        .find(|r| r.contains(r#""verb":"hydration.open_write_begin""#))
        .expect("identical-name O_TRUNC open goes to open_write_begin");
    assert!(begin.contains(&format!(r#""path":"/{CAFE_NFC}""#)), "{begin}");
    let create = recorded
        .iter()
        .find(|r| r.contains(r#""verb":"hydration.create""#))
        .expect("final sigma goes to create");
    assert!(create.contains("\"path\":\"/\u{03C2}.txt\""), "{create}");
}

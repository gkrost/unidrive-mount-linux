//! The engine's stable error tokens reach userland as the errno that says what
//! happened. `read_only`, `outside_scope`, `invalid_path` and `paused` fell
//! through to EIO ("Input/output error"), as did `unknown_path` on a read.
//!
//! Requires a FUSE-enabled environment, like `namespace_ops.rs`.

use std::sync::Arc;
use std::time::Duration;
use support::fake_jvm::{replies, FakeJvm};
use tokio::sync::Mutex;
use unidrive_mount::fuse_fs::UnidriveFs;
use unidrive_mount::reconnect::ReconnectingIpcClient;
mod support;

const LIST: &str = r#"{"ok":true,"entries":[{"path":"/a.txt","size":3,"mtime_ms":1000000,"hydrated":false,"folder":false}]}"#;

async fn mounted(pairs: &[(&str, &str)]) -> (FakeJvm, fuse3::raw::MountHandle, std::path::PathBuf, tempfile::TempDir) {
    let jvm = FakeJvm::spawn(replies(pairs)).await;
    let ipc = ReconnectingIpcClient::connect(&jvm.socket_path).await.unwrap();
    let fs = UnidriveFs::new(Arc::new(Mutex::new(ipc)));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let mut opts = fuse3::MountOptions::default();
    opts.fs_name("unidrive-test").nonempty(false);
    let handle = fuse3::raw::Session::new(opts)
        .mount_with_unprivileged(fs, &path)
        .await
        .expect("mount with unprivileged should succeed in FUSE-enabled env");
    tokio::time::sleep(Duration::from_millis(100)).await;
    (jvm, handle, path, dir)
}

fn errno(r: std::io::Result<impl std::fmt::Debug>) -> i32 {
    r.expect_err("the operation must fail").raw_os_error().expect("an OS error")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn namespace_refusals_carry_their_errno() {
    let (jvm, mount, mp, _dir) = mounted(&[
        ("hydration.list", LIST),
        ("hydration.close_handle", r#"{"ok":true}"#),
        ("hydration.mkdir", r#"{"ok":false,"error":"read_only"}"#),
        ("hydration.create", r#"{"ok":false,"error":"outside_scope"}"#),
        ("hydration.rename", r#"{"ok":false,"error":"invalid_path"}"#),
    ])
    .await;

    let (mkdir, create, rename) = tokio::task::spawn_blocking(move || {
        (
            errno(std::fs::create_dir(mp.join("trash-child"))),
            errno(std::fs::File::create(mp.join("new.txt"))),
            errno(std::fs::rename(mp.join("a.txt"), mp.join("b.txt"))),
        )
    })
    .await
    .unwrap();

    let _ = mount.unmount().await;
    jvm.shutdown().await;
    assert_eq!(mkdir, libc::EROFS, "mkdir of a read_only path");
    assert_eq!(create, libc::EACCES, "create outside the profile's scope");
    assert_eq!(rename, libc::EINVAL, "rename onto a name the engine does not accept");
}

async fn open_errno(open_read_reply: &str) -> i32 {
    let (jvm, mount, mp, _dir) = mounted(&[
        ("hydration.list", LIST),
        ("hydration.close_handle", r#"{"ok":true}"#),
        ("hydration.open_read", open_read_reply),
    ])
    .await;
    let code = tokio::task::spawn_blocking(move || errno(std::fs::File::open(mp.join("a.txt")))).await.unwrap();
    let _ = mount.unmount().await;
    jvm.shutdown().await;
    code
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn download_refused_while_paused_is_try_again() {
    assert_eq!(open_errno(r#"{"ok":false,"error":"paused"}"#).await, libc::EAGAIN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_of_a_path_the_engine_no_longer_knows_is_enoent() {
    assert_eq!(open_errno(r#"{"ok":false,"error":"unknown_path"}"#).await, libc::ENOENT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unlisted_refusal_stays_eio() {
    assert_eq!(open_errno(r#"{"ok":false,"error":"content_refused: SHA-256 mismatch"}"#).await, libc::EIO);
}

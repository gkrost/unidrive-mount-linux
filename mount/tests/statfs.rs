//! FUSE statfs integration test.
//!
//! Mounts a UnidriveFs, calls `libc::statvfs` on the mountpoint, and asserts
//! the returned values match the static constants in `fuse_fs::statfs`.
//! Prior to the implementation, the kernel returned ENOSYS and `df` could
//! not report the mount. This test first confirmed ENOSYS (now passes with
//! the implementation).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use unidrive_mount::fake_jvm::FakeJvm;
use unidrive_mount::fuse_fs::UnidriveFs;
use unidrive_mount::reconnect::ReconnectingIpcClient;

fn empty_replies() -> HashMap<String, String> {
    // statfs needs no list replies — it's purely static.
    HashMap::new()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statfs_returns_sane_static_values() {
    let jvm = FakeJvm::spawn(empty_replies()).await;

    let ipc = ReconnectingIpcClient::connect(&jvm.socket_path).await.unwrap();
    let fs = UnidriveFs::new(Arc::new(Mutex::new(ipc)));

    let tempdir = tempfile::tempdir().unwrap();
    let mount_path = tempdir.path().to_path_buf();

    let mut mount_options = fuse3::MountOptions::default();
    mount_options.fs_name("unidrive-test").nonempty(false);

    let mount_handle = fuse3::raw::Session::new(mount_options)
        .mount_with_unprivileged(fs, &mount_path)
        .await
        .expect("mount with unprivileged should succeed in FUSE-enabled env");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mp = mount_path.clone();
    let statvfs_result = tokio::task::spawn_blocking(move || {
        // SAFETY: statvfs is a pure syscall; no aliasing. path is a valid
        // null-terminated C string produced from a PathBuf that we own.
        use std::ffi::CString;
        let c_path = CString::new(mp.to_str().unwrap()).unwrap();
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut st) };
        if rc == 0 {
            Ok(st)
        } else {
            Err(unsafe { *libc::__errno_location() })
        }
    })
    .await
    .unwrap();

    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    let st = statvfs_result.expect("statvfs should succeed (was ENOSYS before statfs impl)");

    // bsize/frsize: 4096 (matches ReplyStatFs.bsize / .frsize)
    assert_eq!(st.f_bsize, 4096, "f_bsize should be 4096");
    assert_eq!(st.f_frsize, 4096, "f_frsize should be 4096");

    // namelen: 255 (matches ReplyStatFs.namelen)
    assert_eq!(st.f_namemax, 255, "f_namemax should be 255");

    // blocks > 0 (large non-zero value)
    assert!(st.f_blocks > 0, "f_blocks should be > 0, got {}", st.f_blocks);

    // free/avail blocks equal to total (fully-free cloud volume)
    assert_eq!(st.f_bfree, st.f_blocks, "f_bfree should equal f_blocks");
    assert_eq!(st.f_bavail, st.f_blocks, "f_bavail should equal f_blocks");

    // inodes: non-zero
    assert!(st.f_files > 0, "f_files should be > 0, got {}", st.f_files);
    assert!(st.f_ffree > 0, "f_ffree should be > 0, got {}", st.f_ffree);
}

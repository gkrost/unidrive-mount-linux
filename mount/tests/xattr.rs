use std::collections::HashMap;
use std::ffi::CString;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use support::fake_jvm::FakeJvm;
use unidrive_mount::fuse_fs::UnidriveFs;
use unidrive_mount::reconnect::ReconnectingIpcClient;
mod support;

fn one_file_replies() -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert(
        "hydration.list".to_string(),
        r#"{"ok":true,"entries":[{"path":"/probe.txt","size":0,"mtime_ms":1000,"hydrated":false,"folder":false}]}"#.to_string(),
    );
    m
}

async fn with_mount<F, R>(f: F) -> R
where
    F: FnOnce(CString) -> R + Send + 'static,
    R: Send + 'static,
{
    let jvm = FakeJvm::spawn(one_file_replies()).await;
    let ipc = ReconnectingIpcClient::connect(&jvm.socket_path).await.unwrap();
    let fs = UnidriveFs::new(Arc::new(Mutex::new(ipc)));

    let tempdir = tempfile::tempdir().unwrap();
    let mount_path = tempdir.path().to_path_buf();

    let mut mount_options = fuse3::MountOptions::default();
    mount_options.fs_name("unidrive-xattr-test").nonempty(false);

    let mount_handle = fuse3::raw::Session::new(mount_options)
        .mount_with_unprivileged(fs, &mount_path)
        .await
        .expect("mount should succeed in FUSE-enabled env");

    tokio::time::sleep(Duration::from_millis(150)).await;

    let file_path = mount_path.join("probe.txt");
    let c_path = CString::new(file_path.to_str().unwrap()).unwrap();

    let result = tokio::task::spawn_blocking(move || f(c_path)).await.unwrap();

    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;
    drop(tempdir);

    result
}

fn last_errno() -> i32 {
    unsafe { *libc::__errno_location() }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn getxattr_unknown_returns_enodata() {
    let (rc, errno) = with_mount(|c_path| {
        let attr_name = CString::new("user.nonexistent").unwrap();
        let mut buf = [0u8; 256];
        let rc = unsafe {
            libc::getxattr(c_path.as_ptr(), attr_name.as_ptr(), buf.as_mut_ptr() as *mut libc::c_void, buf.len())
        };
        let e = if rc < 0 { last_errno() } else { 0 };
        (rc, e)
    })
    .await;

    assert!(rc < 0, "getxattr should fail for unknown attr");
    assert_eq!(errno, libc::ENODATA, "getxattr should return ENODATA ({}) not ENOSYS ({}) or EIO ({}), got {}", libc::ENODATA, libc::ENOSYS, libc::EIO, errno);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hydrated_xattr_returns_flag() {
    // The canned file has hydrated=false, so we expect "0".
    let (rc, errno, val) = with_mount(|c_path| {
        let attr_name = CString::new("user.unidrive.hydrated").unwrap();
        let mut buf = [0u8; 8];
        let rc = unsafe {
            libc::getxattr(c_path.as_ptr(), attr_name.as_ptr(), buf.as_mut_ptr() as *mut libc::c_void, buf.len())
        };
        let e = if rc < 0 { last_errno() } else { 0 };
        (rc, e, buf)
    })
    .await;

    assert!(rc >= 0, "hydrated xattr must be readable, errno={errno}");
    assert_eq!(rc, 1, "hydrated xattr should be 1 byte (0 or 1)");
    assert_eq!(val[0], b'0', "canned file has hydrated=false, expected b'0'");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listxattr_includes_hydrated() {
    let (size_rc, data_rc, raw) = with_mount(|c_path| {
        let size_rc = unsafe { libc::listxattr(c_path.as_ptr(), std::ptr::null_mut(), 0) };

        let mut buf = [0u8; 256];
        let data_rc = unsafe { libc::listxattr(c_path.as_ptr(), buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };

        (size_rc, data_rc, buf.to_vec())
    })
    .await;

    assert!(size_rc > 0, "listxattr size-probe should report at least one xattr, got {size_rc}");
    assert!(data_rc > 0, "listxattr data request should return bytes, got {data_rc}");
    let list_str = String::from_utf8_lossy(&raw[..data_rc as usize]);
    assert!(list_str.contains("user.unidrive.hydrated"), "listxattr must include synthetic hydrated attr, got: {list_str:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setxattr_succeeds_silently() {
    let rc = with_mount(|c_path| {
        let attr_name = CString::new("user.test").unwrap();
        let value = b"hello";
        unsafe {
            libc::setxattr(c_path.as_ptr(), attr_name.as_ptr(), value.as_ptr() as *const libc::c_void, value.len(), 0)
        }
    })
    .await;

    assert_eq!(rc, 0, "setxattr should succeed (discard write), not return EOPNOTSUPP");
}

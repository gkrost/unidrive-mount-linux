use std::collections::HashMap;
use std::ffi::CString;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use unidrive_mount::fake_jvm::FakeJvm;
use unidrive_mount::fuse_fs::UnidriveFs;
use unidrive_mount::reconnect::ReconnectingIpcClient;

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
async fn getxattr_returns_enodata() {
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

    assert!(rc < 0, "getxattr should fail (no xattr store)");
    assert_eq!(errno, libc::ENODATA, "getxattr should return ENODATA ({}) not ENOSYS ({}) or EIO ({}), got {}", libc::ENODATA, libc::ENOSYS, libc::EIO, errno);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listxattr_returns_empty_list() {
    let (size_rc, size_errno, data_rc, data_errno) = with_mount(|c_path| {
        let size_rc = unsafe { libc::listxattr(c_path.as_ptr(), std::ptr::null_mut(), 0) };
        let size_errno = if size_rc < 0 { last_errno() } else { 0 };

        let mut buf = [0u8; 256];
        let data_rc = unsafe { libc::listxattr(c_path.as_ptr(), buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        let data_errno = if data_rc < 0 { last_errno() } else { 0 };

        (size_rc, size_errno, data_rc, data_errno)
    })
    .await;

    assert_eq!(size_rc, 0, "listxattr size-probe should return 0 (empty list), got {} (errno {})", size_rc, size_errno);
    assert_eq!(data_rc, 0, "listxattr data request should return 0 bytes (empty list), got {} (errno {})", data_rc, data_errno);
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

//! FUSE getattr + readdir integration test.
//!
//! Mounts a `UnidriveFs` backed by a `FakeJvm` that serves a single canned
//! `hydration.list` reply with three entries (one file, one folder, one
//! hydrated file) and asserts `ls` shows them with correct types.
//!
//! Requires a FUSE-enabled environment (fusermount3 setuid + /dev/fuse rw).
//! On hosts without that capability the test is `#[ignore]`-able with
//! `--include-ignored`; currently we leave it un-ignored and rely on the
//! agent-environment check (kernel 7.0.9, fusermount3 3.18, /dev/fuse 666).
//! If CI lacks FUSE, add `#[ignore]` and gate on `cfg(have_fuse)` once that
//! signal exists.

use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use support::fake_jvm::{replies, FakeJvm};
use unidrive_mount::fuse_fs::UnidriveFs;
use unidrive_mount::reconnect::ReconnectingIpcClient;
mod support;


#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ls_la_shows_file_folder_and_hydrated_file() {
    // FakeJvm serves `hydration.list("")` with three children. The mtime
    // values are arbitrary but distinct so we can verify they round-trip
    // into stat() output if we ever extend the assertions.
    let jvm = FakeJvm::spawn(replies(&[(
        "hydration.list",
        r#"{"ok":true,"entries":[{"path":"/file.txt","size":100,"mtime_ms":1000000,"hydrated":false,"folder":false},{"path":"/folder","size":0,"mtime_ms":2000000,"hydrated":false,"folder":true},{"path":"/cached.bin","size":42,"mtime_ms":3000000,"hydrated":true,"folder":false}]}"#,
    )]))
    .await;

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

    // Give the kernel a tick to settle (the mount returns before the
    // session is fully wired in some kernels).
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Shell out to `ls -la`. We run it in a blocking task so the FUSE
    // server (this process) can service the resulting getattr/readdir.
    let mp = mount_path.clone();
    let mp_for_readdir = mp.clone();
    let mp_for_ls = mp.clone();
    let mp_for_folder = mp.clone();
    let mp_for_file = mp.clone();
    let probe = tokio::task::spawn_blocking(move || {
        // First, list via Rust's read_dir directly (no shell, no buffering).
        let read_dir_entries: Vec<String> = match std::fs::read_dir(&mp_for_readdir) {
            Ok(it) => it
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect(),
            Err(e) => {
                vec![format!("READDIR_ERROR: {e}")]
            }
        };
        let stat_folder = std::fs::metadata(mp_for_folder.join("folder"));
        let stat_file = std::fs::metadata(mp_for_file.join("file.txt"));
        let ls_out = StdCommand::new("ls")
            .arg("-1")
            .arg(&mp_for_ls)
            .output()
            .expect("ls should run");
        (read_dir_entries, ls_out, stat_folder, stat_file)
    })
    .await
    .unwrap();

    // Unmount before assertions so a failure doesn't leave a stale mount.
    let _ = mount_handle.unmount().await;
    jvm.shutdown().await;

    let (read_dir_entries, ls_out, stat_folder, stat_file) = probe;

    // Assert std::fs::read_dir saw all three entries (it filters `.` / `..`).
    assert_eq!(
        read_dir_entries.len(),
        3,
        "read_dir entries: {read_dir_entries:?}"
    );
    let mut sorted = read_dir_entries.clone();
    sorted.sort();
    assert_eq!(sorted, vec!["cached.bin", "file.txt", "folder"]);

    let stdout = String::from_utf8_lossy(&ls_out.stdout);
    let stderr = String::from_utf8_lossy(&ls_out.stderr);
    assert!(
        ls_out.status.success(),
        "ls failed (status={:?}):\nstdout=\n{}\nstderr=\n{}",
        ls_out.status,
        stdout,
        stderr
    );
    assert!(stdout.contains("file.txt"), "missing file.txt in:\n{stdout}");
    assert!(stdout.contains("folder"), "missing folder in:\n{stdout}");
    assert!(stdout.contains("cached.bin"), "missing cached.bin in:\n{stdout}");

    let stat_folder = stat_folder.expect("stat folder");
    assert!(stat_folder.is_dir(), "folder should be a directory");
    let stat_file = stat_file.expect("stat file.txt");
    assert!(stat_file.is_file(), "file.txt should be a regular file");
    assert_eq!(stat_file.len(), 100, "file.txt size should be 100");
}

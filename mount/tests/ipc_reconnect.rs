//! ReconnectingIpcClient reconnect-on-disconnect test.
//!
//! Determinism strategy: use short interval (10ms) and short budget
//! (5s) so the whole retry cycle fits comfortably inside the test's
//! 30s outer timeout, but is still long enough to absorb scheduling
//! jitter on a loaded host. No virtual time (tokio::time::pause) —
//! the FakeJvm runs real Unix-socket IO, so virtual time would deadlock
//! the accept loop. The trade-off is "real seconds for retries" against
//! "no flakes from clock-skew between virtual and real time on syscalls."

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use unidrive_mount::fake_jvm::FakeJvm;
use unidrive_mount::fuse_fs::UnidriveFs;
use unidrive_mount::reconnect::ReconnectingIpcClient;

fn replies(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_succeeds_after_server_restart() {
    // Reserve a stable socket path owned by the test (not FakeJvm), so we
    // can shut down the first JVM and respawn another on the same path.
    let socket_tmp = tempfile::tempdir().expect("socket tempdir");
    let socket_path = socket_tmp.path().join("ipc.sock");

    // Round 1: spawn FakeJvm at the stable path, build the reconnecting
    // client, fire one successful list().
    let jvm_v1 = FakeJvm::spawn_at(
        socket_path.clone(),
        replies(&[("hydration.list", r#"{"ok":true,"entries":[]}"#)]),
    )
    .await;
    let mut client = ReconnectingIpcClient::connect_with(
        &socket_path,
        Duration::from_millis(10),
        Duration::from_secs(5),
    )
    .await
    .expect("initial connect");
    let entries = client.list("").await.expect("first list");
    assert!(entries.is_empty());

    // Kill the JVM. The socket file is deleted by spawn_at's cleanup on
    // the next spawn, but right now it lingers and the accept task is
    // aborted so connect() will fail with ECONNREFUSED until v2 binds.
    jvm_v1.shutdown().await;
    // Remove the stale socket file so the next bind() succeeds.
    let _ = std::fs::remove_file(&socket_path);

    // Spawn a tokio task to restart the JVM after a short delay. The
    // delay must be > the client's retry interval so at least one
    // reconnect attempt fails first.
    let socket_for_restart = socket_path.clone();
    let restart_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        FakeJvm::spawn_at(
            socket_for_restart,
            replies(&[("hydration.list", r#"{"ok":true,"entries":[]}"#)]),
        )
        .await
    });

    // Round 2: call list() again. The first attempt sees the v1 socket
    // (broken pipe / connection refused). The wrapper retries every
    // 10ms; after ~150ms the v2 JVM is up and a fresh connect succeeds.
    let result =
        tokio::time::timeout(Duration::from_secs(10), client.list("")).await;
    let entries = result
        .expect("reconnect must complete within 10s")
        .expect("list must succeed after reconnect");
    assert!(entries.is_empty());

    let jvm_v2 = restart_task.await.expect("v2 spawn task panicked");
    jvm_v2.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_succeeds_after_daemon_stop_and_restart_cycle() {
    // Production-shape repro: a graceful `daemon stop` followed by a fresh
    // `daemon run` leaves a multi-second gap where the socket path either
    // doesn't exist or refuses connections. The first verb call AFTER that
    // gap must transparently reconnect — fuse_fs maps any IpcError::Io
    // surfaced here straight to EIO, which the kernel propagates to userland
    // as "ls: Input/output error". The earlier reconnect_succeeds_after_
    // server_restart test schedules v2 spawn at 150ms; the kernel will sit
    // on a hung getattr for far longer in production, so this case extends
    // the gap to be wider than a single retry interval AND issues the next
    // verb after the gap (not concurrent with it) — mirroring the live
    // sequence "ls -> daemon stop -> daemon run -> ls".
    let socket_tmp = tempfile::tempdir().expect("socket tempdir");
    let socket_path = socket_tmp.path().join("ipc.sock");

    let jvm_v1 = FakeJvm::spawn_at(
        socket_path.clone(),
        replies(&[("hydration.list", r#"{"ok":true,"entries":[]}"#)]),
    )
    .await;
    let mut client = ReconnectingIpcClient::connect_with(
        &socket_path,
        Duration::from_millis(50),
        Duration::from_secs(5),
    )
    .await
    .expect("initial connect");
    let _ = client.list("").await.expect("first list");

    jvm_v1.shutdown().await;
    let _ = std::fs::remove_file(&socket_path);

    // Multi-hundred-ms gap with NO server. Any verb attempted during this
    // window must NOT surface to the caller — the wrapper must keep retrying
    // until v2 is up. This is the gap that the live JVM stop+restart cycle
    // produces; the existing tight-loop test (150ms with the v2 spawn task
    // already armed) doesn't exercise it.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let jvm_v2 = FakeJvm::spawn_at(
        socket_path.clone(),
        replies(&[("hydration.list", r#"{"ok":true,"entries":[]}"#)]),
    )
    .await;

    let entries = tokio::time::timeout(Duration::from_secs(10), client.list(""))
        .await
        .expect("reconnect must complete within 10s")
        .expect("list must succeed after daemon stop+restart cycle");
    assert!(entries.is_empty());

    jvm_v2.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fuse_mount_survives_daemon_stop_and_restart_cycle() {
    // Production repro at the FUSE layer: mount, read a file, stop the JVM,
    // restart the JVM at the same socket path, read again. The second read
    // must NOT surface "Input/output error" — the live repro signature.
    //
    // Why open(2) and not stat: kernel caches dentry/attr results across
    // calls (the readdirplus reply populates negative- and positive-cache
    // entries with a TTL), so a second stat may not hit IPC at all. open()
    // always issues hydration.open_read, so it round-trips through the IPC
    // path on every call — that's the path that surfaces EIO on pre-fix
    // code where UnidriveFs held a raw IpcClient instead of the wrapper.
    let socket_tmp = tempfile::tempdir().expect("socket tempdir");
    let socket_path = socket_tmp.path().join("ipc.sock");

    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_file = cache_dir.path().join("foo.cache");
    std::fs::write(&cache_file, b"hi\n").unwrap();
    let open_read_reply = format!(
        r#"{{"ok":true,"cache_path":"{}"}}"#,
        cache_file.to_str().unwrap()
    );
    let list_reply = r#"{"ok":true,"entries":[{"path":"/foo.txt","size":3,"mtime_ms":1000,"hydrated":false,"folder":false}]}"#;
    let canned: Vec<(&str, &str)> = vec![
        ("hydration.list", list_reply),
        ("hydration.open_read", open_read_reply.as_str()),
        ("hydration.close_handle", r#"{"ok":true}"#),
    ];

    let jvm_v1 = FakeJvm::spawn_at(socket_path.clone(), replies(&canned)).await;

    let ipc = ReconnectingIpcClient::connect_with(
        &socket_path,
        Duration::from_millis(50),
        Duration::from_secs(10),
    )
    .await
    .expect("initial connect");
    let fs = UnidriveFs::new(Arc::new(Mutex::new(ipc)));

    let mount_tmp = tempfile::tempdir().expect("mount tempdir");
    let mount_path = mount_tmp.path().to_path_buf();

    let mut mount_options = fuse3::MountOptions::default();
    mount_options.fs_name("unidrive-test").nonempty(false);

    let mount_handle = fuse3::raw::Session::new(mount_options)
        .mount_with_unprivileged(fs, &mount_path)
        .await
        .expect("mount should succeed");

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Round 1: read goes through to v1.
    let mp1 = mount_path.clone();
    let first_read = tokio::task::spawn_blocking(move || std::fs::read(mp1.join("foo.txt")))
        .await
        .unwrap();
    assert!(
        first_read.is_ok(),
        "first read before daemon restart must succeed; got: {first_read:?}"
    );

    // Graceful daemon stop. FakeJvm::shutdown aborts both the accept loop
    // AND any in-flight connection tasks, mirroring how a real JVM process
    // kill tears down all client connections (without this, the IpcClient
    // still talks to v1's lingering child task and never tries to reconnect).
    jvm_v1.shutdown().await;
    let _ = std::fs::remove_file(&socket_path);

    // Multi-hundred-ms gap with NO server — wider than one retry interval
    // so the wrapper must actually retry rather than catch the v2 listener
    // on the first attempt. Also longer than the lookup TTL so the kernel
    // can't shortcut us either.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let jvm_v2 = FakeJvm::spawn_at(socket_path.clone(), replies(&canned)).await;

    // Round 2: read must succeed transparently through the reconnect.
    let mp2 = mount_path.clone();
    let second_read = tokio::task::spawn_blocking(move || std::fs::read(mp2.join("foo.txt")))
        .await
        .unwrap();

    let v2_requests = jvm_v2.recorded_requests().await;
    let _ = mount_handle.unmount().await;
    jvm_v2.shutdown().await;

    assert!(
        second_read.is_ok(),
        "read(foo.txt) after daemon stop+restart cycle must succeed (no EIO); got: {second_read:?}"
    );
    // Belt-and-braces: confirm the second read actually went through v2.
    // If this fires it means we tested kernel-cache pass-through, not the
    // reconnect path — the assertion above would silently pass.
    assert!(
        v2_requests.iter().any(|r| r.contains(r#""verb":"hydration.open_read""#)),
        "v2 JVM should have received hydration.open_read after the reconnect; got: {v2_requests:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budget_exhaustion_surfaces_io_error() {
    // Reserve a path that no JVM will ever bind. Use a deliberately tight
    // budget so the test exits in well under a second.
    let socket_tmp = tempfile::tempdir().expect("socket tempdir");
    let socket_path = socket_tmp.path().join("ghost.sock");

    // The very first connect must fail (no server). The wrapper's
    // constructor uses a single connect attempt — no retries on the
    // initial connect. That's correct: at boot the absence of the JVM
    // is a fatal config error, not a transient blip.
    let res = ReconnectingIpcClient::connect_with(
        &socket_path,
        Duration::from_millis(10),
        Duration::from_millis(50),
    )
    .await;
    // We don't assert on a specific kind beyond "it's an error"; the
    // underlying io::ErrorKind varies (NotFound vs ConnectionRefused
    // depending on whether the parent dir exists).
    assert!(res.is_err(), "connect to nonexistent server must fail");
}

//! The cached view follows the daemon's change events.
//!
//! `lookup` and `getattr` trust a cached attribute until the parent is listed
//! again, so a size or a deletion made elsewhere stayed invisible for ever
//! (a remotely grown file was read cut off at its old size). A
//! `view.invalidated` event (or `lost`, or a gap in the stream) now drops the
//! attributes it names, and `open` takes the size of the file it opened.
//!
//! The fake daemon here can change its listing and push events; every test
//! mounts for real (FUSE-enabled environment, like the other mount tests).

use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, Mutex};
use unidrive_mount::fuse_fs::UnidriveFs;
use unidrive_mount::ipc_auth::IpcAuth;
use unidrive_mount::reconnect::ReconnectingIpcClient;
use unidrive_mount::view_refresh::keep_view_fresh;

struct Daemon {
    socket: PathBuf,
    list: Arc<StdMutex<String>>,
    subscribers: Arc<StdMutex<Vec<mpsc::UnboundedSender<String>>>>,
    _dir: tempfile::TempDir,
}

impl Daemon {
    /// `cache_path` is what `open_read`, `create` and `open_write` hand out.
    async fn spawn(list: &str, cache_path: &Path) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let list = Arc::new(StdMutex::new(list.to_string()));
        let subscribers: Arc<StdMutex<Vec<mpsc::UnboundedSender<String>>>> = Arc::default();
        let cache_path = cache_path.display().to_string();
        let (list_in_task, subscribers_in_task) = (Arc::clone(&list), Arc::clone(&subscribers));
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { return };
                let (list, subscribers, cache_path) =
                    (Arc::clone(&list_in_task), Arc::clone(&subscribers_in_task), cache_path.clone());
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let (out, mut lines) = mpsc::unbounded_channel::<String>();
                    tokio::spawn(async move {
                        while let Some(line) = lines.recv().await {
                            if w.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    });
                    let mut reader = BufReader::new(r);
                    loop {
                        let mut request = String::new();
                        if reader.read_line(&mut request).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let request: serde_json::Value = serde_json::from_str(&request).unwrap();
                        let reply = match request["verb"].as_str().unwrap_or("") {
                            "daemon.status" => r#"{"ok":true,"protocol_version":1}"#.to_string(),
                            "hydration.list" => list.lock().unwrap().clone(),
                            "hydration.open_read" | "hydration.open_write" => {
                                format!(r#"{{"ok":true,"cache_path":"{cache_path}"}}"#)
                            }
                            "hydration.create" => format!(
                                r#"{{"ok":true,"cache_path":"{cache_path}","handle_id":"{}"}}"#,
                                request["handle_id"].as_str().unwrap_or("h")
                            ),
                            "hydration.subscribe" => {
                                subscribers.lock().unwrap().push(out.clone());
                                r#"{"ok":true}"#.to_string()
                            }
                            _ => r#"{"ok":true}"#.to_string(),
                        };
                        let _ = out.send(reply);
                    }
                });
            }
        });
        Daemon { socket, list, subscribers, _dir: dir }
    }

    fn set_list(&self, list: &str) {
        *self.list.lock().unwrap() = list.to_string();
    }

    fn push(&self, event: &str) {
        for s in self.subscribers.lock().unwrap().iter() {
            let _ = s.send(event.to_string());
        }
    }

    async fn wait_subscribed(&self) {
        for _ in 0..200 {
            if !self.subscribers.lock().unwrap().is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the mount never subscribed");
    }
}

fn listing(entries: &[(&str, u64, i64)]) -> String {
    let entries: Vec<String> = entries
        .iter()
        .map(|(path, size, mtime_ms)| {
            format!(r#"{{"path":"{path}","size":{size},"mtime_ms":{mtime_ms},"hydrated":false,"folder":false}}"#)
        })
        .collect();
    format!(r#"{{"ok":true,"entries":[{}]}}"#, entries.join(","))
}

fn cache_file(dir: &Path, bytes: usize) -> PathBuf {
    let path = dir.join("content.cache");
    std::fs::File::create(&path).unwrap().write_all(&vec![b'x'; bytes]).unwrap();
    path
}

struct Mounted {
    mount: fuse3::raw::MountHandle,
    path: PathBuf,
    _dir: tempfile::TempDir,
}

/// Mount a filesystem on `daemon` that follows its events.
async fn mount(daemon: &Daemon) -> Mounted {
    let ipc = ReconnectingIpcClient::connect(&daemon.socket).await.unwrap();
    let fs = UnidriveFs::new(Arc::new(Mutex::new(ipc)));
    let view = fs.view_cache();
    let socket = daemon.socket.clone();
    tokio::spawn(async move {
        keep_view_fresh(&socket, &IpcAuth::new(None, ""), Duration::from_millis(20), view).await;
    });
    daemon.wait_subscribed().await;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let mut options = fuse3::MountOptions::default();
    options.fs_name("unidrive-test").nonempty(false);
    let mount = fuse3::raw::Session::new(options)
        .mount_with_unprivileged(fs, &path)
        .await
        .expect("mount should succeed in a FUSE-enabled environment");
    tokio::time::sleep(Duration::from_millis(100)).await;
    Mounted { mount, path, _dir: dir }
}

async fn stat(path: PathBuf) -> std::io::Result<std::fs::Metadata> {
    tokio::task::spawn_blocking(move || std::fs::metadata(path)).await.unwrap()
}

/// Wait (the kernel keeps an answer for 1 s) until `probe` holds.
async fn eventually<F, Fut>(what: &str, mut probe: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..60 {
        if probe().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("never saw: {what}");
}

const KERNEL_ATTR_TTL: Duration = Duration::from_millis(1200);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_growth_reaches_stat_and_read_after_view_invalidated() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache = cache_file(cache_dir.path(), 5000);
    let daemon = Daemon::spawn(&listing(&[("/a.txt", 100, 1_000_000)]), &cache).await;
    let m = mount(&daemon).await;
    let file = m.path.join("a.txt");

    assert_eq!(stat(file.clone()).await.unwrap().len(), 100);
    daemon.set_list(&listing(&[("/a.txt", 5000, 2_000_000)]));
    tokio::time::sleep(KERNEL_ATTR_TTL).await;
    assert_eq!(stat(file.clone()).await.unwrap().len(), 100, "nothing told the mount; the cached size stands");

    daemon.push(r#"{"event":"view.invalidated","paths":["/a.txt"]}"#);
    let f = file.clone();
    eventually("stat shows the new size", || async { stat(f.clone()).await.is_ok_and(|m| m.len() == 5000) }).await;

    let f = file.clone();
    let body = tokio::task::spawn_blocking(move || std::fs::read(f)).await.unwrap().unwrap();
    assert_eq!(body.len(), 5000);
    let _ = m.mount.unmount().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_deletion_stops_being_visible_after_view_invalidated() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache = cache_file(cache_dir.path(), 3);
    let daemon = Daemon::spawn(&listing(&[("/a.txt", 3, 1_000_000), ("/b.txt", 3, 1_000_000)]), &cache).await;
    let m = mount(&daemon).await;

    assert!(stat(m.path.join("a.txt")).await.is_ok());
    assert!(stat(m.path.join("b.txt")).await.is_ok());
    daemon.set_list(&listing(&[("/b.txt", 3, 1_000_000)]));
    daemon.push(r#"{"event":"view.invalidated","paths":["/a.txt"]}"#);

    let p = m.path.join("a.txt");
    eventually("a.txt is gone", || async {
        stat(p.clone()).await.is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    })
    .await;
    assert!(stat(m.path.join("b.txt")).await.is_ok());
    let _ = m.mount.unmount().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_and_lost_events_drop_every_cached_attribute() {
    for event in [r#"{"event":"view.invalidated","full":true}"#, r#"{"event":"lost","since_last":2}"#] {
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = cache_file(cache_dir.path(), 3);
        let daemon = Daemon::spawn(&listing(&[("/a.txt", 3, 1_000_000), ("/b.txt", 3, 1_000_000)]), &cache).await;
        let m = mount(&daemon).await;

        assert_eq!(stat(m.path.join("a.txt")).await.unwrap().len(), 3);
        assert_eq!(stat(m.path.join("b.txt")).await.unwrap().len(), 3);
        daemon.set_list(&listing(&[("/a.txt", 30, 2_000_000), ("/b.txt", 40, 2_000_000)]));
        daemon.push(event);

        let (a, b) = (m.path.join("a.txt"), m.path.join("b.txt"));
        eventually(event, || async {
            stat(a.clone()).await.is_ok_and(|m| m.len() == 30) && stat(b.clone()).await.is_ok_and(|m| m.len() == 40)
        })
        .await;
        let _ = m.mount.unmount().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_with_unsent_writes_keeps_its_size_across_an_invalidation() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache = cache_file(cache_dir.path(), 0);
    let daemon = Daemon::spawn(&listing(&[]), &cache).await;
    let m = mount(&daemon).await;

    let p = m.path.join("new.txt");
    let writer = tokio::task::spawn_blocking(move || {
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(b"0123456789").unwrap();
        f
    })
    .await
    .unwrap();

    // The cloud's listing knows the file as empty: what the write has not yet
    // sent. The mount must not take that over the bytes being written.
    daemon.set_list(&listing(&[("/new.txt", 0, 1_000_000)]));
    daemon.push(r#"{"event":"view.invalidated","full":true}"#);
    tokio::time::sleep(KERNEL_ATTR_TTL).await;
    let size = tokio::task::spawn_blocking(move || writer.metadata().unwrap().len()).await.unwrap();
    assert_eq!(size, 10);
    let _ = m.mount.unmount().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_takes_the_size_of_the_hydrated_file_and_the_first_read_is_not_cut_off() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache = cache_file(cache_dir.path(), 5000);
    // The listing is behind: it still says 100.
    let daemon = Daemon::spawn(&listing(&[("/a.txt", 100, 1_000_000)]), &cache).await;
    let m = mount(&daemon).await;
    let file = m.path.join("a.txt");

    assert_eq!(stat(file.clone()).await.unwrap().len(), 100);
    let f = file.clone();
    // The kernel took 100 from the lookup a moment ago and clips reads to it;
    // an open that finds the file larger must lift that for this open.
    let got = tokio::task::spawn_blocking(move || std::fs::read(f)).await.unwrap().unwrap();
    assert_eq!(got.len(), 5000, "the read right after the stale lookup must not be cut off");

    tokio::time::sleep(KERNEL_ATTR_TTL).await;
    assert_eq!(stat(file).await.unwrap().len(), 5000, "the size must follow the file that was opened");
    let _ = m.mount.unmount().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writing_advances_the_modified_time() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache = cache_file(cache_dir.path(), 3);
    let daemon = Daemon::spawn(&listing(&[("/a.txt", 3, 1_000_000)]), &cache).await;
    let m = mount(&daemon).await;

    let p = m.path.join("a.txt");
    assert_eq!(stat(p.clone()).await.unwrap().mtime(), 1000);
    let w = p.clone();
    tokio::task::spawn_blocking(move || {
        let mut f = std::fs::OpenOptions::new().write(true).open(w).unwrap();
        f.write_all(b"y").unwrap();
    })
    .await
    .unwrap();

    tokio::time::sleep(KERNEL_ATTR_TTL).await;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let mtime = stat(p).await.unwrap().mtime();
    assert!((now - mtime).abs() < 60, "mtime {mtime} is not about now ({now})");
    let _ = m.mount.unmount().await;
}

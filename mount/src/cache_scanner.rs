
use crate::ipc::{IpcClient, IpcError};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Debug, thiserror::Error)]
pub enum ScannerError {
    #[error("walk error: {0}")]
    Walk(#[from] std::io::Error),
}

/// Walk `cache_root` and replay missed open_write IPCs. Returns the count
/// of files replayed. `cache_root` may not exist — that's treated as
/// "nothing to replay" (returns 0).
pub async fn scan_and_replay(
    ipc: &mut IpcClient,
    cache_root: &Path,
) -> Result<usize, ScannerError> {
    if !cache_root.exists() {
        return Ok(0);
    }
    let files = collect_files(cache_root)?;
    let mut replayed = 0usize;
    let mut handle_n = 0u64;
    for (cache_path, remote_path) in files {
        let cache_mtime_ms = match mtime_ms(&cache_path, cache_root) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(?e, cache_path=%cache_path.display(), "cache_scanner: stat failed; skipping");
                continue;
            }
        };
        let watermark = match ipc.last_synced(&remote_path).await {
            Ok(w) => w,
            Err(IpcError::Unknown { reason }) => {
                tracing::warn!(
                    %reason,
                    cache_path=%cache_path.display(),
                    remote_path=%remote_path,
                    "cache_scanner: last_synced unknown; skipping replay"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(?e, remote_path=%remote_path, "cache_scanner: last_synced IPC failed; skipping");
                continue;
            }
        };
        // Replay on mtime >= watermark, not strictly >. A freshly-created-
        // then-crashed file is the motivating case: create stamps
        // last_synced = now, so the cache mtime equals the watermark to
        // millisecond resolution (and can even round slightly below it,
        // since the bytes were written just before the row was stamped).
        // A strict `>` lets that file slip through and never gets its
        // bytes to the cloud. The cost of `>=` is re-replaying an
        // already-synced file whose mtime happens to tie the watermark;
        // that is harmless — open_write just re-puts identical bytes
        // (idempotent). For a durability fix, erring toward replay-on-tie
        // is the conservative choice: a spurious re-upload is cheap, a
        // missed upload is silent data-on-cloud loss.
        if cache_mtime_ms < watermark {
            continue;
        }
        handle_n += 1;
        let handle_id = format!("recovery-{handle_n}");
        let cache_path_str = cache_path.to_string_lossy();
        match ipc.open_write(&handle_id, &remote_path, &cache_path_str).await {
            Ok(_) => {
                replayed += 1;
            }
            Err(e) => {
                tracing::warn!(?e, remote_path=%remote_path, "cache_scanner: open_write replay failed");
            }
        }
    }
    Ok(replayed)
}

fn collect_files(root: &Path) -> Result<Vec<(PathBuf, String)>, std::io::Error> {
    let mut out = Vec::new();
    let canon_root = root.canonicalize()?;
    let mut stack: Vec<PathBuf> = vec![canon_root.clone()];
    while let Some(dir) = stack.pop() {
        let read_dir = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(?e, dir=%dir.display(), "cache_scanner: read_dir failed");
                continue;
            }
        };
        for entry in read_dir {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                let canon_dir = match path.canonicalize() {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(?e, dir=%path.display(), "cache_scanner: canonicalize dir failed");
                        continue;
                    }
                };
                if canon_dir.starts_with(&canon_root) {
                    stack.push(canon_dir);
                }
                continue;
            }
            if !file_type.is_file() {
                continue;
            }

            let canon_path = match path.canonicalize() {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(?e, path=%path.display(), "cache_scanner: canonicalize file failed");
                    continue;
                }
            };
            if !canon_path.starts_with(&canon_root) {
                continue;
            }

            let rel = match canon_path.strip_prefix(&canon_root) {
                Ok(r) => r,
                Err(_) => continue,
            };
            // Normalise to a "/"-rooted remote path.
            let remote = format!("/{}", rel.to_string_lossy().replace('\\', "/"));
            out.push((canon_path, remote));
        }
    }
    Ok(out)
}

fn mtime_ms(p: &Path, root: &Path) -> Result<i64, std::io::Error> {
    let canon_root = root.canonicalize()?;
    let canon_p = p.canonicalize()?;
    if !canon_p.starts_with(&canon_root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to stat path outside cache root",
        ));
    }

    let m = std::fs::symlink_metadata(&canon_p)?;
    if m.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to stat symlink path",
        ));
    }
    let mt = m.modified()?;
    let d = mt
        .duration_since(UNIX_EPOCH)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    Ok(d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_jvm::FakeJvm;
    use std::collections::HashMap;
    use std::fs;
    use std::io::Write;
    use std::time::{Duration, SystemTime};

    fn replies(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

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
        // Cache file mtime is now (very recent). Watermark from JVM is 1.
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
        // Set the file's mtime to 1 hour ago (in past) so watermark = now > mtime.
        set_mtime(&cache_file, 3600);
        // Watermark is "now" in ms; cache mtime is 1h old. Reply with a
        // value comfortably ahead of any possible cache mtime.
        let future_ms: i64 = (SystemTime::now()
            .duration_since(UNIX_EPOCH)
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
        // Three files: a/b/foo.txt (newer), bar.txt (newer), old.txt (older).
        let sub = cache_dir.path().join("a").join("b");
        fs::create_dir_all(&sub).unwrap();
        let f1 = sub.join("foo.txt");
        let f2 = cache_dir.path().join("bar.txt");
        let f3 = cache_dir.path().join("old.txt");
        for f in [&f1, &f2, &f3] {
            let mut h = fs::File::create(f).unwrap();
            h.write_all(b"x").unwrap();
        }
        // We need a per-path watermark; FakeJvm only supports per-verb canned
        // replies. Set watermark to "1" so newly-created files (current
        // mtime) are always > watermark. Then push old.txt's mtime to 1970
        // -ish so it falls below the watermark.
        set_mtime(&f3, 1_000_000);
        let jvm = FakeJvm::spawn(replies(&[
            ("hydration.last_synced", r#"{"ok":true,"mtime_ms":1}"#),
            ("hydration.open_write", r#"{"ok":true,"cache_path":"/x"}"#),
        ]))
        .await;
        let mut c = client(&jvm).await;
        let n = scan_and_replay(&mut c, cache_dir.path()).await.unwrap();
        // f3 with mtime around 1970 is still > 1ms; the watermark of 1
        // is so low that ALL three files replay.
        assert_eq!(n, 3, "all three files should replay against watermark=1");
        let recorded = jvm.recorded_requests().await;
        let ow_count = recorded
            .iter()
            .filter(|r| r.contains(r#""verb":"hydration.open_write""#))
            .count();
        assert_eq!(ow_count, 3, "expected 3 open_write calls: {recorded:?}");
        jvm.shutdown().await;
    }
}

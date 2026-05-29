
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
    // `collect_files` returns already-canonical paths rooted under
    // `canon_root`; canonicalize the root once here so `mtime_ms` does not
    // re-canonicalize the path AND the root on every file (one stat per file
    // matters on crash-recovery with a large cache).
    let canon_root = cache_root.canonicalize()?;
    let files = collect_files(cache_root)?;
    let mut replayed = 0usize;
    let mut handle_n = 0u64;
    for (cache_path, remote_path) in files {
        let cache_mtime_ms = match mtime_ms(&cache_path, &canon_root) {
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

/// `canon_p` and `canon_root` are both already canonicalized by the caller
/// (`canon_p` comes from `collect_files`, `canon_root` is canonicalized once
/// in `scan_and_replay`), so this does NOT re-canonicalize per file.
fn mtime_ms(canon_p: &Path, canon_root: &Path) -> Result<i64, std::io::Error> {
    if !canon_p.starts_with(canon_root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to stat path outside cache root",
        ));
    }

    let m = std::fs::symlink_metadata(canon_p)?;
    if m.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to stat symlink path",
        ));
    }
    let mt = m.modified()?;
    let d = mt
        .duration_since(UNIX_EPOCH)
        .map_err(std::io::Error::other)?;
    Ok(d.as_millis() as i64)
}

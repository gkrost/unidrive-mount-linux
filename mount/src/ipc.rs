use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing;

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed reply: {0}")]
    Malformed(String),
    #[error("server reported error: {0}")]
    ServerError(String),
    #[error("busy")]
    Busy,
    #[error("unknown: {reason}")]
    Unknown { reason: String },
}

pub struct IpcClient {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

#[derive(Debug)]
pub struct OpenReadReply {
    pub cache_path: PathBuf,
}

#[derive(Debug)]
pub struct OpenWriteReply {
    pub cache_path: PathBuf,
}

#[derive(Debug)]
pub struct OpenWriteBeginReply {
    pub cache_path: PathBuf,
}

#[derive(Debug)]
pub struct CreateReply {
    pub cache_path: PathBuf,
    pub handle_id: String,
}

#[derive(Debug)]
pub struct ListEntry {
    pub path: String,
    pub size: u64,
    pub mtime_ms: i64,
    pub hydrated: bool,
    pub folder: bool,
}

// Parse one `hydration.list` reply entry.
//
// `size` is clamped to a non-negative `u64`: the JVM serialises sizes as a
// signed 64-bit value, and a stale or i32-overflowed folder size can be
// negative (e.g. a multi-GB folder whose recursive size wrapped past
// `i32::MAX`). A negative (or otherwise non-integer) size must clamp to 0
// rather than reject the whole reply — one poisoned entry would otherwise
// `IpcError::Malformed` the entire `list`, EIO-ing every `readdir`/`lookup`
// on the parent directory. Folder sizes are cosmetic and files re-report
// their true size on open, so clamping is lossless in practice.
fn parse_list_entry(e: &serde_json::Value) -> Result<ListEntry, IpcError> {
    let path = e["path"].as_str().ok_or_else(|| IpcError::Malformed(e.to_string()))?.to_string();
    let size = e["size"]
        .as_u64()
        .or_else(|| e["size"].as_i64().map(|v| v.max(0) as u64))
        .unwrap_or(0);
    let mtime_ms = e["mtime_ms"].as_i64().ok_or_else(|| IpcError::Malformed(e.to_string()))?;
    let hydrated = e["hydrated"].as_bool().ok_or_else(|| IpcError::Malformed(e.to_string()))?;
    let folder = e["folder"].as_bool().ok_or_else(|| IpcError::Malformed(e.to_string()))?;
    Ok(ListEntry { path, size, mtime_ms, hydrated, folder })
}

impl IpcClient {
    pub async fn connect(socket: &Path) -> Result<Self, IpcError> {
        let stream = UnixStream::connect(socket).await?;
        let (r, w) = stream.into_split();
        Ok(Self { reader: BufReader::new(r), writer: w })
    }

    pub async fn open_read(&mut self, handle_id: &str, path: &str) -> Result<OpenReadReply, IpcError> {
        let req = serde_json::json!({
            "verb": "hydration.open_read",
            "handle_id": handle_id,
            "path": path,
        });
        let reply = self.round_trip(&req).await?;
        if !reply["ok"].as_bool().unwrap_or(false) {
            return Err(server_error(&reply));
        }
        let cache = reply["cache_path"].as_str()
            .ok_or_else(|| IpcError::Malformed(reply.to_string()))?;
        Ok(OpenReadReply { cache_path: PathBuf::from(cache) })
    }

    pub async fn open_write(&mut self, handle_id: &str, path: &str, cache_path: &str) -> Result<OpenWriteReply, IpcError> {
        let req = serde_json::json!({
            "verb": "hydration.open_write",
            "handle_id": handle_id,
            "path": path,
            "cache_path": cache_path,
        });
        let reply = self.round_trip(&req).await?;
        if !reply["ok"].as_bool().unwrap_or(false) {
            return Err(server_error(&reply));
        }
        let cache = reply["cache_path"].as_str()
            .ok_or_else(|| IpcError::Malformed(reply.to_string()))?;
        Ok(OpenWriteReply { cache_path: PathBuf::from(cache) })
    }

    /// Call `hydration.open_write_begin`.
    ///
    /// `handle_id`: when `Some`, included in the request so the JVM registers a
    /// JVM-side open-set entry for this live O_TRUNC open (enabling busy-checks /
    /// dehydrate guards).  When `None` (one-shot bare-truncate / setattr path),
    /// the field is omitted — the JVM performs no open-set registration, matching
    /// the pre-existing no-spurious-close_handle contract.
    pub async fn open_write_begin(
        &mut self,
        path: &str,
        handle_id: Option<&str>,
    ) -> Result<OpenWriteBeginReply, IpcError> {
        let req = if let Some(hid) = handle_id {
            serde_json::json!({
                "verb": "hydration.open_write_begin",
                "path": path,
                "handle_id": hid,
            })
        } else {
            serde_json::json!({ "verb": "hydration.open_write_begin", "path": path })
        };
        let reply = self.round_trip(&req).await?;
        if !reply["ok"].as_bool().unwrap_or(false) {
            return Err(server_error(&reply));
        }
        let cache = reply["cache_path"].as_str()
            .ok_or_else(|| IpcError::Malformed(reply.to_string()))?;
        Ok(OpenWriteBeginReply { cache_path: PathBuf::from(cache) })
    }

    pub async fn close_handle(&mut self, handle_id: &str) -> Result<(), IpcError> {
        let req = serde_json::json!({
            "verb": "hydration.close_handle",
            "handle_id": handle_id,
        });
        let reply = self.round_trip(&req).await?;
        if !reply["ok"].as_bool().unwrap_or(false) {
            return Err(server_error(&reply));
        }
        Ok(())
    }

    pub async fn hydrate(&mut self, path: &str) -> Result<(), IpcError> {
        let req = serde_json::json!({
            "verb": "hydration.hydrate",
            "path": path,
        });
        let reply = self.round_trip(&req).await?;
        if !reply["ok"].as_bool().unwrap_or(false) {
            return Err(server_error(&reply));
        }
        Ok(())
    }

    pub async fn dehydrate(&mut self, path: &str) -> Result<(), IpcError> {
        let req = serde_json::json!({
            "verb": "hydration.dehydrate",
            "path": path,
        });
        let reply = self.round_trip(&req).await?;
        if !reply["ok"].as_bool().unwrap_or(false) {
            return Err(server_error(&reply));
        }
        Ok(())
    }

    /// Subscribe handshake. After the {"ok":true} reply, the connection becomes
    /// a one-way NDJSON event stream. The Phase-2 client does NOT consume that
    /// stream — Phase 3 work. Reconnect/retry wrappers in Task 3 must NOT wrap
    /// this connection (re-subscribing silently after a drop loses any events
    /// fired during the disconnect window).
    pub async fn subscribe(&mut self) -> Result<(), IpcError> {
        let req = serde_json::json!({
            "verb": "hydration.subscribe",
        });
        let reply = self.round_trip(&req).await?;
        if !reply["ok"].as_bool().unwrap_or(false) {
            return Err(server_error(&reply));
        }
        Ok(())
    }

    pub async fn last_synced(&mut self, path: &str) -> Result<i64, IpcError> {
        let req = serde_json::json!({
            "verb": "hydration.last_synced",
            "path": path,
        });
        let reply = self.round_trip(&req).await?;
        if !reply["ok"].as_bool().unwrap_or(false) {
            // Per the canonical contract (LastSyncedResult.Unknown(reason: String)),
            // the failure carries a dynamic reason string. Surface it as the
            // dedicated Unknown variant so callers can pattern-match without
            // coupling to the exact JVM-side wording ("unknown_path", "no_mtime", …).
            let reason = reply["error"].as_str().unwrap_or("unknown").to_string();
            return Err(IpcError::Unknown { reason });
        }
        reply["mtime_ms"].as_i64()
            .ok_or_else(|| IpcError::Malformed(reply.to_string()))
    }

    pub async fn list(&mut self, prefix: &str) -> Result<Vec<ListEntry>, IpcError> {
        let req = serde_json::json!({
            "verb": "hydration.list",
            "prefix": prefix,
        });
        let reply = self.round_trip(&req).await?;
        if !reply["ok"].as_bool().unwrap_or(false) {
            return Err(server_error(&reply));
        }
        let entries = reply["entries"].as_array()
            .ok_or_else(|| IpcError::Malformed(reply.to_string()))?;
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            out.push(parse_list_entry(e)?);
        }
        Ok(out)
    }

    async fn round_trip(&mut self, req: &serde_json::Value) -> Result<serde_json::Value, IpcError> {
        let line = req.to_string();
        // Enforce the same MAX_REQUEST_BYTES = 64 * 1024 cap the JVM IpcServer
        // imposes on its inbound buffer. Going past it would cause the JVM to
        // disconnect mid-request rather than reply.
        if line.len() + 1 > 64 * 1024 {
            return Err(IpcError::Malformed(format!(
                "request {} bytes exceeds 64 KiB JVM cap",
                line.len()
            )));
        }
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await?;
        // Bound the inbound line at 4 MiB so an unbounded `list` reply can't
        // OOM the co-daemon.
        let mut buf = String::with_capacity(1024);
        let n = (&mut self.reader).take(4 * 1024 * 1024).read_line(&mut buf).await?;
        if n == 0 {
            tracing::warn!("ipc round_trip: unexpected EOF (daemon disconnected?)");
            return Err(IpcError::Io(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)));
        }
        let trimmed = buf.trim_end_matches('\n');
        serde_json::from_str(trimmed).map_err(|e| IpcError::Malformed(format!("{e}: {trimmed}")))
    }

    /// Read the next NDJSON event line from a subscribe stream.  Blocks until
    /// a line arrives or the connection drops (JVM teardown → UnexpectedEof).
    /// After calling `subscribe()`, use this to consume the event stream.
    pub async fn read_event_line(&mut self) -> Result<String, IpcError> {
        let mut buf = String::with_capacity(512);
        let n = (&mut self.reader).take(4 * 1024 * 1024).read_line(&mut buf).await?;
        if n == 0 {
            return Err(IpcError::Io(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)));
        }
        Ok(buf.trim_end_matches('\n').to_string())
    }

    pub async fn mkdir(&mut self, path: &str) -> Result<(), IpcError> {
        let req = serde_json::json!({"verb": "hydration.mkdir", "path": path});
        let reply = self.round_trip(&req).await?;
        if !reply["ok"].as_bool().unwrap_or(false) {
            return Err(server_error(&reply));
        }
        Ok(())
    }

    pub async fn unlink(&mut self, path: &str) -> Result<(), IpcError> {
        let req = serde_json::json!({"verb": "hydration.unlink", "path": path});
        let reply = self.round_trip(&req).await?;
        if !reply["ok"].as_bool().unwrap_or(false) {
            return Err(server_error(&reply));
        }
        Ok(())
    }

    pub async fn rmdir(&mut self, path: &str) -> Result<(), IpcError> {
        let req = serde_json::json!({"verb": "hydration.rmdir", "path": path});
        let reply = self.round_trip(&req).await?;
        if !reply["ok"].as_bool().unwrap_or(false) {
            return Err(server_error(&reply));
        }
        Ok(())
    }

    pub async fn create(&mut self, handle_id: &str, path: &str) -> Result<CreateReply, IpcError> {
        let req = serde_json::json!({
            "verb": "hydration.create",
            "handle_id": handle_id,
            "path": path,
        });
        let reply = self.round_trip(&req).await?;
        if !reply["ok"].as_bool().unwrap_or(false) {
            return Err(server_error(&reply));
        }
        let cache = reply["cache_path"].as_str()
            .ok_or_else(|| IpcError::Malformed(reply.to_string()))?;
        // handle_id in the reply may echo what we sent or be a server-allocated
        // value; treat it as opaque per the contract.
        let hid = reply["handle_id"].as_str()
            .ok_or_else(|| IpcError::Malformed(reply.to_string()))?;
        Ok(CreateReply { cache_path: PathBuf::from(cache), handle_id: hid.to_string() })
    }

    pub async fn rename(&mut self, old_path: &str, new_path: &str) -> Result<(), IpcError> {
        let req = serde_json::json!({
            "verb": "hydration.rename",
            "old_path": old_path,
            "new_path": new_path,
        });
        let reply = self.round_trip(&req).await?;
        if !reply["ok"].as_bool().unwrap_or(false) {
            return Err(server_error(&reply));
        }
        Ok(())
    }
}

fn server_error(reply: &serde_json::Value) -> IpcError {
    let err = reply["error"].as_str().unwrap_or("unknown");
    // `dehydrate` returns the literal "busy" — load-bearing for the dehydrate-
    // while-open test in Task 3 (EBUSY propagation).
    if err == "busy" {
        return IpcError::Busy;
    }
    IpcError::ServerError(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Invariant: a negative `size` (stale/i32-overflowed folder size) clamps to 0
    // and is parsed successfully. Regression guard for the EIO-on-readdir bug where
    // one negative-size entry rejected the whole `hydration.list` reply.
    #[test]
    fn parse_list_entry_clamps_negative_size_instead_of_failing() {
        let e = json!({
            "path": "/gernot", "size": -650676500i64,
            "mtime_ms": 1779866556384i64, "hydrated": false, "folder": true,
        });
        let entry = parse_list_entry(&e).expect("negative size must not reject the entry");
        assert_eq!(entry.path, "/gernot");
        assert_eq!(entry.size, 0, "negative size must clamp to 0");
        assert!(entry.folder);
    }

    // Invariant: a multi-GB size that exceeds i32 but is a valid positive u64 is
    // preserved exactly — the clamp must not truncate legitimately large files.
    #[test]
    fn parse_list_entry_preserves_large_positive_size() {
        let e = json!({
            "path": "/big.bin", "size": 3_644_290_796u64,
            "mtime_ms": 1i64, "hydrated": true, "folder": false,
        });
        let entry = parse_list_entry(&e).expect("large size must parse");
        assert_eq!(entry.size, 3_644_290_796);
    }

    // Invariant: a missing/non-numeric size defaults to 0 rather than rejecting.
    #[test]
    fn parse_list_entry_defaults_absent_size_to_zero() {
        let e = json!({ "path": "/x", "mtime_ms": 1i64, "hydrated": false, "folder": false });
        let entry = parse_list_entry(&e).expect("absent size must default, not fail");
        assert_eq!(entry.size, 0);
    }
}

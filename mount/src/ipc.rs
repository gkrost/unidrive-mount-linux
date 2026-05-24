use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

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
pub struct ListEntry {
    pub path: String,
    pub size: u64,
    pub mtime_ms: i64,
    pub hydrated: bool,
    pub folder: bool,
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
            let path = e["path"].as_str().ok_or_else(|| IpcError::Malformed(e.to_string()))?.to_string();
            let size = e["size"].as_u64().ok_or_else(|| IpcError::Malformed(e.to_string()))?;
            let mtime_ms = e["mtime_ms"].as_i64().ok_or_else(|| IpcError::Malformed(e.to_string()))?;
            let hydrated = e["hydrated"].as_bool().ok_or_else(|| IpcError::Malformed(e.to_string()))?;
            let folder = e["folder"].as_bool().ok_or_else(|| IpcError::Malformed(e.to_string()))?;
            out.push(ListEntry { path, size, mtime_ms, hydrated, folder });
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
            return Err(IpcError::Io(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)));
        }
        let trimmed = buf.trim_end_matches('\n');
        serde_json::from_str(trimmed).map_err(|e| IpcError::Malformed(format!("{e}: {trimmed}")))
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

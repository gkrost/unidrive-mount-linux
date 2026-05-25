//! Reconnecting IPC client wrapper.
//!
//! Wraps an [`IpcClient`] so that an `IpcError::Io` from a disconnected
//! connection triggers a reconnect attempt. Retry every `interval` for up
//! to `total_budget`; default 5s/60s per Phase 2 spec.
//!
//! **Subscribe is intentionally NOT wrapped.** Subscribe opens a long-lived
//! NDJSON event stream; a silent reconnect after a drop would miss every
//! event fired during the disconnect window. Callers consuming subscribe
//! must operate on a raw [`IpcClient`] obtained via [`IpcClient::connect`]
//! directly.

use crate::ipc::{CreateReply, IpcClient, IpcError, ListEntry, OpenReadReply, OpenWriteReply};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default retry interval between connection attempts.
pub const DEFAULT_RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// Default total budget for reconnection attempts before surfacing the error.
pub const DEFAULT_RETRY_BUDGET: Duration = Duration::from_secs(60);

pub struct ReconnectingIpcClient {
    socket: PathBuf,
    inner: Option<IpcClient>,
    interval: Duration,
    budget: Duration,
}

impl ReconnectingIpcClient {
    /// Connect with production defaults (5s interval, 60s budget).
    pub async fn connect(socket: &Path) -> Result<Self, IpcError> {
        Self::connect_with(socket, DEFAULT_RETRY_INTERVAL, DEFAULT_RETRY_BUDGET).await
    }

    /// Connect with caller-supplied retry parameters. Used by tests to
    /// shorten the budget so a deterministic reconnect cycle finishes fast.
    pub async fn connect_with(
        socket: &Path,
        interval: Duration,
        budget: Duration,
    ) -> Result<Self, IpcError> {
        let inner = IpcClient::connect(socket).await?;
        Ok(Self {
            socket: socket.to_path_buf(),
            inner: Some(inner),
            interval,
            budget,
        })
    }

    /// Attempt to (re)connect within the retry budget. Called transparently
    /// on the next verb call after an Io error invalidated the connection.
    async fn ensure_connected(&mut self) -> Result<(), IpcError> {
        if self.inner.is_some() {
            return Ok(());
        }
        let start = tokio::time::Instant::now();
        loop {
            match IpcClient::connect(&self.socket).await {
                Ok(c) => {
                    self.inner = Some(c);
                    return Ok(());
                }
                Err(e) => {
                    if start.elapsed() + self.interval > self.budget {
                        return Err(e);
                    }
                    tokio::time::sleep(self.interval).await;
                }
            }
        }
    }
}

// One non-macro retry loop per verb. Repetitive on purpose: async closure
// lifetime handling for `&mut self` is not worth the abstraction here, and
// three similar copies beat one premature `dyn Future` indirection.
impl ReconnectingIpcClient {
    pub async fn open_read(
        &mut self,
        handle_id: &str,
        path: &str,
    ) -> Result<OpenReadReply, IpcError> {
        loop {
            self.ensure_connected().await?;
            let c = self.inner.as_mut().expect("ensure_connected guarantees Some");
            match c.open_read(handle_id, path).await {
                Ok(v) => return Ok(v),
                Err(IpcError::Io(_)) => {
                    self.inner = None;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn open_write(
        &mut self,
        handle_id: &str,
        path: &str,
        cache_path: &str,
    ) -> Result<OpenWriteReply, IpcError> {
        loop {
            self.ensure_connected().await?;
            let c = self.inner.as_mut().expect("ensure_connected guarantees Some");
            match c.open_write(handle_id, path, cache_path).await {
                Ok(v) => return Ok(v),
                Err(IpcError::Io(_)) => {
                    self.inner = None;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn close_handle(&mut self, handle_id: &str) -> Result<(), IpcError> {
        loop {
            self.ensure_connected().await?;
            let c = self.inner.as_mut().expect("ensure_connected guarantees Some");
            match c.close_handle(handle_id).await {
                Ok(()) => return Ok(()),
                Err(IpcError::Io(_)) => {
                    self.inner = None;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn hydrate(&mut self, path: &str) -> Result<(), IpcError> {
        loop {
            self.ensure_connected().await?;
            let c = self.inner.as_mut().expect("ensure_connected guarantees Some");
            match c.hydrate(path).await {
                Ok(()) => return Ok(()),
                Err(IpcError::Io(_)) => {
                    self.inner = None;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn dehydrate(&mut self, path: &str) -> Result<(), IpcError> {
        loop {
            self.ensure_connected().await?;
            let c = self.inner.as_mut().expect("ensure_connected guarantees Some");
            match c.dehydrate(path).await {
                Ok(()) => return Ok(()),
                Err(IpcError::Io(_)) => {
                    self.inner = None;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn last_synced(&mut self, path: &str) -> Result<i64, IpcError> {
        loop {
            self.ensure_connected().await?;
            let c = self.inner.as_mut().expect("ensure_connected guarantees Some");
            match c.last_synced(path).await {
                Ok(v) => return Ok(v),
                Err(IpcError::Io(_)) => {
                    self.inner = None;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn list(&mut self, prefix: &str) -> Result<Vec<ListEntry>, IpcError> {
        loop {
            self.ensure_connected().await?;
            let c = self.inner.as_mut().expect("ensure_connected guarantees Some");
            match c.list(prefix).await {
                Ok(v) => return Ok(v),
                Err(IpcError::Io(_)) => {
                    self.inner = None;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn mkdir(&mut self, path: &str) -> Result<(), IpcError> {
        loop {
            self.ensure_connected().await?;
            let c = self.inner.as_mut().expect("ensure_connected guarantees Some");
            match c.mkdir(path).await {
                Ok(v) => return Ok(v),
                Err(IpcError::Io(_)) => {
                    self.inner = None;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn unlink(&mut self, path: &str) -> Result<(), IpcError> {
        loop {
            self.ensure_connected().await?;
            let c = self.inner.as_mut().expect("ensure_connected guarantees Some");
            match c.unlink(path).await {
                Ok(v) => return Ok(v),
                Err(IpcError::Io(_)) => {
                    self.inner = None;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn rmdir(&mut self, path: &str) -> Result<(), IpcError> {
        loop {
            self.ensure_connected().await?;
            let c = self.inner.as_mut().expect("ensure_connected guarantees Some");
            match c.rmdir(path).await {
                Ok(v) => return Ok(v),
                Err(IpcError::Io(_)) => {
                    self.inner = None;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn create(&mut self, handle_id: &str, path: &str) -> Result<CreateReply, IpcError> {
        loop {
            self.ensure_connected().await?;
            let c = self.inner.as_mut().expect("ensure_connected guarantees Some");
            match c.create(handle_id, path).await {
                Ok(v) => return Ok(v),
                Err(IpcError::Io(_)) => {
                    self.inner = None;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn rename(&mut self, old_path: &str, new_path: &str) -> Result<(), IpcError> {
        loop {
            self.ensure_connected().await?;
            let c = self.inner.as_mut().expect("ensure_connected guarantees Some");
            match c.rename(old_path, new_path).await {
                Ok(v) => return Ok(v),
                Err(IpcError::Io(_)) => {
                    self.inner = None;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    // Deliberately NO `subscribe` method. See module docstring.
}

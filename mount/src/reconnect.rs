
use crate::ipc_auth::IpcAuth;
use crate::ipc::{CreateReply, IpcClient, IpcError, ListEntry, OpenReadReply, OpenWriteBeginReply, OpenWriteReply};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing;

/// Default retry interval between connection attempts.
pub const DEFAULT_RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// Default total budget for reconnection attempts before surfacing the error.
pub const DEFAULT_RETRY_BUDGET: Duration = Duration::from_secs(60);

pub struct ReconnectingIpcClient {
    socket: PathBuf,
    auth: Option<IpcAuth>,
    inner: Option<IpcClient>,
    interval: Duration,
    budget: Duration,
    // A refused handshake is terminal for this client: every later call
    // fails fast with the same error instead of re-handshaking.
    auth_failure: Option<String>,
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
        Self::connect_inner(socket, None, interval, budget).await
    }

    /// Like `connect`, but every connection (the first and each reconnect)
    /// runs the protocol-2 handshake first.
    pub async fn connect_auth(socket: &Path, auth: IpcAuth) -> Result<Self, IpcError> {
        Self::connect_auth_with(socket, auth, DEFAULT_RETRY_INTERVAL, DEFAULT_RETRY_BUDGET).await
    }

    pub async fn connect_auth_with(
        socket: &Path,
        auth: IpcAuth,
        interval: Duration,
        budget: Duration,
    ) -> Result<Self, IpcError> {
        Self::connect_inner(socket, Some(auth), interval, budget).await
    }

    async fn connect_inner(
        socket: &Path,
        auth: Option<IpcAuth>,
        interval: Duration,
        budget: Duration,
    ) -> Result<Self, IpcError> {
        let inner = open(socket, auth.as_ref()).await?;
        Ok(Self {
            socket: socket.to_path_buf(),
            auth,
            inner: Some(inner),
            interval,
            budget,
            auth_failure: None,
        })
    }

    async fn ensure_connected(&mut self) -> Result<(), IpcError> {
        if let Some(msg) = &self.auth_failure {
            return Err(IpcError::ServerError(msg.clone()));
        }
        if self.inner.is_some() {
            return Ok(());
        }
        let start = tokio::time::Instant::now();
        loop {
            match open(&self.socket, self.auth.as_ref()).await {
                Ok(c) => {
                    self.inner = Some(c);
                    return Ok(());
                }
                Err(e) if e.is_auth_terminal() => {
                    tracing::error!(error=%e, "reconnect: IPC authentication failed; not retrying");
                    if let IpcError::ServerError(msg) = &e {
                        self.auth_failure = Some(msg.clone());
                    }
                    return Err(e);
                }
                Err(e) => {
                    // Compare elapsed AFTER the failed attempt, not before the
                    // next sleep. Pre-adding `self.interval` surfaced the error
                    // ~one interval early; the budget is the wall-clock deadline
                    // for *attempts*, so we only give up once it has truly elapsed.
                    if start.elapsed() >= self.budget {
                        return Err(e);
                    }
                    tokio::time::sleep(self.interval).await;
                }
            }
        }
    }
}

async fn open(socket: &Path, auth: Option<&IpcAuth>) -> Result<IpcClient, IpcError> {
    match auth {
        Some(a) => IpcClient::connect_auth(socket, a).await,
        None => IpcClient::connect(socket).await,
    }
}

// Two macro_rules! helpers that collapse the per-verb boilerplate into one
// line each. Idempotent reads get `retry_io_loop!` (retry on IpcError::Io);
// mutating verbs get `no_retry_on_io!` (surface the error, invalidate connection).
// See each call site for the verb name used to generate the trace breadcrumb.
macro_rules! retry_io_loop {
    ($self:expr, $method:ident($($arg:expr),*)) => {{
        loop {
            $self.ensure_connected().await?;
            let c = $self.inner.as_mut().expect("ensure_connected guarantees Some");
            match c.$method($($arg),*).await {
                Ok(v) => return Ok(v),
                Err(IpcError::Io(_)) => {
                    tracing::warn!(concat!("reconnect: Io error on ", stringify!($method), ", retrying"));
                    $self.inner = None;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }};
}

macro_rules! no_retry_on_io {
    ($self:expr, $method:ident($($arg:expr),*)) => {{
        $self.ensure_connected().await?;
        let c = $self.inner.as_mut().expect("ensure_connected guarantees Some");
        match c.$method($($arg),*).await {
            Ok(v) => Ok(v),
            Err(IpcError::Io(e)) => {
                tracing::warn!(concat!("reconnect: Io error on ", stringify!($method), " (non-idempotent, not retrying)"));
                $self.inner = None;
                Err(IpcError::Io(e))
            }
            Err(e) => Err(e),
        }
    }};
}

impl ReconnectingIpcClient {
    pub async fn open_read(
        &mut self,
        handle_id: &str,
        path: &str,
    ) -> Result<OpenReadReply, IpcError> {
        retry_io_loop!(self, open_read(handle_id, path))
    }

    pub async fn open_write(
        &mut self,
        handle_id: &str,
        path: &str,
        cache_path: &str,
    ) -> Result<OpenWriteReply, IpcError> {
        no_retry_on_io!(self, open_write(handle_id, path, cache_path))
    }

    pub async fn open_write_begin(
        &mut self,
        path: &str,
        handle_id: Option<&str>,
    ) -> Result<OpenWriteBeginReply, IpcError> {
        no_retry_on_io!(self, open_write_begin(path, handle_id))
    }

    /// Retrying `close_handle` across a reconnect is safe: the JVM-side handle
    /// is keyed by ID, so a re-`close_handle` on a fresh connection at worst
    /// returns a benign `handle_not_found` error (surfaced as a non-Io error,
    /// which exits the retry loop) rather than silently corrupting state.
    pub async fn close_handle(&mut self, handle_id: &str) -> Result<(), IpcError> {
        retry_io_loop!(self, close_handle(handle_id))
    }

    pub async fn hydrate(&mut self, path: &str) -> Result<(), IpcError> {
        retry_io_loop!(self, hydrate(path))
    }

    pub async fn dehydrate(&mut self, path: &str) -> Result<(), IpcError> {
        retry_io_loop!(self, dehydrate(path))
    }

    pub async fn last_synced(&mut self, path: &str) -> Result<i64, IpcError> {
        retry_io_loop!(self, last_synced(path))
    }

    pub async fn list(&mut self, prefix: &str) -> Result<Vec<ListEntry>, IpcError> {
        retry_io_loop!(self, list(prefix))
    }

    pub async fn mkdir(&mut self, path: &str) -> Result<(), IpcError> {
        no_retry_on_io!(self, mkdir(path))
    }

    pub async fn unlink(&mut self, path: &str) -> Result<(), IpcError> {
        no_retry_on_io!(self, unlink(path))
    }

    pub async fn rmdir(&mut self, path: &str) -> Result<(), IpcError> {
        no_retry_on_io!(self, rmdir(path))
    }

    pub async fn create(&mut self, handle_id: &str, path: &str) -> Result<CreateReply, IpcError> {
        no_retry_on_io!(self, create(handle_id, path))
    }

    pub async fn rename(&mut self, old_path: &str, new_path: &str) -> Result<(), IpcError> {
        no_retry_on_io!(self, rename(old_path, new_path))
    }

    // Deliberately NO `subscribe` method. See module docstring.
}

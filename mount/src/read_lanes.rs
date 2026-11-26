//! Extra IPC connections for read-only opens.
//!
//! A cold `hydration.open_read` is answered only after the daemon has downloaded the whole file. On the
//! one shared connection that serialises every cold open of the mount, whatever the number of readers, and
//! holds up `lookup`, `readdir` and the rest behind it. A read-only open therefore goes through one of a few
//! dedicated connections: the first idle one, else the next one in turn, so a download only ever waits for
//! another download when every lane is busy. Everything else, including the release of the handle (the daemon
//! resolves `hydration.close_handle` by handle id, whichever connection opened it), stays on the main connection.

use crate::ipc::{IpcError, OpenReadReply};
use crate::reconnect::ReconnectingIpcClient;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Mutex;

pub struct ReadLanes {
    lanes: Vec<Mutex<ReconnectingIpcClient>>,
    next: AtomicUsize,
}

impl ReadLanes {
    pub fn new(clients: Vec<ReconnectingIpcClient>) -> Self {
        Self {
            lanes: clients.into_iter().map(Mutex::new).collect(),
            next: AtomicUsize::new(0),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.lanes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.lanes.len()
    }

    /// `hydration.open_read` on an idle lane, or queued behind the next lane in turn when all are busy.
    /// Must not be called on an empty set.
    pub async fn open_read(&self, handle_id: &str, path: &str) -> Result<OpenReadReply, IpcError> {
        let n = self.lanes.len();
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        for i in 0..n {
            if let Ok(mut lane) = self.lanes[(start + i) % n].try_lock() {
                return lane.open_read(handle_id, path).await;
            }
        }
        self.lanes[start % n].lock().await.open_read(handle_id, path).await
    }
}

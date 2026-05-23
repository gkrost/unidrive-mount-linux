use crate::ipc::IpcClient;
use crate::path_map::PathMap;
use fuse3::raw::prelude::*;
use fuse3::raw::Request;
use fuse3::Result;
use std::num::NonZeroU32;
use std::sync::Arc;
use tokio::sync::Mutex;

/// The unidrive FUSE filesystem.
///
/// Read-side state lives here. Holds:
/// - `ipc`: shared `IpcClient` over `Arc<Mutex<…>>` (FUSE methods run
///   concurrently per the async trait; the JVM IpcServer assumes one
///   in-flight verb per connection, so we serialise verb calls).
/// - `paths`: inode <-> remote-path bidirectional map.
///
/// Task 2 wires `getattr`, `readdir`, `open`, `read`, `release`. Task 3 will
/// add `write`/`fsync`/dirty-release tracking.
pub struct UnidriveFs {
    #[allow(dead_code)] // used by Task 2 sub-step 4 (getattr/readdir) and 5 (open/read/release)
    pub(crate) ipc: Arc<Mutex<IpcClient>>,
    #[allow(dead_code)]
    pub(crate) paths: Arc<Mutex<PathMap>>,
}

impl UnidriveFs {
    pub fn new(ipc: Arc<Mutex<IpcClient>>) -> Self {
        Self {
            ipc,
            paths: Arc::new(Mutex::new(PathMap::new())),
        }
    }
}

impl Filesystem for UnidriveFs {
    async fn init(&self, _req: Request) -> Result<ReplyInit> {
        Ok(ReplyInit {
            // 1 MiB max write. Read-path Task 2 doesn't write; Task 3 may
            // raise this once write throughput becomes a concern.
            max_write: NonZeroU32::new(1 << 20).expect("non-zero"),
        })
    }

    async fn destroy(&self, _req: Request) {
        // Nothing to flush in Task 2 (read-only path). Task 3 will fsync the
        // cache dirty-set here.
    }
}

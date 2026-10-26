//! Keeps the cached view in step with the daemon: the change-event stream
//! drops the cached attributes of what changed, so the next `lookup` or
//! `getattr` lists the parent again.

use crate::fuse_fs::ViewCache;
use crate::ipc_auth::IpcAuth;
use crate::subscribe::{invalidation_of, run_subscription, SubscriptionEvent};
use std::path::Path;
use std::time::Duration;

/// Run the change-event subscription (see `run_subscription`) and hand each
/// invalidation it calls for to `view`. Returns when the subscription ends.
pub async fn keep_view_fresh(socket: &Path, auth: &IpcAuth, interval: Duration, view: ViewCache) {
    // The callback is synchronous and the cache is behind async locks.
    let (changes, mut pending) = tokio::sync::mpsc::unbounded_channel();
    let applier = tokio::spawn(async move {
        while let Some(change) = pending.recv().await {
            view.apply(change).await;
        }
    });
    run_subscription(socket, auth, interval, move |ev| {
        if matches!(ev, SubscriptionEvent::Gap) {
            tracing::warn!("subscribe: stream re-established after a gap; events in between are lost, dropping all cached attributes");
        }
        match invalidation_of(&ev) {
            Some(change) => {
                let _ = changes.send(change);
            }
            None => {
                if let SubscriptionEvent::Line(line) = &ev {
                    tracing::trace!(event=%line, "subscribe event");
                }
            }
        }
        true
    })
    .await;
    let _ = applier.await;
}

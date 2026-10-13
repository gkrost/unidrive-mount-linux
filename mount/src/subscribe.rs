//! The long-lived `hydration.subscribe` event stream, kept alive across
//! daemon restarts.
//!
//! The request connection is wrapped by `ReconnectingIpcClient`; the event
//! stream cannot be, because a silent reconnect hides the events lost while
//! the stream was down. This loop re-establishes the stream itself (same
//! retry interval and protocol-2 handshake as the request path) and reports
//! the gap to the caller exactly once per re-establishment.

use crate::ipc::IpcClient;
use crate::ipc_auth::IpcAuth;
use std::path::Path;
use std::time::Duration;

/// What the subscription loop hands to its consumer.
#[derive(Debug, PartialEq, Eq)]
pub enum SubscriptionEvent {
    /// One NDJSON event line from the daemon.
    Line(String),
    /// The stream was re-established after a drop; events in between are
    /// lost, so the whole view must be treated as invalidated once.
    Gap,
}

/// Keep `hydration.subscribe` established until `on_event` returns `false`
/// or the daemon refuses authentication (terminal; retrying cannot help).
/// A dropped stream or failed connect is retried every `interval`.
pub async fn run_subscription<F>(
    socket: &Path,
    auth: &IpcAuth,
    interval: Duration,
    mut on_event: F,
) where
    F: FnMut(SubscriptionEvent) -> bool,
{
    let mut had_stream = false;
    loop {
        let mut sub = match IpcClient::connect_auth(socket, auth).await {
            Ok(c) => c,
            Err(e) if e.is_auth_terminal() => {
                tracing::error!(error=%e, "subscribe: IPC authentication failed; not retrying");
                return;
            }
            Err(e) => {
                tracing::warn!(error=%e, "subscribe: connect failed, retrying");
                tokio::time::sleep(interval).await;
                continue;
            }
        };
        if let Err(e) = sub.subscribe().await {
            tracing::warn!(error=%e, "subscribe: handshake failed, retrying");
            tokio::time::sleep(interval).await;
            continue;
        }
        tracing::info!("hydration.subscribe established");
        if had_stream && !on_event(SubscriptionEvent::Gap) {
            return;
        }
        had_stream = true;
        loop {
            match sub.read_event_line().await {
                Ok(line) => {
                    if !on_event(SubscriptionEvent::Line(line)) {
                        return;
                    }
                }
                Err(e) => {
                    tracing::warn!(error=%e, "subscribe: event stream ended, re-establishing");
                    break;
                }
            }
        }
        tokio::time::sleep(interval).await;
    }
}

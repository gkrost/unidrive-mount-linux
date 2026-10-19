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

/// What a change event means for the cached view.
#[derive(Debug, PartialEq, Eq)]
pub enum Invalidation {
    /// These paths, and anything below them, changed.
    Paths(Vec<String>),
    /// Anything may have changed.
    All,
}

/// The invalidation `ev` calls for, if any. `view.invalidated` names the
/// changed paths or says `full`; `lost` (events dropped for this subscriber)
/// and a `Gap` (the stream was down) leave the changed paths unknown. An
/// invalidation that cannot be read is treated as `All`.
pub fn invalidation_of(ev: &SubscriptionEvent) -> Option<Invalidation> {
    let line = match ev {
        SubscriptionEvent::Gap => return Some(Invalidation::All),
        SubscriptionEvent::Line(line) => line,
    };
    let event: serde_json::Value = serde_json::from_str(line).ok()?;
    match event["event"].as_str()? {
        "lost" => Some(Invalidation::All),
        "view.invalidated" => {
            if event["full"].as_bool() == Some(true) {
                return Some(Invalidation::All);
            }
            let Some(listed) = event["paths"].as_array() else {
                return Some(Invalidation::All);
            };
            let mut paths = Vec::with_capacity(listed.len());
            for p in listed {
                match p.as_str() {
                    // The root, or something that is not a path: no narrower claim to make.
                    Some(p) if !p.is_empty() && p != "/" => paths.push(p.to_string()),
                    _ => return Some(Invalidation::All),
                }
            }
            Some(Invalidation::Paths(paths))
        }
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn line(json: &str) -> SubscriptionEvent {
        SubscriptionEvent::Line(json.to_string())
    }

    #[test]
    fn view_invalidated_names_its_paths() {
        let ev = line(r#"{"event":"view.invalidated","paths":["/a","/b/c"],"moved":[{"from":"/a","to":"/b"}]}"#);
        assert_eq!(invalidation_of(&ev), Some(Invalidation::Paths(vec!["/a".into(), "/b/c".into()])));
    }

    #[test]
    fn full_lost_and_gap_invalidate_everything() {
        assert_eq!(invalidation_of(&line(r#"{"event":"view.invalidated","full":true}"#)), Some(Invalidation::All));
        assert_eq!(invalidation_of(&line(r#"{"event":"lost","since_last":3}"#)), Some(Invalidation::All));
        assert_eq!(invalidation_of(&SubscriptionEvent::Gap), Some(Invalidation::All));
    }

    #[test]
    fn an_unreadable_invalidation_invalidates_everything() {
        for json in [
            r#"{"event":"view.invalidated"}"#,
            r#"{"event":"view.invalidated","paths":"/a"}"#,
            r#"{"event":"view.invalidated","paths":["/a",7]}"#,
            r#"{"event":"view.invalidated","paths":["/"]}"#,
            r#"{"event":"view.invalidated","paths":[""]}"#,
        ] {
            assert_eq!(invalidation_of(&line(json)), Some(Invalidation::All), "{json}");
        }
    }

    #[test]
    fn other_events_and_garbage_invalidate_nothing() {
        for json in [
            r#"{"event":"hydrated","path":"/a","bytes":3}"#,
            r#"{"event":"completed","path":"/a","handle_id":"h1","direction":"upload","ok":true}"#,
            r#"{"event":"history.lost","since_last":3}"#,
            r#"{"no_event":1}"#,
            "not json",
        ] {
            assert_eq!(invalidation_of(&line(json)), None, "{json}");
        }
    }
}

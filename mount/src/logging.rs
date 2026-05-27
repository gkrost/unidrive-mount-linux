use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

/// Initialise the global tracing subscriber.
///
/// Called at the very top of `run_with_argv`, before anything else can log.
/// Without this, every `tracing::*` call in the co-daemon is a no-op — the
/// process emits nothing even at `RUST_LOG=debug`, which made the read-path
/// EIO undiagnosable.
///
/// Sink selection:
/// - `JOURNAL_STREAM` set (we are a direct child of systemd, which captures
///   our stderr into the journal): log to **stderr**.
/// - otherwise (we are a child of the JVM `unidrive mount`, whose pipe
///   buffers/swallows our output): log to a **file** under the XDG state dir
///   so the output survives the parent's pipe.
///
/// Idempotent: a second call is a no-op (the global default is already set).
/// Never panics — logging must not be able to take the mount down.
pub fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    if std::env::var_os("JOURNAL_STREAM").is_some() {
        // Under systemd: stderr is captured into the journal.
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .try_init();
        return;
    }

    // Child of the JVM: write to a file in the XDG state dir. `rolling::never`
    // is a synchronous MakeWriter that opens the file in append mode and
    // flushes per write, so no WorkerGuard lifetime to manage.
    let dir = state_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        // Last resort: if we can't create the state dir, fall back to stderr
        // so we don't silently lose all logs.
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .try_init();
        return;
    }

    let appender = tracing_appender::rolling::never(&dir, "unidrive-mount.log");
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(appender)
        .try_init();
}

/// Resolve the directory the log file lives in: `$XDG_STATE_HOME/unidrive`,
/// falling back to `~/.local/state/unidrive`.
fn state_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("unidrive");
        }
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".local/state/unidrive")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_logging_is_idempotent_and_does_not_panic() {
        // First call wins; subsequent calls are no-ops via try_init. Neither
        // may panic.
        init_logging();
        init_logging();
    }

    #[test]
    fn state_dir_prefers_xdg_state_home() {
        // SAFETY: single-threaded test; we set+read the var in one shot.
        unsafe { std::env::set_var("XDG_STATE_HOME", "/var/tmp/xdgtest") };
        assert_eq!(state_dir(), PathBuf::from("/var/tmp/xdgtest/unidrive"));
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
    }
}

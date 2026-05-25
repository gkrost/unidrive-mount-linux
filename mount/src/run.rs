use crate::cache_scanner::scan_and_replay;
use crate::cli::{parse_args, CliError};
use crate::fuse_fs::UnidriveFs;
use crate::ipc::IpcClient;
use crate::kernel_floor::check_kernel_floor;
use crate::profile_lock::ProfileLock;
use fuse3::raw::Session;
use fuse3::MountOptions;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex;

/// Exit codes mirror sysexits(3).
const EX_OK: u8 = 0;
const EX_USAGE: u8 = 64;
const EX_CONFIG: u8 = 78;
const EX_GENERIC_FAILURE: u8 = 1;

/// Entry point used by `main()`. Reads `std::env::args`, then defers to
/// [`run_with_argv`].
pub fn run_main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    run_with_argv(&argv)
}

/// Run with a caller-supplied argv. Pure-function shape so tests can drive it.
pub fn run_with_argv(argv: &[String]) -> ExitCode {
    let cli = match parse_args(argv) {
        Ok(c) => c,
        Err(CliError::Help(msg)) => {
            print!("{msg}");
            return ExitCode::from(EX_OK);
        }
        Err(CliError::Usage(msg)) => {
            eprint!("{msg}");
            return ExitCode::from(EX_USAGE);
        }
    };

    if let Err(e) = check_kernel_floor(None) {
        eprintln!("{e}");
        return ExitCode::from(EX_CONFIG);
    }

    if effective_uid_is_root() {
        eprintln!("refusing to run as root; unidrive-mount is per-user. Re-run as a normal user.");
        return ExitCode::from(EX_GENERIC_FAILURE);
    }

    // Build a single-threaded tokio runtime explicitly — we don't need work
    // stealing for one FUSE session + one IPC connection.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to build tokio runtime: {e}");
            return ExitCode::from(EX_GENERIC_FAILURE);
        }
    };

    rt.block_on(async move {
        match run_async(&cli.mount, &cli.ipc, &cli.cache, cli.lock.as_deref()).await {
            Ok(()) => ExitCode::from(EX_OK),
            Err(e) => {
                eprintln!("{e}");
                ExitCode::from(EX_GENERIC_FAILURE)
            }
        }
    })
}

/// Real wiring: connect IPC, run LocalCache crash-recovery scan, mount FUSE,
/// block on FUSE event loop with SIGTERM/SIGINT-driven shutdown.
///
/// Load-bearing per spec §Phase 2 crash-semantics: scan-and-replay MUST run
/// BEFORE the FUSE mount goes live so the JVM sees deferred uploads before
/// user-space sees the mount.
async fn run_async(
    mount_path: &Path,
    ipc_path: &Path,
    cache_root: &Path,
    lock_path: Option<&Path>,
) -> Result<(), String> {
    // NOTE on mount-already-exists check: fuse3's `mount_with_unprivileged`
    // calls `mount_empty_check` internally which rejects a non-empty mount
    // point. A pre-mounted FUSE filesystem on the same path manifests as
    // either EBUSY from the kernel or AlreadyExists from that check. Filed
    // as Low-tier BACKLOG entry "Add mount-already-exists pre-flight check
    // with friendlier error" rather than re-implement here.

    let mut ipc = IpcClient::connect(ipc_path)
        .await
        .map_err(|e| format!("failed to connect IPC at {}: {e}", ipc_path.display()))?;

    // Per spec §4 R4 option (b): acquire the co-daemon-side flock on the
    // per-profile .lock file AFTER the IPC connect (so we know the JVM is
    // up) but BEFORE the FUSE mount goes live. Holding the lock across
    // MountHandle::unmount closes the kill -9 race: if the JVM parent dies,
    // the kernel keeps the co-daemon's lock alive until /this/ process
    // exits, preventing a fresh `sync --watch` from racing into ~/Onedrive
    // while the FUSE mount is still serving.
    let _profile_lock = match lock_path {
        Some(p) => Some(
            ProfileLock::acquire(p).map_err(|e| e.to_string())?,
        ),
        None => None,
    };

    // Crash-recovery: replay any open_write the previous mount missed.
    // Errors are logged inside the scanner; the only thing that can fail
    // at this layer is a walk-level io error on a directory we expected to
    // exist — surface that, since it indicates the cache root passed in is
    // unusable.
    match scan_and_replay(&mut ipc, cache_root).await {
        Ok(n) if n > 0 => {
            tracing::info!(replayed = n, cache_root=%cache_root.display(), "cache_scanner: replayed deferred open_write");
        }
        Ok(_) => {}
        Err(e) => {
            return Err(format!("cache_scanner failed at {}: {e}", cache_root.display()));
        }
    }

    let fs = UnidriveFs::new(Arc::new(Mutex::new(ipc)))
        .with_cache_root(cache_root.to_path_buf());

    let mut mount_options = MountOptions::default();
    mount_options.fs_name("unidrive").nonempty(false);

    let mut mount_handle = Session::new(mount_options)
        .mount_with_unprivileged(fs, mount_path)
        .await
        .map_err(|e| format!("failed to mount FUSE at {}: {e}", mount_path.display()))?;

    // Wait on SIGTERM / SIGINT / FUSE-session-finished. The MountHandle is
    // itself a Future that resolves when the session ends.
    let mut sigterm = signal(SignalKind::terminate())
        .map_err(|e| format!("failed to install SIGTERM handler: {e}"))?;
    let mut sigint = signal(SignalKind::interrupt())
        .map_err(|e| format!("failed to install SIGINT handler: {e}"))?;

    let stop_reason: StopReason = tokio::select! {
        res = &mut mount_handle => StopReason::SessionEnded(res),
        _ = sigterm.recv() => StopReason::Signal,
        _ = sigint.recv() => StopReason::Signal,
    };

    match stop_reason {
        StopReason::SessionEnded(res) => {
            res.map_err(|e| format!("fuse session ended with error: {e}"))
        }
        StopReason::Signal => mount_handle
            .unmount()
            .await
            .map_err(|e| format!("unmount failed: {e}")),
    }
}

enum StopReason {
    SessionEnded(std::io::Result<()>),
    Signal,
}

fn effective_uid_is_root() -> bool {
    // SAFETY: `geteuid` is always safe; libc binding is just an FFI shim.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_error_exits_with_64() {
        let argv = vec!["unidrive-mount".to_string()];
        let code = run_with_argv(&argv);
        // ExitCode doesn't expose its raw value publicly; round-trip via format.
        // We rely on the documented mapping from parse_args -> EX_USAGE.
        assert_eq!(format!("{:?}", code), format!("{:?}", ExitCode::from(EX_USAGE)));
    }

    #[test]
    fn help_arg_exits_with_zero() {
        let argv = vec!["unidrive-mount".to_string(), "--help".to_string()];
        let code = run_with_argv(&argv);
        assert_eq!(format!("{:?}", code), format!("{:?}", ExitCode::from(EX_OK)));
    }
}

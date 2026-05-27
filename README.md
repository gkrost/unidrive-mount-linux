# unidrive-mount

Linux FUSE co-daemon for the `unidrive` ecosystem. Handles VFS operations natively via `fuse3`/`tokio` and communicates with the JVM sync daemon over a Unix domain socket (UDS).

## Architecture

1. **Metadata Cache** — `CachedAttr` populated during `readdir`/`lookup` to avoid IPC on every `getattr`.
2. **Inode Mapping** — Bidirectional `PathMap` assigns monotonic `u64` inodes (root = `1`). Never recycled during a session.
3. **IPC (NDJSON over UDS)** — VFS syscalls map to NDJSON verbs; defined in `HydrationIpcHandler.kt` (`../unidrive/core/app/hydration/.../HydrationIpcHandler.kt`): `mkdir` → `hydration.mkdir`, `unlink` → `hydration.unlink`, `rmdir` → `hydration.rmdir`.
4. **Reconnection** — `ReconnectingIpcClient` retries every 5s (60s budget). `hydration.subscribe` is unwrapped (reconnect risks lost events).
5. **Crash Recovery** — Pre-mount scanner walks `$XDG_CACHE_HOME/unidrive/hydration`, queries `hydration.last_synced`, replays dirty writes with `recovery-<n>` IDs.
6. **Advisory Locking** — `--lock` uses `flock(2)` (`LOCK_EX | LOCK_NB`) to close `kill -9` race with JVM ProcessLock.
7. **Kernel Floor** — **Linux ≥ 6.9** (exit `EX_CONFIG`). Reserves `FUSE_PASSTHROUGH`; read path currently uses userspace `pread`. No fallback.

## Build

```bash
uname -r                     # >= 6.9
cargo build --release        # target/release/unidrive-mount
```

## CLI Usage

```
Usage: unidrive-mount --mount <path> --ipc <socket> [--cache <path>] [--lock <path>]

Options:
  --mount <path>   Mount point (existing empty directory).
  --ipc <socket>   UDS path to the unidrive JVM IpcServer.
  --cache <path>   Cache root for crash-recovery scan. Defaults to
                   $XDG_CACHE_HOME/unidrive/hydration.
  --lock <path>    Per-profile lock file for flock(2) advisory locking.
  --help           Show this message and exit.
```

```bash
unidrive-mount --mount /home/user/Cloud --ipc /run/user/1000/unidrive.sock --lock /home/user/.config/unidrive/profile.lock
```

Exit codes: `sysexits(3)` (`EX_USAGE = 64`, `EX_CONFIG = 78`). No systemd dependency. XDG-compliant paths.

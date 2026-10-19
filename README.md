# unidrive-mount

Linux FUSE co-daemon for the `unidrive` ecosystem. Handles VFS operations natively via `fuse3`/`tokio` and communicates with the JVM sync daemon over a Unix domain socket (UDS).

## Architecture

1. **Metadata Cache** — `CachedAttr` populated during `readdir`/`lookup` to avoid IPC on every `getattr`.
2. **Inode Mapping** — Bidirectional `PathMap` assigns monotonic `u64` inodes (root = `1`). Never recycled during a session.
3. **IPC (NDJSON over UDS)** — VFS syscalls map to NDJSON verbs defined in `HydrationIpcHandler.kt` (`../unidrive/core/app/hydration/.../HydrationIpcHandler.kt`). Read/open path: `hydration.open_read`, `hydration.open_write`, `hydration.open_write_begin`, `hydration.close_handle`, `hydration.hydrate`, `hydration.dehydrate`, `hydration.subscribe`, `hydration.last_synced`, `hydration.list`. Namespace ops: `hydration.mkdir`, `hydration.unlink`, `hydration.rmdir`, `hydration.create`, `hydration.rename`. FUSE `setattr` (chmod/utimes accepted; truncate routes through `open_write_begin`), `statfs`, and xattr stubs (`getxattr`→ENODATA, `setxattr`→EOPNOTSUPP) round out the surface.
4. **Authentication (IPC protocol 2)** — Every connection, including each reconnect, runs the `hello` / `hello.proof` HMAC-SHA256 handshake (engine spec `docs/dev/specs/ipc-authentication.md`) before its first verb. The token file is re-read on every connect. Without a readable token only a protocol-1 daemon is used (one warning); a refused handshake (`auth_failed`) or a daemon proof mismatch is terminal, never retried.
5. **Reconnection** — `ReconnectingIpcClient` retries every 5s (60s budget). Read verbs are sent again after a reconnect, `open_write` once more (an upload of the same bytes is harmless), the other mutating verbs never (the outcome of the first send is unknown). `hydration.subscribe` is unwrapped (reconnect risks lost events).
   The engine's error tokens reach userland as `not_found`/`unknown_path` → ENOENT, `read_only` → EROFS, `outside_scope` → EACCES, `invalid_path` → EINVAL, `paused` → EAGAIN, `busy` → EBUSY (and the namespace tokens of `HydrationIpcHandler.kt`); any other refusal is EIO.
6. **Crash Recovery** — Pre-mount scanner walks `$XDG_CACHE_HOME/unidrive/hydration`, queries `hydration.last_synced`, replays dirty writes with `recovery-<n>` IDs.
7. **Advisory Locking** — `--lock` uses `flock(2)` (`LOCK_EX | LOCK_NB`) to close `kill -9` race with JVM ProcessLock.
8. **Kernel Floor** — **Linux ≥ 6.9** (exit `EX_CONFIG`). Reserves `FUSE_PASSTHROUGH`; read path currently uses userspace `pread`. No fallback.

## Build

```bash
uname -r                     # >= 6.9
cargo build --release        # target/release/unidrive-mount
```

## CLI Usage

```
Usage: unidrive-mount --mount <path> --ipc <socket> [--cache <path>] [--lock <path>]
                      [--ipc-token-file <path> --profile <name>]
       unidrive-mount --version

Options:
  --mount <path>   Mount point (existing empty directory).
  --ipc <socket>   UDS path to the unidrive JVM IpcServer.
  --cache <path>   Cache root for crash-recovery scan. Defaults to
                   $XDG_CACHE_HOME/unidrive/hydration.
  --lock <path>    Per-profile lock file for flock(2) advisory locking.
  --ipc-token-file <path>
                   The profile's ipc.token (scope full); re-read on every
                   connect for the protocol-2 handshake. Requires --profile.
  --profile <name> Profile name exactly as in config.toml.
  --version        Print "unidrive-mount <version> ipc-protocol <n>" and exit.
  --help           Show this message and exit.
```

```bash
unidrive-mount --mount /home/user/Cloud --ipc /run/user/1000/unidrive.sock --lock /home/user/.config/unidrive/profile.lock
```

Exit codes: `sysexits(3)` (`EX_USAGE = 64`, `EX_CONFIG = 78`). No systemd dependency. XDG-compliant paths.

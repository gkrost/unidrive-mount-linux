# unidrive-mount

An uncompromising, high-performance Linux FUSE co-daemon for the `unidrive` ecosystem. Built in 100% pure, asynchronous Rust via `fuse3` and `tokio`, `unidrive-mount` bridges your local VFS directly to the unidrive JVM core engine without proprietary blobs, hidden trackers, or corporate telemetry.

This tool is designed for Linux power users who demand transparency, absolute control over their file systems, and a strict separation of concerns.

## Technical Architecture & How It Works

Instead of packing massive syncing logic and network stacks into a single opaque binary, `unidrive-mount` acts as a lightweight, specialized FUSE daemon. It handles VFS operations natively on Linux and communicates with the main JVM synchronization service over a fast, transparent local Unix Domain Socket (UDS).

### 1. Zero-Overhead Metadata Cache
To comfortably handle enterprise scales (e.g., 195k+ files) without triggering bottlenecking IPC round-trips on every trivial `getattr` or `stat` call, the daemon populates a thread-safe, in-memory inode attribute cache (`CachedAttr`) during `readdir` (bulk) and `lookup` (single) operations.

### 2. Transparent Inode Mapping
FUSE relies on static `u64` numerical inodes, where inode `1` is strictly reserved for the filesystem root. `unidrive-mount` maintains an internal, bidirectional `PathMap` that monotonically maps remote paths to unique inodes. 
* To ensure filesystem stability and prevent ghost-content bugs caused by kernel-cached dentries, **inode numbers are never recycled**—even if a directory tree or file is forgotten/unlinked during a session.

### 3. IPC Wire Protocol (NDJSON over UDS)
Communication with the core engine relies entirely on an open, auditable Newline-Delimited JSON (NDJSON) framing protocol. Every VFS system call maps to an explicit JSON request payload passed over the local socket. For example:
* `mkdir` maps to `{"verb": "hydration.mkdir", "path": "/..."}`
* `unlink` maps to `{"verb": "hydration.unlink", "path": "/..."}`
* `rmdir` maps to `{"verb": "hydration.rmdir", "path": "/..."}`

### 4. Robust Reconnection & Fault Tolerance
Distributed network states and background background tasks are inherently prone to drops. The mount engine is built defensively to survive co-daemon crashes:
* **Automatic Healing:** Filesystem operations wrap around a `ReconnectingIpcClient` that handles transient I/O disconnects, retrying connections every 5 seconds within a strict 60-second budget window before forcing an error back to the user.
* **Why `hydration.subscribe` is Different:** The standard asynchronous event stream is intentionally kept *unwrapped* from auto-reconnection. Silent reconnects on long-lived event listeners risk losing critical change events that occur during the dark window. Instead, it forces a failure so consumer logic can explicitly handle state reconciliation.

### 5. Crash Recovery State Verification
If the JVM daemon crashes mid-flight while a local file handle is holding unwritten modifications, data loss could occur. To prevent this, `unidrive-mount` runs a **Cache Scanner** loop *at startup, before the FUSE mount goes live*.
1. It walks the local cache directory (`$XDG_CACHE_HOME/unidrive/hydration`).
2. It queries `hydration.last_synced(remote_path)` from the core engine.
3. If a file's local modification time (`cache_mtime`) is greater than the remote sync watermark, it safely replays the missed write operation using an explicit recovery ID (`recovery-<n>`). This ensures the engine's internal audit log can cleanly differentiate crash-recovery replays from active interactive user writes.

### 6. Strict Advisory Locking
To eradicate race conditions and dual-mount corruption, the tool relies on a dual-lock system. When utilizing the optional `--lock` path, the daemon acquires a strict BSD advisory lock via `flock(2)` (`LOCK_EX | LOCK_NB`) on a per-profile lockfile. This eliminates any possible `kill -9` race window with the JVM-side process supervisor.

### 7. Modern Kernel Floor Requirements
This daemon is built for modern Linux systems and commits to a modern kernel floor:
* **Linux Kernel >= 6.9 is strictly enforced.** The daemon verifies `/proc/sys/kernel/osrelease` on launch and refuses to start (exit `EX_CONFIG`) on anything older. The floor is the design commitment that reserves `FUSE_PASSTHROUGH` — direct kernel-to-cache I/O routing that bypasses userspace copying — for when the underlying `fuse3` crate exposes it. Until then the read path serves hydrated files via a userspace `pread` on the cache file descriptor; the kernel floor is enforced regardless, with no fallback branch for older kernels.

---

## Installation & Build

Build directly from source using the standard Rust toolchain (edition 2024 requires Rust 1.85 or newer). No third-party pre-compiled dependencies required.

```bash
# Ensure you are on Linux Kernel 6.9+
uname -r

# Build the release profile
cargo build --release

```

The resulting binary will be available at `target/release/unidrive-mount`.

---

## Command Line Usage

The binary adheres strictly to standard UNIX CLI conventions, sending human-readable configuration/usage errors to `stderr` and returning standard exit codes mapping to `sysexits(3)` (e.g., `EX_USAGE = 64`, `EX_CONFIG = 78`).

```
Usage: unidrive-mount --mount <path> --ipc <socket> [--cache <path>] [--lock <path>]

Options:
  --mount <path>   Filesystem mount point (an existing empty directory).
  --ipc <socket>   Unix-domain-socket path to the unidrive JVM IpcServer.
  --cache <path>   LocalCache root for crash-recovery scan at startup.
                   Defaults to $XDG_CACHE_HOME/unidrive/hydration, or
                   $HOME/.cache/unidrive/hydration if XDG_CACHE_HOME unset.
  --lock <path>    Per-profile lock file. When supplied, the co-daemon
                   acquires its own flock(2) on this path for the session,
                   closing the kill -9 race with the JVM-side ProcessLock.
  --help           Show this message and exit.

```

### Example

```bash
# Run the daemon safely isolated to your user profile
unidrive-mount \
  --mount /home/user/Cloud \
  --ipc /run/user/1000/unidrive.sock \
  --lock /home/user/.config/unidrive/profile.lock

```

---

## Clean, Uncompromising Codebase

* **Zero unsafe rust blocks** in core logic maps (safe FFI shims only where libc interaction is strictly necessary).
* **No systemd hard-dependencies** required to run—launches cleanly in any environment, script, or custom namespace.
* **XDG Compliant:** Clean fallback paths respecting `$XDG_CACHE_HOME` directly out of the box.

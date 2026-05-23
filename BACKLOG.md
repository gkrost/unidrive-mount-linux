# Backlog

Top of file = next up. Work down. Move done items to `CLOSED.md` in the same commit. No IDs, no dates, no versions. See [AGENTS.md](AGENTS.md) for the discipline.

## Critical — data-risk

| Title | Scope |
|---|---|

## High — correctness, required for first release

| Title | Scope |
|---|---|
| FUSE write-path: write, fsync, dirty-release-triggers-upload; LocalCache crash-recovery scanner at startup | Extend `fuse_fs` with `write`, `fsync`, write-side `release` (issues `hydration.open_write` with the cache path on dirty FDs). Add `cache_scanner.rs` that walks `~/.cache/unidrive/hydration/` at startup, calls `hydration.last_synced` per file, and replays `hydration.open_write` for any cache file newer than the watermark — runs BEFORE the FUSE mount goes live so the JVM sees deferred uploads before user-space sees the mount. IPC reconnect resilience (5 s retry up to 60 s) lands here; `subscribe` MUST NOT be wrapped in the retry layer. Tests: `write_through.rs`, `dehydrate_while_open.rs` (EBUSY propagation), `crash_recovery_replay.rs`, `ipc_reconnect.rs`. Scope per `../unidrive/docs/dev/plans/sparse-hydration-roadmap-phase-2.md` Task 3. |
| Phase 2 JVM wiring: `unidrive mount` CLI subcommand + dist/install.sh co-daemon placeholder | Lands in the SIBLING unidrive repo (`../unidrive/`), NOT this repo — listed here for cross-repo visibility. Adds `MountCommand.kt` (Picocli `unidrive mount <path> [--profile <name>]`), reuses `IpcServer.defaultSocketPath(profileName)` so the spawned co-daemon binary stays in lockstep with the JVM's socket-path resolution (including the 90-char `MAX_SOCKET_PATH_LENGTH` hashed-name fallback and `.meta` sidecar). Modifies `dist/install.sh` with a placeholder co-daemon-download section. Scope per `../unidrive/docs/dev/plans/sparse-hydration-roadmap-phase-2.md` Task 4. |

## Medium — efficiency

| Title | Scope |
|---|---|
| fuse3 crate version pin needs upgrade for FUSE_PASSTHROUGH support | `fuse3 = "0.9"` does not expose `FUSE_PASSTHROUGH`/`backing_id`/`setup_passthrough` (grepped the entire crate source — no matches). The Phase-2 read-path therefore falls back to userspace cache reads on hydrated files: `open` still does an IPC `hydration.open_read` to fetch the cache path, but subsequent `read(2)` calls go through the FUSE userspace `read` handler doing `pread` on the cache FD rather than the kernel-direct passthrough path. Correctness unaffected; cost is one extra IPC round-trip per `open` on hydrated files plus userspace round-trips on every read. Re-evaluate when fuse3 ships passthrough (track upstream `Sherlock-Holo/fuse3` for the API), or vendor the kernel ioctl directly via `libc::ioctl(/dev/fuse, FUSE_DEV_IOC_BACKING_OPEN, ...)`. `mount/Cargo.toml`, `mount/src/fuse_fs.rs`. |
| Move `tempfile` from `[dependencies]` to `[dev-dependencies]` | Task 1 (foundation) placed `tempfile` under `[dependencies]` because `fake_jvm.rs` lives in `mount/src/` (gated on `cfg(any(test, debug_assertions))`) and integration tests import it via the library crate. Cargo resolves all `[dependencies]` at release time regardless of cfg gates — so `tempfile` and its transitive deps compile on every release build. Cleaner: move `fake_jvm.rs` into `mount/tests/support/mod.rs` (shared integration-test helper module), declare `tempfile` in `[dev-dependencies]`. Same test coverage, smaller release closure. Today's release binary is 446 KB — not user-visible, but the principle is worth fixing once. `mount/Cargo.toml`, `mount/src/fake_jvm.rs` → `mount/tests/support/mod.rs`. |

## Low — guards and UX

| Title | Scope |
|---|---|

## Cross-cutting

| Title | Scope |
|---|---|

## Design constraints (not tickets — bind when related work lands)

- **Phase 2 + Phase 3 do not live inside `../unidrive/core/`.** Anchor: `../unidrive/AGENTS.md` *What not to do* — "Don't grow the daemon to host a UI tier." Trigger: any new Phase 2 / Phase 3 work. The FUSE binary is its own process; the Dolphin extension is its own crate inside this repo.

## Deferred

| Title | Scope |
|---|---|

## Out of scope across all surfaces

- Windows or macOS support in this binary. The Windows Cloud Files API placeholder surface lives in a separate platform tier per `../unidrive/docs/adr/multi-platform.md`.
- Auth, sync, provider logic, OAuth flows. Those belong in the JVM daemon.
- KIO slave implementation. Skipped at first; promote from optional only if Dolphin ServiceMenus prove insufficient.
- Graceful degrade to kernels < 6.9. The kernel-floor commitment is the design.
- Auto-restart of a crashed co-daemon. Explicit user re-mount only.
- Two concurrent `unidrive mount` invocations on the same path. Relies on kernel-level rejection of the second `fusermount3`.
- Running as root. Refused with an explicit error; mount is per-user.

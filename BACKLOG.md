# Backlog

Top of file = next up. Work down. Move done items to `CLOSED.md` in the same commit. No IDs, no dates, no versions. See [AGENTS.md](AGENTS.md) for the discipline.

## Critical — data-risk

| Title | Scope |
|---|---|

## High — correctness, required for first release

| Title | Scope |
|---|---|
| FUSE `mkdir` not implemented — returns ENOSYS | `mkdir /tmp/onedrive-smoke/test_gernot` returns `Function not implemented` because `mount/src/fuse_fs.rs` has no `async fn mkdir` impl. The grep-confirmed FUSE op list today is `init`, `destroy`, `lookup`, `getattr`, `opendir`, `readdir`, `readdirplus`, `open`, `read`, `write`, `fsync`, `release` — namespace mutation (mkdir / rmdir / unlink / create / rename / setattr) is entirely absent. Phase 2's read-mostly design is the reason, and there's no existing BACKLOG entry making that gap explicit. **Required for first release** because operators trying to use the mount as a working filesystem will hit ENOSYS on the first `mkdir` and reasonably conclude the tool is broken. Fix shape: implement `mkdir(parent_inode, name, mode)` as an IPC round-trip — new hydration verb `hydration.mkdir(path)` that synthesises a `state.db` row + creates the remote folder via the provider. The unidrive JVM side already has `OneDriveProvider.createFolder()` and `InternxtProvider.createFolder()` — the missing piece is the hydration SPI verb wiring. Live evidence: 2026-05-24 smoke against `/tmp/onedrive-smoke` returned `mkdir: Function not implemented` for `mkdir test_gernot`. `mount/src/fuse_fs.rs` (add mkdir impl), `mount/src/ipc.rs` (add hydration.mkdir client method), `../unidrive/core/app/hydration/src/main/kotlin/org/krost/unidrive/hydration/HydrationIpcHandler.kt` (add verb), `../unidrive/core/app/hydration/src/main/kotlin/org/krost/unidrive/hydration/HydrationImpl.kt` (delegate to provider createFolder + state.db insert). |
| FUSE `unlink` / `rmdir` not implemented — returns ENOSYS | `rm /tmp/onedrive-smoke/foo.txt` returns `Function not implemented` for the same reason as the mkdir entry above: `mount/src/fuse_fs.rs` has no `async fn unlink` or `async fn rmdir`. Live evidence: 2026-05-24 smoke against `/tmp/onedrive-smoke/260410_anlagen/` returned five `rm: cannot remove '<file>.pdf': Function not implemented` on `rm *`. Fix shape: implement `unlink(parent_inode, name)` and `rmdir(parent_inode, name)` as IPC verbs `hydration.unlink(path)` and `hydration.rmdir(path)`. Provider-side capability exists (`OneDriveProvider.deleteRemote()`, `InternxtProvider.deleteRemote()`); same wiring shape as the mkdir entry. **Caveat for the implementer**: unlink/rmdir through the mount surface a coordination question with the legacy `SyncEngine` symmetric to the mount-write-clobber entry filed in `../unidrive/BACKLOG.md` — a delete through the mount will be re-created on the next sync cycle if the same path still exists in `~/Onedrive/`. Resolve that BACKLOG entry first (or in parallel) so the mount's delete actually sticks. |

## Medium — efficiency

| Title | Scope |
|---|---|
| fuse3 crate version pin needs upgrade for FUSE_PASSTHROUGH support | `fuse3 = "0.9"` does not expose `FUSE_PASSTHROUGH`/`backing_id`/`setup_passthrough` (grepped the entire crate source — no matches). The Phase-2 read-path therefore falls back to userspace cache reads on hydrated files: `open` still does an IPC `hydration.open_read` to fetch the cache path, but subsequent `read(2)` calls go through the FUSE userspace `read` handler doing `pread` on the cache FD rather than the kernel-direct passthrough path. Correctness unaffected; cost is one extra IPC round-trip per `open` on hydrated files plus userspace round-trips on every read. Re-evaluate when fuse3 ships passthrough (track upstream `Sherlock-Holo/fuse3` for the API), or vendor the kernel ioctl directly via `libc::ioctl(/dev/fuse, FUSE_DEV_IOC_BACKING_OPEN, ...)`. `mount/Cargo.toml`, `mount/src/fuse_fs.rs`. |
| Move `tempfile` from `[dependencies]` to `[dev-dependencies]` | Task 1 (foundation) placed `tempfile` under `[dependencies]` because `fake_jvm.rs` lives in `mount/src/` (gated on `cfg(any(test, debug_assertions))`) and integration tests import it via the library crate. Cargo resolves all `[dependencies]` at release time regardless of cfg gates — so `tempfile` and its transitive deps compile on every release build. Cleaner: move `fake_jvm.rs` into `mount/tests/support/mod.rs` (shared integration-test helper module), declare `tempfile` in `[dev-dependencies]`. Same test coverage, smaller release closure. Today's release binary is 446 KB — not user-visible, but the principle is worth fixing once. `mount/Cargo.toml`, `mount/src/fake_jvm.rs` → `mount/tests/support/mod.rs`. |

## Low — guards and UX

| Title | Scope |
|---|---|
| Co-daemon-side `flock(2)` of profile lock file (mode-mutex R4 option (b)) | Follow-up filed at close-out per `../unidrive/docs/dev/specs/mount-sync-mode-mutex-design.md` §4 R4. The Phase 1 mutex relies on the JVM-side `ProcessLock`'s kernel `FileLock` to refuse mount when sync holds it (and vice versa). The spec's §4 R4 analysis: "If the JVM `MountCommand` is killed (e.g. `kill -9 <pid>`) while the co-daemon is alive, the kernel releases the file lock (JVM exit) but the co-daemon keeps running, still attached to the FUSE mount via `/dev/fuse`. The next `unidrive -p X sync --watch` would then acquire the lock and start syncing into `~/Onedrive/` while the mount is still serving from `/tmp/...`." The spec defers option (b) — Rust co-daemon `flock(2)` of the same `.lock` file — to keep Phase 1 JVM-only; the residual is mitigated by the §3.4.1 zombie-mount sanity check in `SyncCommand.run()` (warns but doesn't abort) and by `fusermount3` itself refusing a re-mount of a stale endpoint. Scope: co-daemon acquires its own advisory `flock(2)` (LOCK_SH or LOCK_EX as design dictates) on `~/.config/unidrive/<profile>/.lock` for the duration of the FUSE session, releasing on co-daemon exit; kernel-level release is then observed by both JVM-side and co-daemon-side acquisitions, closing the `kill -9 <jvm-pid>` window. Acceptance: an integration test that spawns a mount, sends `SIGKILL` to the JVM parent while the Rust co-daemon is still serving, then asserts a `sync --watch` attempt against the same profile refuses with the mount-holder error. Priority Low — only triggers on `kill -9 <jvm-pid>`; clean shutdown via `kill <pid>` or Ctrl-C correctly tears down both sides through the existing supervise + signal-forwarding path. |

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

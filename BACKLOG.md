# Backlog

Top of file = next up. Work down. Actionable items now live as **GitHub Issues** on `gkrost/unidrive-mount-linux` — this file is a section-grouped index of pointers plus the non-ticket prose (Design constraints / Deferred / Out of scope). Open an issue per the [AGENTS.md](AGENTS.md) discipline; mirror its title here as `- [#N] <title>` under the matching section. `CLOSED.md` remains the archive of landed work. No IDs, no dates, no versions in commit messages.

## Critical — data-risk

- [#27] OneDrive mount hangs before `mount()` — co-daemon connects to IPC then never establishes the FUSE filesystem (profile entirely unmountable)

## High — correctness, required for first release

- [#28] `ReconnectingIpcClient` blind-replays non-idempotent verbs on `IpcError::Io` → spurious errors after a mid-op disconnect

## Medium — efficiency

- [#29] `lookup` does a byte-exact path match with no Unicode (NFC) normalization → an NFC/NFD-mismatched name resolves to ENOENT
- [#30] Co-daemon issues `hydration.subscribe` on mount (clean mount-detection signal + Phase-3 view-invalidation prereq)
- [#31] fuse3 crate version pin needs upgrade for FUSE_PASSTHROUGH support
- ~~[#32] Move `tempfile` from `[dependencies]` to `[dev-dependencies]`~~ → CLOSED
- [#33] File-manager preview/thumbnail generation triggers bulk hydration of cloud-only files

## Low — guards and UX

- [#34] Cache-file eviction on `unlink`/`rmdir` of hydrated paths — **closed: fixed in #6** (see `CLOSED.md`)
- [#35] `mkdir` parent-missing maps to EIO instead of POSIX ENOENT — **closed: fixed in #5** (see `CLOSED.md`)
- ~~[#36] Extract `replies()` test helper to `fake_jvm.rs`~~ → CLOSED
- [#37] Collapse the now-10 `ReconnectingIpcClient` io-retry wrappers via a macro
- ~~[#38] Path-construction idiom drift between `lookup` and namespace ops~~ → CLOSED
- [#39] Directories always report `nlink=2` regardless of subdirectory count (leaf-directory optimization gap)
- [#40] No way to see hydration state (local vs cloud-only) from within the mount
- [#41] Optimise truncate-to-`N`>0 of a cloud-only file (prefix/Range download, not whole-file)
- [#42] Mount co-daemon vanished mid-session (intermittent, unexplained)
- [#43] IPC teardown surfaces as bare `EIO` with no co-daemon breadcrumb (`round_trip` `UnexpectedEof`)

## Cross-cutting

_(none open)_

## Design constraints (not tickets — bind when related work lands)

- **Phase 2 + Phase 3 do not live inside `../unidrive/core/`.** Anchor: `../unidrive/AGENTS.md` *What not to do* — "Don't grow the daemon to host a UI tier." Trigger: any new Phase 2 / Phase 3 work. The FUSE binary is its own process; the Dolphin extension is its own crate inside this repo.

## Deferred

_(none)_

## Out of scope across all surfaces

- Windows or macOS support in this binary. The Windows Cloud Files API placeholder surface lives in a separate platform tier per `../unidrive/docs/adr/multi-platform.md`.
- Auth, sync, provider logic, OAuth flows. Those belong in the JVM daemon.
- KIO slave implementation. Skipped at first; promote from optional only if Dolphin ServiceMenus prove insufficient.
- Graceful degrade to kernels < 6.9. The kernel-floor commitment is the design.
- Auto-restart of a crashed co-daemon. Explicit user re-mount only.
- Two concurrent `unidrive mount` invocations on the same path. Relies on kernel-level rejection of the second `fusermount3`.
- Running as root. Refused with an explicit error; mount is per-user.

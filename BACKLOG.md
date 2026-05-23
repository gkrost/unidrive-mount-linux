# Backlog

Top of file = next up. Work down. Move done items to `CLOSED.md` in the same commit. No IDs, no dates, no versions. See [AGENTS.md](AGENTS.md) for the discipline.

## Critical — data-risk

| Title | Scope |
|---|---|

## High — correctness, required for first release

| Title | Scope |
|---|---|
| Cargo workspace scaffold + kernel-floor check + kernel-floor unit test | First Rust commit. Lands the workspace `Cargo.toml`, an `mount` crate skeleton, a `bin/unidrive-mount.rs` entry point that reads `/proc/sys/kernel/osrelease` and refuses to start on kernel < 6.9 with exit code 78 (`EX_CONFIG`) and a one-line stderr citing the required kernel + which feature (`FUSE_PASSTHROUGH`) is missing, and a `tests/kernel_floor.rs` integration test exercising the refusal against a faked-old kernel string. Everything else (FUSE filesystem impl, IPC client, LocalCache reader) comes in follow-up commits. Per spec §Phase 2 components and §Error handling. |

## Medium — efficiency

| Title | Scope |
|---|---|

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

# Closed

Things that were done. Append new entries when items move out of `BACKLOG.md`.

## Initial

- Foundation: Cargo workspace scaffold + kernel-floor check (refuses Linux below 6.9 with exit `EX_CONFIG` citing `FUSE_PASSTHROUGH` as the missing feature) + IPC client over `tokio::net::UnixStream` exercising all eight hydration verbs (`open_read`, `open_write`, `close_handle`, `hydrate`, `dehydrate`, `subscribe`, `last_synced`, `list`) with serde_json round-trips and inherited JVM-side limits enforced client-side (64 KiB outbound cap matching `IpcServer.MAX_REQUEST_BYTES`, 4 MiB inbound cap) + `FakeJvm` test fixture binding a temp UDS and serving canned per-verb replies. Subscribe covered handshake-only (Phase 3 owns the event stream). Distinct `IpcError::Busy` for `dehydrate` busy replies; `IpcError::Unknown { reason }` for `last_synced` failures (dynamic reason, not literal-coupled). Scope per `../unidrive/docs/dev/plans/sparse-hydration-roadmap-phase-2.md` Task 1.

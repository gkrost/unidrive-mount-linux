# Backlog

Top of file = next up. Work down. Move done items to `CLOSED.md` in the same commit. No IDs, no dates, no versions. See [AGENTS.md](AGENTS.md) for the discipline.

## Critical — data-risk

| Title | Scope |
|---|---|

## High — correctness, required for first release

| Title | Scope |
|---|---|
| FUSE `rename` not implemented — editors/IDEs that save via swap-rename fail; `mv` fails | Companion to the `create`/`mknod` gap above, surfaced by the same nano live-smoke. Even with `create` landed, nano's save sequence is `write(.foo.swp); fsync(.foo.swp); rename(.foo.swp, foo)`. Same ENOSYS surface for `mv /mount/a /mount/b`. Many editors (vim, VS Code, IntelliJ) use the same atomic-rename pattern for crash safety. Fix shape: implement `rename(old_parent, old_name, new_parent, new_name)` routing to a new JVM verb `hydration.rename(old_path, new_path)` that calls the provider's rename API (OneDrive PATCH /me/drive/items/{id}/{name}, Internxt move endpoint) and updates the state.db row. Acceptance: nano save round-trips through the FUSE mount; `mv /mount/a /mount/b` succeeds; the rename appears on the cloud. `mount/src/fuse_fs.rs`, sibling JVM verb. |

## Medium — efficiency

| Title | Scope |
|---|---|
| fuse3 crate version pin needs upgrade for FUSE_PASSTHROUGH support | `fuse3 = "0.9"` does not expose `FUSE_PASSTHROUGH`/`backing_id`/`setup_passthrough` (grepped the entire crate source — no matches). The Phase-2 read-path therefore falls back to userspace cache reads on hydrated files: `open` still does an IPC `hydration.open_read` to fetch the cache path, but subsequent `read(2)` calls go through the FUSE userspace `read` handler doing `pread` on the cache FD rather than the kernel-direct passthrough path. Correctness unaffected; cost is one extra IPC round-trip per `open` on hydrated files plus userspace round-trips on every read. Re-evaluate when fuse3 ships passthrough (track upstream `Sherlock-Holo/fuse3` for the API), or vendor the kernel ioctl directly via `libc::ioctl(/dev/fuse, FUSE_DEV_IOC_BACKING_OPEN, ...)`. `mount/Cargo.toml`, `mount/src/fuse_fs.rs`. |
| Move `tempfile` from `[dependencies]` to `[dev-dependencies]` | Task 1 (foundation) placed `tempfile` under `[dependencies]` because `fake_jvm.rs` lives in `mount/src/` (gated on `cfg(any(test, debug_assertions))`) and integration tests import it via the library crate. Cargo resolves all `[dependencies]` at release time regardless of cfg gates — so `tempfile` and its transitive deps compile on every release build. Cleaner: move `fake_jvm.rs` into `mount/tests/support/mod.rs` (shared integration-test helper module), declare `tempfile` in `[dev-dependencies]`. Same test coverage, smaller release closure. Today's release binary is 446 KB — not user-visible, but the principle is worth fixing once. `mount/Cargo.toml`, `mount/src/fake_jvm.rs` → `mount/tests/support/mod.rs`. |

## Low — guards and UX

| Title | Scope |
|---|---|
| Cache-file eviction on `unlink`/`rmdir` of hydrated paths | Spec `../unidrive/docs/dev/specs/hydration-namespace-verbs-design.md` §4 R5 / NG4 explicit deferral. When `unlink`/`rmdir` succeed against a hydrated file, the cache copy at `~/.cache/unidrive/hydration/<provider>/<path>` is orphaned — the cloud copy is gone, the state.db row is tombstoned, but the cache file stays. Disk-space concern, not correctness. The existing cache-eviction logic on the JVM side will reap it eventually, but until then it's wasted bytes. Scope: trivial cleanup-on-unlink in `fuse_fs.rs::unlink`/`rmdir` after IPC success — `std::fs::remove_file(cache_path)` (file) or `std::fs::remove_dir_all(cache_dir)` (folder), ignoring NotFound so a never-hydrated path is harmless. The path-to-cache mapping is `<cache_root>/<provider>/<rel>`; `<cache_root>` is the `--cache` flag, `<provider>` is the profile providerId, `<rel>` is the FUSE path. `mount/src/fuse_fs.rs` (unlink/rmdir bodies). |
| `mkdir` parent-missing maps to EIO instead of POSIX ENOENT | Spec `../unidrive/docs/dev/specs/hydration-namespace-verbs-design.md` §4 R3 explicit deferral. `provider.createFolder("/a/b/c")` fails if `/a/b` doesn't exist (OneDrive 404, Internxt similar); the JVM emits a raw provider message and `namespace_err_to_errno` (`mount/src/fuse_fs.rs:172-179`) falls through to the catch-all `EIO`. POSIX says `mkdir` on a non-existent parent returns `ENOENT`. Userland `mkdir -p` creates intermediates one at a time so the kernel always sees a known parent, but a single-call `mkdir` from a program that walks the path itself hits the divergence. Fix shape per spec: JVM-side adds a distinct `parent_not_found` wire-error in `HydrationIpcHandler`; co-daemon adds a `"parent_not_found" => ENOENT` arm to `namespace_err_to_errno`. Both halves coordinate as a single cross-repo change. `mount/src/fuse_fs.rs:172-179` + sibling-repo `HydrationIpcHandler.kt` `hydration.mkdir` branch. |
| Extract `replies()` test helper to `fake_jvm.rs` | The `fn replies(pairs: &[(&str, &str)]) -> HashMap<String, String> { pairs.iter().map(\|(k, v)\| (k.to_string(), v.to_string())).collect() }` body is duplicated byte-for-byte across at least seven sites: `mount/tests/{namespace_ops,getattr_readdir,crash_recovery_replay,write_through,cold_read,ipc_reconnect,ipc_client}.rs` plus `mount/src/cache_scanner.rs`. Each new integration test adds an eighth. Move to `mount/src/fake_jvm.rs` as `pub fn replies(...)`; delete the per-file copies. Pure DRY cleanup, no behavioural change. |
| Collapse the now-10 `ReconnectingIpcClient` io-retry wrappers via a macro | The comment at `mount/src/reconnect.rs:75-77` justifies "three similar copies beat one premature `dyn Future` indirection." That was true at 3. The count is now 10 (`open_read`, `open_write`, `close_handle`, `hydrate`, `dehydrate`, `last_synced`, `list`, `mkdir`, `unlink`, `rmdir`) — every method is `loop { ensure_connected; match c.<verb>(args).await { Ok(v) => return Ok(v), Err(Io(_)) => { self.inner = None; continue; }, Err(e) => return Err(e) } }`. A `macro_rules! retry_io_loop` would collapse ~120 lines to ~20 with no `dyn Future` (the original concern). The lifetime objection in the comment doesn't apply to macro expansion. Each new verb the JVM adds (Phase 3 events, future namespace ops) currently means another 10-line copy-paste. Acceptance: macro + 10 single-line invocations, update the load-bearing comment to point at the macro definition. `mount/src/reconnect.rs`. |
| Path-construction idiom drift between `lookup` and namespace ops | `lookup` (`fuse_fs.rs:240-244`) uses `if parent_path.is_empty() { format!("/{name}") } else { format!("{parent_path}/{name}") }`. The three namespace ops (mkdir/unlink/rmdir at `:652`, `:693`, `:728`) use `format!("{}/{}", parent_path.trim_end_matches('/'), name)`. Both produce identical strings for the cases the codebase exercises (root: `/foo`; nested: `/folder/foo`). Pure stylistic divergence; future readers will burn cycles wondering whether the two idioms encode different invariants. Acceptance: extract `fn child_path(parent: &str, name: &str) -> String` near `basename` (`:154-160`), call from all four sites; tests unchanged. `mount/src/fuse_fs.rs`. |

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

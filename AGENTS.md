# Agent instructions

unidrive-mount-linux is the Linux FUSE co-daemon for the [unidrive](https://github.com/gkrost/unidrive) sparse-hydration roadmap. It implements Phase 2 (FUSE3 mount with `FUSE_PASSTHROUGH` for hydrated files) and Phase 3 (Dolphin / KDE integration crates) of the design spec at `../unidrive/docs/dev/specs/sparse-hydration-roadmap-design.md`. It consumes the Hydration SPI over Unix-domain-socket JSON-line IPC from the sibling unidrive JVM daemon.

This file is the rulebook for everyone touching the repo — human contributors and LLM agents alike. End-users land on `README.md`; the moment you want to *change* something, you read this file.

## Hard rules

- **Single binary at first** (`unidrive-mount`). The Cargo workspace currently has one member (`mount`). New crates need a named justification tied to the spec; `kio` is the planned Phase-3 crate but is not present yet.
- **Kernel ≥ 6.9 is the hard floor.** The binary refuses to start on too-old kernels with exit code 78 (`EX_CONFIG`) and a one-line stderr citing `FUSE_PASSTHROUGH` as the missing feature. Enforced in `mount/src/kernel_floor.rs`.
- **libfuse ≥ 3.16 is the hard floor (implicit).** Not checked at runtime — the constraint is inherited from the `fuse3 = "0.9"` crate dependency and the `FUSE_PASSTHROUGH` ioctl symbol set. If a future change wants this enforced explicitly, a `fusermount3 --version` probe is the place to add it.
- **MVP-shape note on `FUSE_PASSTHROUGH`.** The kernel-floor check is the binding commitment. The runtime use of `FUSE_PASSTHROUGH` itself is **not implemented at MVP** — neither `fuse3 = "0.9"` nor any other published Rust crate exposes the `FUSE_DEV_IOC_BACKING_OPEN` ioctl path, and the ioctl requires `CAP_SYS_ADMIN` on the FUSE daemon, which the unprivileged-mount design here cannot grant without a security-model change. Hydrated-file reads therefore go through the userspace `read` handler (one extra IPC round-trip per `open`, plus pread on the cache FD per `read(2)`). This is correct, not broken — the user-visible behaviour is identical to passthrough; the cost is extra context switches and IPC traffic. The kernel-floor refusal exists so adding real passthrough later doesn't need a runtime compatibility branch. Re-evaluation triggers, in any order: (a) upstream `Sherlock-Holo/fuse3` lands passthrough APIs; (b) the security model splits into a `CAP_SYS_ADMIN`-bearing privsep helper + an unprivileged main; (c) the design decides a single-binary setcap is acceptable.
- **One IPC contract surface.** The Phase-2 hydration verbs the co-daemon currently consumes are listed under *Cross-repo contract* below. Phase 2 does not invent new verbs unilaterally. If a verb is missing, file an issue on the sibling unidrive repo, not here.
- **No `--respawn` of a crashed co-daemon by default.** The user explicitly chose to mount. No respawn logic exists in `run.rs` / `main.rs`.
- **No mount-already-exists auto-resolution.** Refuse at startup and ask the user to clear with `fusermount3 -u`.
- **No running as root.** Refuse with an explicit error; mount is per-user. `run.rs` checks `geteuid() == 0` and exits 1.
- **`cargo test`** (in a FUSE-enabled environment, e.g. `--cap-add=SYS_ADMIN` in a container) is the gate. No semgrep, gitleaks, codecov, trivy, clippy baselines.
- **No CI policing.** CI lands when there's a release surface to defend. The dev loop is `cargo test`. Exceptions: a `ci` workflow runs `cargo build` and `cargo test` on push/PR; security advisory scanning (`security.yml` running `cargo audit`, plus CodeQL on push/PR/schedule) stays advisory — none of them are required merge checks; `cargo test` remains the gate.
- **No IDs, dates, or version numbers** in commit messages, file names, or document content. Describe what a thing is, not when it was filed or which release ships it.
- **Doc surface is bounded.** Shared docs are this file, `README.md`, `CLOSED.md`. The work queue lives in GitHub issues (`gh issue list -R gkrost/unidrive-mount-linux`). Per-crate `README.md` files are permitted; ADRs under `docs/adr/` if a decision is load-bearing enough to outlive memory.

## Output token management

- **Write long outputs to disk.** For long analysis or ticketing sessions, write outputs (tickets, summaries, audits) to files rather than emitting them inline to chat.
- **Keep chat updates concise.** Offload verbose content (logs, full ticket bodies, large diffs) to disk and reference their paths.

## How to work

1. List open work: `gh issue list -R gkrost/unidrive-mount-linux`. Pick the first item that isn't blocked.
2. Read three nearby source files before writing. The existing patterns are the style guide.
3. **Pre-execution sanity check.** If the work goes beyond the issue itself — scope expansion, deletion of a user-facing feature, new abstraction not already approved, IPC verb invention — surface it and pause for confirmation before executing. Plan approval doesn't cover sideband cuts.
4. Make the change. Run `cargo test`. Iterate.
5. When the change lands, append a one-paragraph entry to `CLOSED.md` and close the issue in the same commit. One commit, one item.
6. If you discover a new piece of work, open a new issue (`gh issue create -R gkrost/unidrive-mount-linux`) rather than carrying it in-session.

## Verification

- **Do not rely on summaries.** Verify load-bearing claims (kernel-floor behaviour, IPC wire format, FUSE_PASSTHROUGH ioctl invocation) with a full pass before reporting.
- **Re-verify everything on red flags.** If review flags a fabricated `fuse3` API call or wrong IPC field name, treat it as a signal to re-verify the *whole artifact*, not just the called-out line.
- **Check sibling-repo state.** The IPC contract lives in `../unidrive/`. Before claiming a verb works against the canonical contract, run `git -C ../unidrive fetch origin && git -C ../unidrive log main..origin/main` to confirm the sibling mirror isn't stale.

## What lives where

- `mount/` — Phase 2 crate. `src/main.rs` entry point, `fuse3` filesystem impl in `src/fuse_fs.rs`, `IpcClient` over UDS in `src/ipc.rs`, `ReconnectingIpcClient` wrapper in `src/reconnect.rs`, `LocalCache` reader against `~/.cache/unidrive/hydration/`, kernel-floor check in `src/kernel_floor.rs`, profile-lock acquisition in `src/profile_lock.rs`, crash-recovery scanner in `src/cache_scanner.rs`.
- `kio/` — Phase 3 crate (planned, not yet present). Dolphin `.desktop` ServiceMenus, D-Bus shim, icon-overlay refresh land here when Phase 3 starts.
- `docs/adr/` — architectural decisions, added on demand.

## Cross-repo contract

The Hydration SPI verbs the co-daemon consumes (JSON-line over UDS):

Phase-2 hydration verbs:

- `hydration.open_read` — open a path for read; triggers hydrate on cache miss; returns `{cache_path, handle_id}`.
- `hydration.open_write` — fired at FUSE RELEASE on a written file; triggers upload of the cache file.
- `hydration.open_write_begin` — declares a write-side open without downloading (used by `O_TRUNC` opens and bare `truncate(path)` setattr).
- `hydration.close_handle` — fired at FUSE RELEASE; releases the JVM's connection-scoped open-set entry.
- `hydration.hydrate` — explicit hydrate (e.g. `unidrive get`).
- `hydration.dehydrate` — explicit free; refuses with `HydrationError.Busy` if a handle is open.
- `hydration.subscribe` — long-lived NDJSON event stream.
- `hydration.last_synced(path)` — watermark query for crash-recovery cache scan.
- `hydration.list(prefix)` — direct-children listing for `getattr`/`readdir`.

Namespace verbs:

- `hydration.mkdir(path)` — directory creation.
- `hydration.unlink(path)` — file deletion (the co-daemon also evicts the local cache entry).
- `hydration.rmdir(path)` — directory deletion (the co-daemon also evicts the local cache subtree).
- `hydration.create(handle_id, path)` — empty-file materialisation for FUSE `create` / `mknod`.
- `hydration.rename(old_path, new_path)` — file/directory rename, with distinct typed errors for `old_path_not_found`, `new_parent_not_found`, `new_path_exists`.

Canonical contract: `../unidrive/core/app/hydration/src/main/kotlin/org/krost/unidrive/hydration/HydrationIpcHandler.kt`. Field names, error codes, and wire shape are defined there.

## Build and run locally

```bash
cargo build --release
cargo test                                # the gate
./target/release/unidrive-mount --mount <path> --ipc <socket>
```

The binary expects a running unidrive JVM daemon and a UDS socket path passed via `--ipc`.

## Commit etiquette

- Conventional Commits style — see recent `git log` for examples.
- One issue per commit. The `CLOSED.md` append and the issue close land in the same commit as the code change.
- No IDs, dates, or version-number references in new commits, file names, or document body — describe work, not tickets.
- **Split commits cleanly.** Stage hunks explicitly (`git add -p`) rather than `git add .` when working across mixed concerns (e.g. docs vs. code vs. deletions).

## Design constraints (not tickets)

Some constraints bind only when future work happens — they have no current actionable item, but must not be silently forgotten. File them as `design-constraint`-labelled issues on this repo: the rule, the anchor it binds, and the trigger condition.

## What not to do

- **Don't add Windows or macOS support code.** Linux only, kernel 6.9+ only. The Windows desktop surface lives elsewhere (Cloud Files API placeholders) per the sibling repo's multi-platform ADR.
- **Don't host non-FUSE features.** Auth, sync, provider logic, OAuth flows belong in the JVM daemon. This binary is FUSE + IPC + cache, nothing more.
- **Don't reach into the sibling unidrive repo and modify it.** If a contract change is needed, file an issue on `../unidrive/` and stop here. Cross-repo edits in the same session are out.
- **Don't invent IPC verbs unilaterally.** The contract is owned by the JVM-side `HydrationIpcHandler.kt`. New verbs need a sibling-repo issue first.
- **Don't add rustdoc comments where the existing code has none.** The code is the spec.
- **Don't auto-restart a crashed co-daemon.** Explicit user re-mount only.
- **Don't introduce a fallback for kernels < 6.9.** No `if has_passthrough { … } else { read_via_userspace }` runtime branch. The kernel-floor check (exit `EX_CONFIG` below 6.9) is the commitment; the read path is a single shape — userspace pread on the cache FD — at MVP, see the `FUSE_PASSTHROUGH` MVP-shape note above. A future passthrough implementation replaces the userspace shape; it does not stand alongside it.
- **Don't sync-scan the FUSE mount from the engine side.** The engine learns about writes only via `hydration.open_write` at FUSE RELEASE. The cache tree is not in any sync_root.
- **Ask before deleting things you don't recognize.** Unfamiliar files, scripts, branches, or config sections may be in-progress work or load-bearing in a way that isn't obvious. Investigate or ask; don't sweep.

## Work-queue discipline in one line

If it isn't an open issue on this repo, it isn't going to happen. Open one or drop it.

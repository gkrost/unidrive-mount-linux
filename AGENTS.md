# Agent instructions

unidrive-mount-linux is the Linux FUSE co-daemon for the [unidrive](https://github.com/gkrost/unidrive) sparse-hydration roadmap. It implements Phase 2 (FUSE3 mount with `FUSE_PASSTHROUGH` for hydrated files) and Phase 3 (Dolphin / KDE integration crates) of the design spec at `../unidrive/docs/dev/specs/sparse-hydration-roadmap-design.md`. It consumes the Hydration SPI over Unix-domain-socket JSON-line IPC from the sibling unidrive JVM daemon.

This file is the rulebook for everyone touching the repo — human contributors and LLM agents alike. End-users land on `README.md`; the moment you want to *change* something, you read this file.

## Hard rules

- **Single binary at first** (`unidrive-mount`). Workspace structure grows as the spec dictates: `mount` crate now, `kio` crate later for Phase 3. New crates need a named justification tied to the spec.
- **Kernel ≥ 6.9 is the hard floor.** `FUSE_PASSTHROUGH` is required. No graceful degrade to older kernels. The binary refuses to start on too-old kernels with exit code 78 (`EX_CONFIG`) and a one-line stderr.
- **libfuse ≥ 3.16 is the hard floor.** Same shape.
- **One IPC contract surface.** The six hydration verbs (plus Phase-1 follow-ups already landed: `last_synced`, `list`, `subscribe` NDJSON event stream) are documented in the sibling repo at `../unidrive/core/app/hydration/src/main/kotlin/org/krost/unidrive/hydration/HydrationIpcHandler.kt`. Phase 2 does not invent new verbs. If a verb is missing, file a BACKLOG entry on the sibling unidrive repo, not here.
- **No `--respawn` of a crashed co-daemon by default.** The user explicitly chose to mount.
- **No mount-already-exists auto-resolution.** Refuse at startup and ask the user to clear with `fusermount3 -u`.
- **No running as root.** Refuse with an explicit error; mount is per-user.
- **`cargo test`** (in a FUSE-enabled environment, e.g. `--cap-add=SYS_ADMIN` in a container) is the gate. No semgrep, gitleaks, codecov, trivy, clippy baselines.
- **No CI policing.** CI lands when there's a release surface to defend. The dev loop is `cargo test`.
- **No IDs, dates, or version numbers** in commit messages, file names, or document content. Describe what a thing is, not when it was filed or which release ships it.
- **Doc surface is bounded.** Shared docs are this file, `README.md`, `BACKLOG.md`, `CLOSED.md`. Per-crate `README.md` files are permitted; ADRs under `docs/adr/` if a decision is load-bearing enough to outlive memory.

## Output token management

- **Write long outputs to disk.** For long analysis or ticketing sessions, write outputs (tickets, summaries, audits) to files rather than emitting them inline to chat.
- **Keep chat updates concise.** Offload verbose content (logs, full ticket bodies, large diffs) to disk and reference their paths.

## How to work

1. Read the top of `BACKLOG.md`. Pick the first item that isn't blocked.
2. Read three nearby source files before writing. The existing patterns are the style guide.
3. **Pre-execution sanity check.** If the work goes beyond the BACKLOG item itself — scope expansion, deletion of a user-facing feature, new abstraction not already approved, IPC verb invention — surface it and pause for confirmation before executing. Plan approval doesn't cover sideband cuts.
4. Make the change. Run `cargo test`. Iterate.
5. Move the item from `BACKLOG.md` to `CLOSED.md` in the same commit that lands the work. One commit, one item.
6. If you discover a new piece of work, append it to `BACKLOG.md` under the matching priority section in one line.

## Verification

- **Do not rely on summaries.** Verify load-bearing claims (kernel-floor behaviour, IPC wire format, FUSE_PASSTHROUGH ioctl invocation) with a full pass before reporting.
- **Re-verify everything on red flags.** If review flags a fabricated `fuse3` API call or wrong IPC field name, treat it as a signal to re-verify the *whole artifact*, not just the called-out line.
- **Check sibling-repo state.** The IPC contract lives in `../unidrive/`. Before claiming a verb works against the canonical contract, run `git -C ../unidrive fetch origin && git -C ../unidrive log main..origin/main` to confirm the sibling mirror isn't stale.

## What lives where

Initially empty. Module layout lands with the first implementation commit per `../unidrive/docs/dev/specs/sparse-hydration-roadmap-design.md`. Expected shape per the spec:

- `mount/` — Phase 2 crate. `bin/unidrive-mount.rs` entry point, `fuse3` filesystem impl, `IpcClient` over UDS, `LocalCache` reader against `~/.cache/unidrive/hydration/`, kernel-floor check.
- `kio/` — Phase 3 crate. Dolphin `.desktop` ServiceMenus, D-Bus shim, icon-overlay refresh.
- `docs/adr/` — architectural decisions, added on demand.

## Cross-repo contract

The Hydration SPI verbs the co-daemon consumes (JSON-line over UDS):

- `hydration.open_read` — open a path for read; triggers hydrate on cache miss; returns `{cache_path, handle_id}`.
- `hydration.open_write` — fired at FUSE RELEASE on a written file; triggers upload of the cache file.
- `hydration.close_handle` — fired at FUSE RELEASE; releases the JVM's connection-scoped open-set entry.
- `hydration.hydrate` — explicit hydrate (e.g. `unidrive get`).
- `hydration.dehydrate` — explicit free; refuses with `HydrationError.Busy` if a handle is open.
- `hydration.subscribe` — long-lived NDJSON event stream.
- `hydration.last_synced(path)` — watermark query for crash-recovery cache scan.
- `hydration.list(prefix)` — direct-children listing for `getattr`/`readdir`.

Canonical contract: `../unidrive/core/app/hydration/src/main/kotlin/org/krost/unidrive/hydration/HydrationIpcHandler.kt`. Field names, error codes, and wire shape are defined there.

## Build and run locally

Placeholder until the Cargo workspace lands:

```bash
cargo build --release
cargo test                                # the gate
./target/release/unidrive-mount --mount <path> --ipc <socket>
```

The binary expects a running unidrive JVM daemon and a UDS socket path passed via `--ipc`. In production, `unidrive mount <path>` (a CLI subcommand on the JVM) spawns and supervises this binary.

## Commit etiquette

- Conventional Commits style — see recent `git log` for examples.
- One BACKLOG item per commit. The BACKLOG → CLOSED move lands in the same commit as the code change.
- No IDs, dates, or version-number references in new commits, file names, or document body — describe work, not tickets.
- **Split commits cleanly.** Stage hunks explicitly (`git add -p`) rather than `git add .` when working across mixed concerns (e.g. docs vs. code vs. deletions).

## Design constraints (not tickets)

Some constraints bind only when future work happens — they have no current actionable item, but must not be silently forgotten. File them in the *Design constraints* section near the bottom of `BACKLOG.md`, one per constraint: the rule, the anchor it binds, and the trigger condition. Don't put them in the main BACKLOG tables.

## What not to do

- **Don't add Windows or macOS support code.** Linux only, kernel 6.9+ only. The Windows desktop surface lives elsewhere (Cloud Files API placeholders) per the sibling repo's multi-platform ADR.
- **Don't host non-FUSE features.** Auth, sync, provider logic, OAuth flows belong in the JVM daemon. This binary is FUSE + IPC + cache, nothing more.
- **Don't reach into the sibling unidrive repo and modify it.** If a contract change is needed, file a BACKLOG entry on `../unidrive/` and stop here. Cross-repo edits in the same session are out.
- **Don't invent IPC verbs unilaterally.** The contract is owned by the JVM-side `HydrationIpcHandler.kt`. New verbs need a sibling-repo BACKLOG entry first.
- **Don't add rustdoc comments where the existing code has none.** The code is the spec.
- **Don't auto-restart a crashed co-daemon.** Explicit user re-mount only.
- **Don't introduce a fallback for kernels < 6.9.** No `if has_passthrough { … } else { read_via_userspace }` branches. The whole point of the design is the kernel-floor commitment.
- **Don't sync-scan the FUSE mount from the engine side.** The engine learns about writes only via `hydration.open_write` at FUSE RELEASE. The cache tree is not in any sync_root.
- **Ask before deleting things you don't recognize.** Unfamiliar files, scripts, branches, or config sections may be in-progress work or load-bearing in a way that isn't obvious. Investigate or ask; don't sweep.

## Backlog discipline in one line

If it isn't in `BACKLOG.md`, it isn't going to happen. Add it or drop it.

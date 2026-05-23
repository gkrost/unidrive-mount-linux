# unidrive-mount-linux

A Linux FUSE co-daemon for [unidrive](https://github.com/gkrost/unidrive). Mounts cloud storage as placeholder files for un-hydrated content; hydrates on demand and hands the kernel a `FUSE_PASSTHROUGH` FD for zero-userspace reads on hydrated files. Talks to the unidrive JVM daemon over Unix-domain-socket JSON-line IPC. Apache-2.0.

This is the Phase 2 + Phase 3 implementation of the sparse-hydration roadmap. The design spec lives in the sibling repo at `../unidrive/docs/dev/specs/sparse-hydration-roadmap-design.md`.

## Status

Pre-implementation. Scope and design are locked in the spec above. No shippable binary yet — the first implementation commits land the Cargo workspace, kernel-floor refusal, FUSE skeleton, IPC client, and per-test invariants per the spec's testing tables.

## Quickstart (when shipping)

End-user entry point is the unidrive JVM CLI:

```bash
unidrive mount ~/cloud
```

The CLI spawns and supervises this binary, passing it `--mount ~/cloud --ipc <profile-socket>`. Distribution is via the sibling repo's `dist/install.sh`, which downloads the release tarball from this repo's GitHub Releases and drops it at `~/.local/lib/unidrive/unidrive-mount`. No system-wide install; everything under `~/.local/` and `~/.cache/`.

## Requirements

- **Linux kernel ≥ 6.9** (for `FUSE_PASSTHROUGH`; hard floor, no fallback).
- **libfuse ≥ 3.16** (hard floor).
- **rustc** — toolchain version to be pinned with the Cargo workspace.
- The sibling unidrive JVM daemon running, producing a UDS socket the binary connects to.
- Architecture: x86_64 or aarch64.

Windows and macOS are explicitly not in scope. The Windows Cloud Files API placeholder surface lives elsewhere per the multi-platform ADR in `../unidrive/docs/adr/multi-platform.md`.

## Hacking on it / running an agent against it

Read [AGENTS.md](AGENTS.md). It is the rulebook for every change to this repo — human or LLM.

## License

Apache-2.0. See [LICENSE](LICENSE), [NOTICE](NOTICE).

## Sibling repo

[`../unidrive/`](https://github.com/gkrost/unidrive) — the JVM daemon, the Hydration SPI, the canonical IPC contract.

Maintainer: Gernot Krost — `unidrive@krost.org`.

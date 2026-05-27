# GitHub security posture — advisory audit

This is a read-only advisory audit of the GitHub security configuration for
`gkrost/unidrive-mount-linux` after the repo was switched private → public and
several GitHub security services were enabled. No GitHub setting was changed in
the course of this audit; everything below is a recommendation.

## What this repo is

A public **Rust** (cargo) CLI: the UniDrive FUSE co-daemon (`mount/` crate). It
talks to a sibling JVM daemon over a **local Unix-domain-socket, line-delimited
JSON IPC**. There is **no web server, no network listener, and no secret
material in the repo**. Single binary (`unidrive-mount`), small contributor
base, `cargo test` is the gate. These properties drive every recommendation:
the threat surface is *dependency supply chain* and *unsafe-Rust / memory- and
concurrency-correctness in FUSE handling*, **not** web-app classes (SQLi, XSS,
SSRF) or leaked credentials.

## Service-by-service table

| Service | Current state | Recommendation | Rationale |
| --- | --- | --- | --- |
| **Secret scanning** | Enabled | **Keep** | Free for public repos, zero cost to leave on. Currently 0 alerts. No secrets are expected in this repo, but it's cheap insurance against an accidental token paste. |
| **Secret scanning — push protection** | Enabled | **Keep** | Free for public repos. Blocks a credential from ever landing in history. Pure upside. |
| **Secret scanning — validity checks** | Disabled | **Optional / leave off** | Validity checks ping the provider to confirm a found secret is live. Adds value only once a secret is *found*; with 0 alerts and no secrets expected, no urgency. Enable if you ever start handling provider tokens. |
| **Secret scanning — non-provider patterns** | Disabled | **Leave off** | Generic/entropy patterns are noisy and this repo has no custom secret formats. Low value, would only add false positives. |
| **Dependabot alerts (vulnerability alerts)** | Enabled (API returns 204) | **Keep** | The single highest-value service for a Rust CLI: surfaces RUSTSEC/GHSA advisories against the dependency tree (`tokio`, `fuse3`, `libc`, `serde_json`, …). Currently 0 alerts. |
| **Dependabot security updates (automated fixes)** | Enabled, not paused | **Keep** | Auto-opens PRs bumping a vulnerable transitive dep to a patched version. Effective only once `Cargo.lock` is committed — see Gaps. |
| **Dependabot version updates** | **Not configured** (no `.github/dependabot.yml`) | **Configure** | Separate from security updates. Adds routine `cargo` + `github-actions` ecosystem bump PRs so the tree doesn't rot between advisories, and keeps pinned Action SHAs current. See Gaps. |
| **CodeQL code scanning (default setup)** | Configured — languages `rust` + `actions`, weekly, build-mode `none`, CodeQL 2.25.5 | **Keep (with caveat)** | CodeQL Rust went **GA in October 2025**; security queries for Rust shipped in CodeQL 2.23.7/2.23.8 (Dec 2025). The latest analysis ran the **Rust** pack with **25 rules / 0 results** (clean, real coverage — not the empty/preview behaviour of early 2025) and the **actions** pack with **17 rules / 1 result**. So CodeQL is doing real work here. Caveat: for a no-web, no-secrets CLI, CodeQL's *Rust* yield will stay low (it does not replace `cargo audit`/`cargo deny` for the dependency-CVE surface). Keep it for the `actions` coverage and the cheap baseline, but treat supply-chain scanning (below) as the primary control. |
| └─ open code-scanning alert #1 | `actions/missing-workflow-permissions` (medium), `.github/workflows/ci.yml:14` | **Fix** | The `ci.yml` workflow declares no `permissions:` block, so `GITHUB_TOKEN` inherits the repo default. Add `permissions: { contents: read }` at workflow scope. (The repo default is already `read` — see below — so impact is low, but the explicit block silences the alert and is defence-in-depth against a future job that needs more.) |
| **Branch protection — classic** | **None** (`/branches/main/protection` → 404) | **Configure** (or fix the ruleset, below) | `main` is not protected by a classic rule. |
| **Repository ruleset "main protect"** | Exists, enforcement `active`, but **applies to no branches** | **Fix (high priority)** | The ruleset targets `branch` with rules `deletion`, `non_fast_forward`, `copilot_code_review`, but its `conditions.ref_name.include` is **empty**, so it matches nothing. Verified: `gh api .../rules/branches/main` returns `[]` — **zero rules currently apply to `main`**. Direct pushes, branch deletion, and non-fast-forward (force) pushes to `main` are all allowed right now. Add `~DEFAULT_BRANCH` (or `refs/heads/main`) to the include list, **and add a `pull_request` rule + a `required_status_checks` rule gating on the `ci` job** so CI actually blocks merges. |
| **Actions — `GITHUB_TOKEN` default permissions** | `read` (cannot approve PRs) | **Keep** | Already the hardened default. Good. The per-workflow explicit block (alert #1) is still recommended as defence-in-depth. |
| **Actions — allowed actions** | `all` | **Optional** | Acceptable for a small repo. Could tighten to "selected actions + verified creators" but low value at current scale. |
| **Actions — SHA pinning required** | `false` (and `ci.yml` pins by tag: `checkout@v4`, `rust-toolchain@stable`, `cache@v4`) | **Configure** | Pin third-party Actions to full commit SHAs (the `dtolnay/rust-toolchain` and `actions/*` steps). Tag refs are mutable; a compromised tag re-point is a real supply-chain vector. Dependabot `github-actions` updates (above) keep SHAs current once pinned. |
| **Private vulnerability reporting** | Enabled | **Keep** | Free, gives security researchers a private channel instead of a public issue. Pure upside for a public repo. |
| **`SECURITY.md`** | **Absent** | **Add** | A short policy pointing reporters at private vulnerability reporting (and the `unidrive@krost.org` contact already in `Cargo.toml`). GitHub surfaces it in the Security tab and the "Report a vulnerability" flow. |
| **`CODEOWNERS`** | **Absent** | **Optional** | With a small contributor base and a single maintainer, low value now. Worth adding only once required-review-by-owner is wanted; it pairs naturally with the `pull_request` ruleset rule. |
| **`Cargo.lock` committed** | **No** — gitignored (`.gitignore:9`), untracked locally | **Configure (commit it)** | This crate produces a **binary**, so the lockfile should be committed (cargo's own guidance). Two concrete reasons here: (1) Dependabot security/version updates and `cargo audit` operate most precisely against a committed lockfile — without it they fall back to manifest ranges; (2) it makes CI builds reproducible. (Side note: the `ci.yml` cache step comment claims "Cargo.lock is gitignored by current repo policy" — that policy is marked TBD in `.gitignore` and is the thing to revisit.) |
| **Supply-chain CI scan (`cargo audit` / `cargo deny`)** | **Absent** | **Configure (high value)** | See Gaps — this is the highest-leverage *addition* for a Rust CLI. |
| **Squash merge / delete branch on merge** | Squash allowed; delete-on-merge on | **Keep** | Sensible hygiene defaults. |

## Gaps — services / config that WOULD add value but are not enabled

Ordered roughly by value for *this* repo.

1. **`main` is effectively unprotected.** The `main protect` ruleset is active
   but matches no refs (empty include). Add `~DEFAULT_BRANCH` to the include
   list, plus a `pull_request` rule and a `required_status_checks` rule keyed on
   the `ci` workflow's `build-and-test` job. This is the gap with the largest
   blast radius: today anyone with write access (or a leaked token) can push or
   force-push straight to `main` with no CI gate.

2. **Supply-chain scanning in CI via `cargo audit` (or `cargo deny`).** For a
   Rust dependency tree this is higher-signal than CodeQL's Rust pack. Add a CI
   job that runs `cargo audit` (RUSTSEC advisory DB) — or `cargo deny check
   advisories bans sources` for license/duplicate/source policy on top. This
   gates a PR on a vulnerable dependency at merge time, complementing
   Dependabot's after-the-fact alerting. Requires committing `Cargo.lock`.

3. **`.github/dependabot.yml` for version updates.** Security updates are on, but
   routine version-bump PRs are not. A minimal config:
   ```yaml
   version: 2
   updates:
     - package-ecosystem: "cargo"
       directory: "/"
       schedule: { interval: "weekly" }
     - package-ecosystem: "github-actions"
       directory: "/"
       schedule: { interval: "weekly" }
   ```
   The `github-actions` entry keeps the (recommended) pinned Action SHAs fresh.

4. **Explicit `permissions:` block in `ci.yml`.** Add `permissions: { contents:
   read }` at the top of the workflow. Resolves open CodeQL alert #1 and is
   defence-in-depth even though the repo default is already `read`.

5. **Pin Action SHAs.** Replace `actions/checkout@v4`, `actions/cache@v4`, and
   `dtolnay/rust-toolchain@stable` with full commit SHAs (with the tag in a
   trailing comment for readability). Mutable tags are a supply-chain vector.

6. **`SECURITY.md`.** A few lines pointing at private vulnerability reporting and
   the maintainer contact.

7. **Commit `Cargo.lock`.** Prerequisite for #2 precision and #3, and for
   reproducible CI; standard practice for a binary crate.

## Low / no value here — with rationale

- **CodeQL's *Rust* analysis as a security control.** Keep it (it's free, runs
  clean, and the `actions` half is genuinely useful), but do **not** rely on it
  as the primary security control. For a no-web, no-secrets CLI the Rust query
  pack will find little; the dependency-CVE surface — which is where the real
  risk lives — is covered by Dependabot + `cargo audit`/`cargo deny`, not
  CodeQL. Treat CodeQL as a low-cost baseline, not the centerpiece. (Note: this
  reverses the older assumption that CodeQL Rust is empty/preview — it went GA
  Oct 2025 and now ships Rust security queries, so it is no longer noise; it is
  simply low-yield *for this domain*.)
- **Secret scanning validity checks & non-provider patterns.** No secrets, no
  custom secret formats → validity checks have nothing to validate and
  non-provider patterns would only add false positives. Leave off.
- **CODEOWNERS.** Single-maintainer, small contributor base → little to enforce
  today. Revisit alongside required reviews.
- **Tightening "allowed actions" to selected/verified.** Marginal at current
  scale; the higher-value control is SHA-pinning the few actions actually used.

## GitHub MCP — which toolsets add value for this repo

The user asked: "GH offers MCP support — which MCPs offer additional value?"
The GitHub MCP server exposes ~50 tools across ~22 toolsets. The relevant
security toolsets reached **GA** (secret protection, code security/code
scanning, and dependabot dependency scanning), so they're production-usable, not
preview. The question for this repo is which buy something **over the `gh` CLI**,
which an agent here already uses fluently.

**Worth enabling (additive over `gh`):**

- **`dependabot` toolset** (`list_dependabot_alerts`, `get_dependabot_alert`) and
  **`code_security` toolset** (`list_code_scanning_alerts`,
  `get_code_scanning_alert`). Value: lets an agent *triage* the alerts inline —
  read the RUSTSEC/CodeQL finding, locate the offending dependency or workflow
  line, and draft the fix PR in one flow, without the human shuttling alert text
  out of the Security tab. Directly useful here because dependency CVEs and the
  one open `missing-workflow-permissions` alert are exactly the work this repo
  has. (These are read-only tools; the *fix* still goes through normal PR tools.)
- **`secret_protection` toolset** (`list_secret_scanning_alerts`,
  `get_secret_scanning_alert`). GA, read-only. Low immediate value (0 alerts, no
  secrets expected) but zero cost to have available for the day an accidental
  paste happens.
- **`actions` toolset** (`actions_list`, `actions_get`, `get_job_logs`,
  `actions_run_trigger`). Value: tail a failed CI run's logs and re-dispatch
  without leaving the agent loop — meaningfully faster than `gh run view --log`
  copy-paste when iterating on the `cargo build + cargo test` job.
- **`pull_requests`** (`create_pull_request`, `pull_request_read`,
  `pull_request_review_write`, merge) and **`repos`/`context`**. Value: an agent
  can open and review PRs (e.g. the Dependabot bump PRs, or the fixes
  recommended above) end-to-end. Overlaps with `gh pr`, so the gain is mostly
  ergonomic — fewer shell round-trips inside an agent session.

**Redundant with `gh` (no need to add for this repo):**

- **`repos` write tools** (`create_or_update_file`, `push_files`,
  `create_branch`, `delete_file`) and **`issues`**. The agent here works in a
  local clone with git + `gh`; committing through MCP file-write tools is strictly
  more awkward than `git`. Skip these — they buy nothing over the existing
  local-git workflow.

**Bottom line for this repo:** enable the **`dependabot` + `code_security`**
toolsets (and `actions` if you iterate on CI from an agent). They turn alert
triage and CI-log inspection into in-loop steps. Everything else either overlaps
`gh`/local-git or has no alerts to act on yet.

## Audit method (for reproducibility)

All facts above were read via `gh api` against `repos/gkrost/unidrive-mount-linux`
and its sub-resources (`security_and_analysis` block, `vulnerability-alerts`,
`automated-security-fixes`, `code-scanning/default-setup`,
`code-scanning/analyses`, `code-scanning/alerts`, `secret-scanning/alerts`,
`dependabot/alerts`, `branches/main/protection`, `rulesets`,
`rulesets/{id}`, `rules/branches/main`, `actions/permissions`,
`actions/permissions/workflow`, `private-vulnerability-reporting`) and from the
working tree (`.github/workflows/ci.yml`, `.gitignore`, `Cargo.toml`,
`mount/Cargo.toml`). The token used had `repo` scope; all queries returned data
(no 403s). MCP toolset and CodeQL-Rust-GA facts were confirmed via the GitHub
MCP server README and GitHub changelog.

# Codixing — Project Review & Autonomy Roadmap (2026-06-09)

Follow-up to `codixing-full-review-2026-05-30.md`. That review was a deep
correctness audit; this one verifies its remediation, reviews current project
health at v0.44.0, and focuses on the new question: **how to make the project
more autonomous** — both the product (a self-maintaining index) and the
project (self-maintaining repo operations).

---

## 1. Executive summary

The project is in strong shape. The May 30 review's entire P1/P2 backlog was
remediated in a single verified commit (`1688934`, 2026-06-02, +3,604/−267
across 43 files), and spot-checks confirm the fixes are real, not cosmetic:

- `apply_patch` now routes through `resolve_safe_path`, and a shared
  `truncate_chars` helper replaced all three byte-slice panic sites
  (22 combined call sites in `crates/mcp/src/tools/files.rs`).
- Persistence gained a real `atomic_write` (tmp file → `sync_all` → `rename`)
  in `crates/core/src/persistence/mod.rs:25`.
- Federation `max_resident` is clamped to ≥1 on load
  (`crates/core/src/federation/config.rs:92`) — the infinite-loop/deadlock is gone.
- `sync --no-embed` vector removal is now gated on `embedder.is_some()`
  (`crates/core/src/engine/sync.rs:268`), honoring the "stale, not deleted" contract.
- All PageRank variants build a `HashSet` node set (`graph/pagerank.rs:40,164,262`)
  — the O(N²·d) adjacency build is fixed.

The remaining issues are operational, not correctness:

1. **PR #117 is stale and superseded** — the signature-fingerprint feature it
   proposes already landed on main inside commit `1688934`
   (`crates/core/src/engine/fingerprint.rs` exists on main). It should be closed.
2. **CHANGELOG.md is two releases behind** — workspace is v0.44.0 but the
   changelog stops at v0.42.0. v0.43 (MCP profiles + doctor) and v0.44 (agent
   context pack) have no entries.
3. **The nightly agent benchmark is dormant** — `nightly-agent-benchmark.yml`
   has its cron commented out pending an `ANTHROPIC_API_KEY` repo secret.
   This is the single highest-leverage autonomy asset sitting unused.
4. **No automated dependency updates** — no Dependabot/Renovate config, and the
   `cargo-audit` CI job is `continue-on-error` with no escalation path, so
   advisories can pass silently forever.
5. **The `temporal.rs` tests are not hermetic against global git config.**
   Running `cargo test` in an environment with global commit signing enabled
   (e.g. `commit.gpgsign=true`, or managed environments that enforce SSH
   signing) fails all 5 `temporal::tests` — `setup_git_repo()` sets
   `user.name`/`user.email` but inherits everything else, so the test commits
   fail to sign and the assertions see zero history. Fix: run the test git
   commands with `GIT_CONFIG_GLOBAL=/dev/null` / `GIT_CONFIG_SYSTEM=/dev/null`
   env (or add `-c commit.gpgsign=false`) in the helper. With config isolated,
   the full suite passes (verified in this review).
6. Minor: `scripts/check_readme_commands.sh:18` hints
   `cargo build --release -p codixing-cli`, but the package is named
   `codixing` — the hint fails verbatim.

### Verification run (this review)

`cargo test --workspace` on main @ `1688934`: **1298 passed, 0 failed**
(~10 ignored — ONNX-embedder-dependent), `clippy`/`fmt` enforced by CI.
The only failures encountered were the 5 `temporal::tests` under a
signing-enforced global git config — see §3.5; they pass with config isolated.

## 2. What already works (credit where due)

The repo has more automation than most projects its size, and it composes well:

| Asset | What it does autonomously |
|---|---|
| `auto-tag.yml` | Tags releases from `release: vX.Y.Z` commit messages, with a 3-file version-consistency gate before tagging |
| `release.yml` | On tag: downloads CI-built binaries by SHA (no rebuild), creates the GitHub Release, publishes npm |
| CI `check_readme_commands.sh` | Fails CI when README references a nonexistent subcommand — automated doc-drift detection |
| Plugin `PreToolUse` hooks | Redirect agent `Grep`/`grep`/`rg`/`find` to the Codixing CLI (Claude Code **and** Codex via `.codex/hooks/`) |
| Plugin `PostToolUse` hook | `codixing update --file <edited>` after every Edit/Write — the index self-heals during agent sessions |
| `/codixing-release` skill | One-command release pipeline (bump → test → docs → PR → CI → merge → tag → blog → X post) |
| `tool_description_rubric.rs` test | CI fails if a new MCP tool ships without an activation-trigger description |

The release path is already ~80% autonomous: a human (or agent) lands a
`release: vX.Y.Z` commit and tags, binaries, GitHub Release, and npm publish
all cascade without intervention.

## 3. Hygiene findings (fix this week)

1. **Close PR #117.** Superseded by `1688934` on main. Leaving it open invites
   an accidental merge that would conflict with or regress the landed version
   (the PR base is `d9cbeaa`, two commits behind).
2. **Backfill CHANGELOG.md for v0.43 and v0.44**, and add a guard so this
   can't recur: extend `auto-tag.yml`'s consistency step (or `bump_version.py`)
   to require a `## [X.Y.Z]` heading in CHANGELOG.md matching the release
   version. Same pattern as the existing 3-file version check — cheap, total.
3. **Fix the `-p codixing-cli` hint** in `check_readme_commands.sh` → `-p codixing`.
4. **Add Dependabot** (`cargo` + `github-actions` ecosystems, weekly) and make
   `cargo-audit` failures visible: keep `continue-on-error` but add a step that
   opens/updates a labeled issue when the audit step fails, so advisories enter
   the triage queue instead of vanishing into a yellow checkmark.
5. **Test-count drift automation.** README/CLAUDE.md/docs/index.html all
   hard-code the test count (currently 1308), maintained by hand per the
   "documentation is part of the feature" rule. Replace discipline with a
   script: `scripts/check_test_count.sh` that parses `cargo test` summary
   output in CI and fails if the docs disagree. (Or generate the number into
   the docs at release time via `bump_version.py`.)

## 4. Strategic improvements (product)

The May 30 roadmap remains valid; nothing there has shipped since (one commit
since May 30). Rather than repeat it, here is the prioritization I'd defend
today, with the autonomy lens applied:

1. **`codixing watch` as a first-class subcommand** (M). The notify-based
   watcher exists inside the daemon, but there is no `codixing watch`. This is
   the product-side autonomy gap: today index freshness depends on agents
   running `sync`/hooks. A standalone watch mode (and a `--watch` flag on
   `serve`) makes the index self-maintaining for *every* consumer — editors,
   CI, other agents — not just hook-instrumented Claude/Codex sessions.
2. **Scope-resolved `SymbolId`** (XL) — still the precision moat vs SCIP/Kythe.
3. **`codixing astgrep` + rules runner** (L+L) — still the leapfrog vs
   ast-grep/Semgrep, and it feeds autonomy too (see arch-check below).
4. **`codixing arch-check --rules`** (M) — architecture conformance as a CI
   exit code. This converts the dependency graph from an exploration tool into
   an *enforcement* tool — the project guarding its own architecture without a
   human reviewer in the loop.
5. **Parallelize the grep candidate scan** (S) — the verified single-threaded
   gap vs Zoekt; cheapest visible perf win.

## 5. Making the project more autonomous

The goal: the repo maintains itself — index fresh, deps fresh, regressions
caught, issues triaged, releases cut — with humans approving rather than
executing. Ordered by leverage:

### 5.1 Wake the nightly agent benchmark (S — do this first)

Everything is already built: `nightly-agent-benchmark.yml`, the
`claude-agent-sdk` harness, task suites (`agent_tasks*.toml`), artifact upload.
It needs only the `ANTHROPIC_API_KEY` secret and the cron line uncommented.
Then close the loop:

- Compare each run's token/call/success metrics against the last `main`
  baseline artifact; on regression beyond a threshold, **auto-open an issue**
  with the diff table (and on improvement, update a `benchmarks/results/`
  baseline file via PR).
- This is the project's core marketing claim ("73% fewer tokens") under
  continuous verification — the single most on-brand piece of autonomy
  Codixing can have.

### 5.2 Add a Claude Code GitHub Actions workflow (S)

There is currently no `claude.yml`. One workflow file unlocks three autonomous
loops on the repo itself:

- **`@claude` issue triage/fix**: a bug report comes in, `@claude` reproduces,
  fixes on a branch, opens a PR with the verification triad run.
- **Auto-review**: every PR gets a dependency-graph-aware review comment
  (dogfooding `/codixing-review` — which already exists as a skill).
- **CI-failure autofix**: on a red `main` or release branch, the action
  attempts the fix and opens a PR rather than waiting for a human to notice.

Given the repo already ships agent hooks for both Claude Code and Codex,
running its own maintenance through the same agents is the natural next step
— and generates dogfooding telemetry for the benchmark suite.

### 5.3 Scheduled self-audit (S)

A weekly `workflow_dispatch + cron` job that runs the project's own tools
against itself and files issues on drift:

- `codixing audit` (index freshness) and `codixing doctor` on a fresh index —
  catches indexing regressions on real data (this repo).
- `cargo audit` escalation (per §3.4).
- Stale-PR/branch sweep: anything open and untouched >14 days gets a comment
  or label (would have caught PR #117 sitting superseded for 18 days).

### 5.4 Criterion regression gate (M)

CI's `benchmarks` job uploads `bench-results.txt` and stops. CLAUDE.md already
documents named baselines (`--save-baseline vX.Y`) but `cargo clean`/ephemeral
runners discard them. Persist the baseline directory as a cached/committed
artifact keyed to the last release tag, run `--baseline vX.Y` in CI, and fail
(or comment on the PR) when criterion reports a significant regression. Perf
claims are the product; they should be machine-enforced like the test count.

### 5.5 Fully autonomous release train (M)

The last 20% of the release pipeline is interactive (`/codixing-release` asks
for a blog angle, a human invokes it). Two upgrades:

- **Release-readiness bot**: a scheduled job that checks the release checklist
  mechanically (open PRs to main, version locations, changelog entry, CI
  artifacts present with <14-day retention remaining) and posts a "ready to
  cut v0.45" issue when green — turning the release decision into an approval.
- **`workflow_dispatch` release**: trigger `/codixing-release`-equivalent via
  the Claude GitHub Action with the version as input, so a release is one
  click + one PR approval. Keep the human on the merge button; automate
  everything around it.

### 5.6 Product-side autonomy (ties back to §4)

- `codixing watch` (§4.1) — self-maintaining index without hooks.
- A `SessionStart` hook in the plugin running `codixing sync` (fast no-op when
  current, thanks to mtime gating) so every agent session starts with a fresh
  index — today freshness depends on the previous session's PostToolUse hooks
  having fired.
- `codixing doctor --fix` as the self-healing path: hooks currently silently
  no-op when the index is missing/corrupt; doctor could rebuild instead.

## 6. Suggested sequence

| Week | Items |
|---|---|
| 1 | Close #117 · backfill CHANGELOG + add changelog gate · Dependabot + audit escalation · fix `check_readme_commands.sh` hint · add `ANTHROPIC_API_KEY` secret + enable nightly benchmark cron |
| 2 | Claude GitHub Action (`@claude` triage + PR auto-review) · benchmark regression auto-issue · stale-PR sweep |
| 3–4 | `codixing watch` · criterion regression gate · SessionStart sync hook · release-readiness bot |
| 5+ | Resume the strategic roadmap: `SymbolId`, `astgrep`, `arch-check` (arch-check doubles as an autonomy feature — CI enforcing architecture) |

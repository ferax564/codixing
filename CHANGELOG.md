# Changelog

All notable changes to Codixing will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- **reStructuredText (`.rst`) indexing** — new `RstLanguage` `DocLanguageSupport` impl with section-hierarchy tracking following RST's dynamic-level rule (first-seen adornment char = level 1, next distinct = level 2, …). Supports both single-underline and overline+underline title forms, detects `.. code-block::` directives, and extracts `` ``symbol`` `` references via the shared backtick heuristic. Unlocks the Linux kernel `Documentation/` tree (3,909 `.rst` files indexed in 0.48 s on an M4) and every Sphinx-based Python project. Out of scope for this pass: directive expansion, cross-file `:ref:` resolution, Sphinx extensions.
- **AsciiDoc (`.adoc`, `.asciidoc`) indexing** — new `AsciiDocLanguage` impl. Line-based section detector using the `=` prefix count (1–6 → level), code-block detection for `[source,<lang>]` blocks bounded by `----`, backtick symbol refs. Covers Ruby and Java doc estates.
- **Plain-text indexing** (`.txt` + bare `README` / `AUTHORS` / `LICENSE` / `NOTICE` / `CONTRIBUTORS` / `CHANGELOG` / `HISTORY` / `RELEASES` / `COPYING` / `INSTALL` — all extension-less). Paragraph-based sections (blank-line separated) with a 2 KB soft cap; long paragraphs split on sentence boundaries so chunks stay retrieval-friendly.
- **CHANGELOG-aware Markdown mode** — Markdown impl now detects `CHANGELOG*` / `HISTORY*` / `RELEASES*` filenames and splits strictly on level-2 release headings (`## [0.40.0]`, `## v1.2.3`, `## Unreleased`). Sub-headings like `### Added` / `### Fixed` stay **inside** the parent release section, so `codixing search "v0.40 features" --docs-only` lands on the single v0.40 block instead of being scattered across each `###` subsection.
- `DocLanguageSupport::parse_sections` now takes an `Option<&str>` file-name hint so impls can branch on filename; existing impls pass through unchanged for non-hint-sensitive cases.
- `SearchResult::is_doc()` covers `reStructuredText`, `AsciiDoc`, and `Plain text` in addition to Markdown and HTML, so `--docs-only` / `--code-only` filters now cover the full doc-format matrix.
- **`codixing usages --complete` (+ matching `search_usages` MCP `complete=true` parameter)** — deterministic blast-radius mode. Disables ranking and the result cap, returns every known call site / import for the symbol sorted by `(file, line)`. Counters the sticky-mode failure mode where agents trust the top-K ranked view and miss the long tail (see `docs/research-recall-stickiness-2026-04-13.md` §4.8, §4.10 #2). New `ReferenceOptions { complete, max_results }` struct + `Engine::symbol_references` entry-point exposed from `codixing_core`.

## [0.39.0] — 2026-04-18

Maintenance release hardening the external surfaces and the contributor on-ramp. No breaking changes; the shipped binaries are fully backward-compatible with v0.38.1.

### Added

- **LSP integration tests** (`crates/lsp/tests/protocol_test.rs`) — 5 subprocess-harness tests covering the initialize → hover → go-to-definition → references → rename flow. Harness drains stderr to prevent pipe-buffer deadlocks and propagates assertion panics out of the budgeted test thread. URIs are built via `url::Url::from_file_path` so Windows drive letters and backslashes serialize correctly.
- **Server SSE integration tests** (`crates/server/tests/api_test.rs`) — 3 tests covering `POST /index/sync` Content-Type, progress frames, and strict terminal frame semantics (asserts on `frames.last()`, not "any result frame somewhere in the stream").
- **Crate-level `//!` doc** for `codixing-core` summarizing the AST parsing, hybrid retrieval stack, and graph/federation layers — links resolve cleanly on docs.rs.
- **Nightly agent-benchmark workflow** (`.github/workflows/nightly-agent-benchmark.yml`) — scaffold for catching retrieval-quality regressions before release. `workflow_dispatch`-only until the `ANTHROPIC_API_KEY` repo secret lands; cron restoration is a one-line follow-up.
- **`rust-toolchain.toml`** pinning `channel = "stable"` so new contributors to an edition-2024 workspace get a recent enough toolchain automatically.
- **`benchmarks/results/README.md`** freshness index — dates every result file from the last commit touching it and flags anything older than 14 days as a re-run candidate.

### Fixed

- **Panic-prone `.unwrap()` on LSP + MCP handler boundaries** — 27 conversions across `crates/lsp/src/main.rs` and `crates/mcp/src/main.rs`. The most meaningful change: `self.engine.write().unwrap()` in `did_save` no longer crashes the LSP process when the engine RwLock has been poisoned by a prior reindex panic. The handler logs a warning and skips the reindex, keeping the editor session alive. Inner helpers with true structural invariants keep their `.unwrap()` calls.
- **Rust 1.95 clippy compliance** — `collapsible_match`, `collapsible_if`, and `unnecessary_sort_by` lints that landed with rust 1.95.0 resolved across `crates/core`, `crates/lsp`, `crates/mcp`, `crates/server`. No behavioural change; the sort-by rewrites use `sort_by_key(|x| std::cmp::Reverse(x.field))`.
- **`benchmarks/results` date-stamping of retrieval numbers** — README and CLAUDE.md now qualify the 0.763 R@10 / 0.706 MRR figures with "last measured on v0.26.0 (2026-02)" so readers don't assume they reflect v0.39.0.

### Changed

- **Test count**: 1107 → 1115 (+8 LSP + SSE integration tests).
- **Dual plugin manifest** documented explicitly in CLAUDE.md: `.claude-plugin/marketplace.json` is the registry entry, `claude-plugin/.claude-plugin/plugin.json` is the shipping bundle, `scripts/bump_version.py` keeps them version-synced.
- **`audit.toml`** ignores `RUSTSEC-2026-0098` and `RUSTSEC-2026-0099` (rustls-webpki name-constraints advisories, transitive via `reqwest → rustls`) pending a fastembed bump with `rustls-webpki >= 0.103.12`. `ci.yml` passes the two new ignore IDs to `cargo audit`.

## [0.38.1] — 2026-04-14

Patch release bundling the PR #85 review feedback from codex and coderabbit on the v0.38.0 landing. No behavioural change in the shipped `codixing-mcp` binary — all fixes target the benchmark harness (`benchmarks/agent_benchmark_large.py`), ground-truth lists, and documentation that shipped alongside v0.38.0. If you're just consuming the MCP server, v0.38.0 and v0.38.1 are identical at runtime. If you're running the agent benchmark against your own project, v0.38.1 is the one you want.

### Fixed

- **`hard-lx-syscall-openat` ground truth** — was `do_sys_openat2` (a distinct entry point for the `openat2` syscall, #437); corrected to `do_sys_open` which is what `SYSCALL_DEFINE4(openat, ...)` at `fs/open.c:1383` actually dispatches into. Prompt also now explicitly distinguishes `openat(2)` from `openat2(2)`, and the ground truth adds `SYSCALL_DEFINE4(openat` (with open-paren) as a collision-proof third anchor.
- **`hard-oc-exec-approval-flow` ground truth** — was three generic substrings that let partial answers pass with 100% recall; now anchors both endpoints of the flow with concrete symbol names (`createExecApprovalHandlers` in `src/gateway/server-methods/exec-approval.ts` → `createExecApprovalForwarder` in `src/infra/exec-approval-forwarder.ts`), verified via `codixing symbols` on the openclaw index.
- **`lx-callers-1` ground truth + prompt** — was `["mm/"]` (any mm/ mention scored as full recall); now names five concrete mm/ files (slub.c, vmalloc.c, mempool.c, dmapool.c, util.c) verified via `codixing grep kmalloc_node --glob "mm/**"`. Prompt also updated to request those exact files so a divergent-but-correct answer listing other valid call sites (mempolicy.c, zsmalloc.c, khugepaged.c) isn't falsely penalized.
- **Missing-repo filter in `agent_benchmark_large.py`** (codex P1) — tasks whose repo clone is missing or whose name isn't in `REPO_PATHS` are now skipped up-front with a `SKIP` line instead of later crashing with `KeyError` in the main loop or polluting averages with zero-recall failed sessions. If all tasks get dropped, the run aborts with a clear error message.
- **Infra-failure exclusion in `render_report`** — when `query()` raises or finishes without a `ResultMessage`, the session is now flagged with `error` and excluded from the means computed in the Summary and Deltas tables. The raw JSON still contains every session (nothing lost), but aggregates reflect only reproducible runs. A new "Infra-failed sessions" section in the report lists every drop with the error string for inspection. Previously, these looked like model regressions rather than SDK/auth/cwd failures.
- **Paired-session-set aggregation** (codex P1 + coderabbit) — the Summary and Deltas tables now average each mode over the intersection of `(task, run)` pairs where EVERY present mode produced a successful session. Previously, if vanilla infra-failed task X and codixing-sticky succeeded, the means compared different task populations and the headline deltas were apples-to-oranges. Per-task table still shows every available mode; only the aggregate rows use the paired intersection. New header lines report the paired-pair count and list any tasks dropped from the means.
- **Per-task table shows infra failures** — `by_task` is now built from every session (including `result.error` set), so a task where vanilla infra-failed and codixing succeeded still appears as a row with `FAIL` in the vanilla cell. The paired-set filter operates on a separate `ok_by_task` dict, so aggregates remain clean. Successful mode cells that had at least one run fail get a `*(N fail)*` annotation on the average.
- **Word-boundary recall scoring in `score_recall`** — was raw substring match, which meant `do_sys_open` would falsely hit inside `do_sys_openat2`. Now uses `(?<![A-Za-z0-9_])needle(?![A-Za-z0-9_])` so identifier-shaped anchors are collision-proof regardless of what other symbols the result text mentions.
- **Delta sign in `render_report`** — was `(base - other) / base` labeled as `+X%`, which read as "X% more" when it actually meant "X% fewer". Formula flipped to `(other - base) / base` so negative = codixing lower = codixing win, and the column header says "(negative = codixing lower)" explicitly.
- **Cost split between successful and failed sessions** — the `Cost` line now separates `$ok_cost across N successful sessions` from `Wasted on infra failures (≥): $fail_cost across M failed sessions`. The `≥` is explicit: `cost_usd` only populates from `ResultMessage`, so crashes that never emit one report `$0.00` for the failed session even when real API spend occurred — the actual waste is typically higher.
- **`plugin.json` description** — dropped "56 MCP tools" → "67 MCP tools"; trimmed "Runs on macOS, Linux, and Windows" to match README scope ("macOS and Linux; Windows support is available for the raw CLI/MCP binary but the plugin's bash-based dogfooding hooks are not currently tested on Windows").
- **`claude-plugin/README.md` intro** — was "57 MCP tools"; now "67 MCP tools (all always advertised on `tools/list`)".
- **`crates/mcp/src/tools/mod.rs` file-header docstring** — dropped the stale `MEDIUM_TOOLS` mention; v0.38.0 already removed the generated constant.
- **`docs/research-recall-stickiness-2026-04-13.md` §4.20 + §4.21 + §4.16** — rewrote the stale "ship option 3: drop `--medium`, one-line PR" call-to-action as "Shipped in v0.38.0" notes pointing at the `e2e_medium_flag_is_rejected` tripwire test. Backlog item #4 ("Audit which tools are `medium = true`") rewritten for post-removal reality.
- **`docs/index.html` step 4** — copy now mentions both `.mcp.json` and `claude mcp add` paths, and the quickstart code pane's `claude mcp add codixing -- npx -y codixing-mcp --root .` line gains `--no-daemon-fork` to match the shipped plugin config.
- **`benchmarks/agent_benchmark_large.py` module docstring** — removed the stale `--wire-hooks` example (never implemented); replaced with the actual `--only-sticky` / `--tasks-file` / `--output-suffix` flags. Recall scoring description updated from "substring match" to "word-boundary identifier match".

### Not changed

- **`codixing-mcp` binary**: byte-identical to v0.38.0. If you're consuming the MCP server, there's no reason to upgrade from v0.38.0 to v0.38.1 unless you're also running the agent benchmark or consuming the `benchmarks/results/` artifacts.
- **Historical benchmark result files** (`benchmarks/results/agent_benchmark_large*.md`) — kept with their old delta signs. They're frozen regression artifacts; rerunning would invalidate the comparison they exist to provide. Fresh runs going forward use the corrected renderer.
- **Research doc `docs/research-recall-stickiness-2026-04-13.md`** — every reference to `--medium` in a historical / narrative context (e.g. "v2 used `--medium`", "the `--medium` curation was hiding…") is preserved as accurate historical record. Only the stale actionable recommendations were rewritten.

## [0.38.0] — 2026-04-14

v0.38 removes the `--medium` MCP tool curation flag. The April 2026 agent benchmark ([`docs/research-recall-stickiness-2026-04-13.md`](docs/research-recall-stickiness-2026-04-13.md)) found that `--medium` was silently hiding `get_complexity`, `review_context`, `predict_impact` and other showcase tools — erasing Codixing's tool-call and token savings on exactly the tasks it's designed for. Removing the flag restored the reproducible **"66% fewer tokens, 66% fewer calls, +5 pp recall"** headline on the March 4-task prompt set. Full benchmark infrastructure (runner, task files, 4 runs of results, research doc) lands alongside the removal so the result is reproducible.

### Removed
- **`codixing-mcp --medium` flag** — hard-removed. v0.37 and earlier advertised a curated 27-tool subset on `tools/list`; v0.38 always advertises all 67 tools. Users with `--medium` in `.mcp.json` must remove the flag (clap will reject it as an unknown argument). The `e2e_medium_flag_is_rejected` test is a tripwire against accidental re-introduction, mirroring `e2e_compact_flag_is_rejected`.
- **`ListingMode` enum** (`crates/mcp/src/main.rs`) — collapsed to the single "always full" code path. `run_daemon`, `handle_socket_connection`, `handle_pipe_connection`, and `run_jsonrpc_loop` lose their `listing_mode` parameter. `handle_tools_list` signature simplified.
- **`MEDIUM_TOOLS` constant + `medium_tool_definitions()`** (`crates/mcp/build.rs`) — code generation for the curated list deleted. `ToolDef.medium` field dropped.
- **`medium = true` tags** stripped from `crates/mcp/tool_defs/*.toml` (12 lines across 11 files). They're no longer meaningful.

### Added
- **`benchmarks/agent_benchmark_large.py`** — agent benchmark runner using the Claude Agent SDK to compare vanilla Grep/Glob/Read vs. codixing-MCP-sticky-mode on openclaw (~2K TS files) and the Linux kernel (~63K C/H files). Scores ground-truth recall (substring match), reports tool-call breakdowns, supports three modes (`vanilla`, `codixing`, `codixing-sticky` with PreToolUse deny hooks + prompt nudge for production parity). Incremental JSON checkpoint after every session; `--tasks-file`, `--output-suffix`, `--only-sticky`, `--no-sticky` flags for flexible reruns. Reproduces the "66% fewer tokens" headline on `agent_tasks_march_replay.toml` in one command.
- **`benchmarks/agent_tasks_large.toml`** — 8 balanced easy/medium tasks across openclaw and linux. Baseline sanity bench.
- **`benchmarks/agent_tasks_hard.toml`** — 9 grep-hostile tasks including the permanent `hard-oc-complexity` fixture (reproduces March's `grep-impossible-complexity-1`), multi-hop transitive impact, macro-resolved symbols (`SYSCALL_DEFINE4`), per-architecture sweeps (`do_page_fault` across 10+ archs), and NL concept queries (RCU grace period, copy-on-write handling). Ground truth verified upfront with `codixing symbols`/`codixing impact`/`codixing search` so "missed" means genuinely missed.
- **`benchmarks/agent_tasks_march_replay.toml`** — the exact 4 March 2026-03-29 prompts, archived for release-gate isolation of model shift from task-mix shift.
- **`benchmarks/results/agent_benchmark_large*.{md,json}`** — 4 full runs with per-session breakdowns: `_large` (v2 easy), `_hard`/`_hard_full` (v3 hard with/without `--medium`), `_march_replay_medium`/`_march_replay_full` (March prompts with/without `--medium`). The `_medium` runs are kept as negative evidence — every future benchmark PR can diff against them to catch regressions.
- **`docs/research-recall-stickiness-2026-04-13.md`** — full writeup of the 4 runs, §4.15–4.24 documenting the `--medium` curation trap, per-task reading of why sticky mode wins where it wins and loses where it loses, and the reproduction of the March headline once the flag is gone.
- **`e2e_medium_flag_is_rejected`** test in `crates/mcp/tests/e2e_protocol_test.rs` — clap must reject `--medium` with "unexpected argument". Pair of this + `e2e_compact_flag_is_rejected` covers both historical curation flags.

### Changed
- **Shipped configs drop `--medium`** — `.mcp.json`, `claude-plugin/.claude-plugin/plugin.json`, `README.md`, `CLAUDE.md`, `npm/README.md`, `claude-plugin/README.md`, `docs/index.html`, `docs/docs.html`, and `docs/blog-mcp-adoption-fix.html` all updated. The blog post gained an "Update April 2026" banner pointing at the research doc; its historical prose stays intact.
- **Feature-list talking point flipped** — old copy said "Curated tool listing via `--medium` for clients without dynamic tool discovery". New copy says "All 67 tools always advertised on `tools/list` — no curation, no hidden tools."

### Benchmark headline (reproduced on Sonnet 4.6, 4 March prompts, `_march_replay_full`)

| Metric | vanilla | codixing-sticky (full MCP) | delta |
|---|---|---|---|
| Tool calls (mean) | 13.2 | **4.5** | **66% fewer** |
| Tokens (mean) | 11,318 | **3,836** | **66% fewer** |
| Wall time (mean) | 143.8s | **52.2s** | **64% faster** |
| Recall (mean) | 85% | **90%** | **+5 pp** |

The `hard-oc-complexity` task alone goes from **20 calls / 11,792 tokens** (vanilla) to **4 calls / 1,015 tokens** (sticky, full MCP) at 100% recall — 91% fewer tokens from one advertised tool (`mcp__codixing__get_complexity`).

## [0.37.0] — 2026-04-13

v0.37 is a performance and coverage release driven by dogfooding `codixing grep` on the Linux kernel. Three big wins: a compact v2 trigram format (~86% smaller on-disk), lazy-loading of concept+reformulation blobs (~2.6 GB of cold-start bitcode decode deferred), and assembly file coverage for kernel/embedded repos. Plus several grep UX fixes surfaced by the same dogfooding run.

### Added
- **Trigram index v2 on-disk format** — `chunk_trigram.bin` gains an `encoding_flags` header and two pluggable codecs: **delta + varint + u32 IDs** (default; Russ Cox codesearch scheme) and **roaring bitmaps** (alternative, production-grade via the `roaring` crate). On a representative test corpus v2 is **86% smaller than v1** (181 KB → 25 KB). Projected Linux kernel impact: 3.0 GB → ~400 MB. Readers dispatch on the version field (v1 still supported for backward compatibility) and use the compact mmap entry layout (16-byte trigram entries with `posting_byte_off` / `posting_count` / `posting_byte_size`). 5 new round-trip tests in `grep_trigram_test.rs`.
- **Lazy-loaded `concept_index` and `reformulations`** — `Engine::open` no longer eagerly deserializes `concepts.bin` (2.1 GB on the kernel) and `reformulations.bin` (528 MB on the kernel) via bitcode. Both are now wrapped in `OnceLock<Option<T>>` and only touched when `search` actually walks the concept-aware or reformulation code paths. `codixing grep` on the kernel no longer pays that ~2.6 GB decode tax on cold start. `Engine::init` pre-seeds the OnceLocks so the freshly built index works without re-reading disk. `reload.rs` just resets the OnceLocks after sync.
- **`Language::Assembly`** — GAS/Intel assembly files (`.S`, `.s`, `.asm`) are now indexed. Line-based label + `.globl` directive extraction, with preceding `#`/`//`/`;` comments captured as doc comments. Local labels (`.L*`, numeric) are skipped to avoid noise. Closes the kernel coverage gap where `codixing grep "schedule_tail"` missed 19 `arch/*/kernel/entry.S` files that `grep -rF` saw. 2 new tests.
- **`AssemblyLanguage` config support** — new `ConfigLanguageSupport` implementation in `crates/core/src/language/assembly.rs`, registered in the language registry alongside YAML/TOML/Dockerfile/Makefile. No tree-sitter grammar, pure line-based entity extraction.

### Changed
- **`codixing grep --count` and `--files-with-matches` ignore the default `--limit`** — previously both modes were clamped by the 50-hit cap, producing e.g. "50 matches across 2 files" when the real total was 390/57. `--limit` is now `Option<usize>`; when unset and `--count` or `--files-with-matches` is passed, the cap is dropped so totals reflect the real match set. Explicit `--limit` is still honoured for backward compat.
- **`codixing grep` now errors clearly on pre-v0.33 indexes** — previously returned `0 matches across 0 files` silently when the index had no `file_chunk_counts` (pre-content-trigram era), giving no hint to the user. Now surfaces `"grep requires an indexed file set but the index is empty — this is likely a pre-v0.33 index built before the content trigram was added. Run codixing init <root> to rebuild."` as an error.
- **`Engine::open` cold start floor cut by ~2.6 GB** — see the lazy-load bullet above. Biggest single win for `codixing grep` on large repos.

### Fixed
- **Silent zero-match on indexes without a content trigram** — see the Changed entry above.
- **`--count` undercounting due to default `--limit`** — see the Changed entry above.

## [0.36.0] — 2026-04-12

v0.36 closes the last grep-fallback gap and collapses the CI→release pipeline so tagging no longer spends ~25 min rebuilding what CI has already cached. 2 PRs: #80 (`codixing grep`), #81 (CI binary reuse). Skipping the v0.35.x patch series — v0.35.0 shipped clean.

### Added
- **`codixing grep` CLI command** — literal or regex text scan across indexed files, trigram-accelerated. Emits `path:line:col:text` by default (1-indexed line/col to match `grep -n`). Supports `--literal`, `-i/--ignore-case`, `--invert`, `--file`, `--glob`, `-C/-B/-A` (symmetric or asymmetric context), `--count`, `--files-with-matches`, `--json`, and `--limit`. Closes the last grep-fallback gap surfaced during v0.35 polish work. Fast-path auto-proxies through a running `codixing-mcp` daemon when available. (#80)
- **`Engine::grep_code_opts(&GrepOptions)`** — structured variant of `Engine::grep_code` that adds case-insensitive matching (via `regex::RegexBuilder`), inverted line selection, and asymmetric before/after context. Legacy positional `Engine::grep_code(...)` remains as a thin forwarder for backward compatibility. (#80)
- **MCP `grep_code` tool gains new params** — `case_insensitive`, `invert`, `before_context`, `after_context`, `count_only`, `files_with_matches`. Existing `context_lines` still accepted as a symmetric shorthand. (#80)
- **11 new tests** — 4 core `grep_trigram_test` cases (case-insensitive literal, case-insensitive regex, invert, asymmetric context) and 7 CLI `grep_cli_test` cases covering every output mode. Total 1087 → 1098. (#80)

### Changed
- **Bash dogfooding hook shrink** — `claude-plugin/hooks/pretool-bash-codixing.sh` drops the single-file, `| wc -l`, and version-string passthroughs (127 → 112 lines). All three cases are now native `codixing grep` features, so the compliance leaks close. Deny message now suggests `codixing grep "<pattern>"` first. (#80)
- **CI now builds release binaries on main + tag pushes** — `ci.yml` gains a `release-build` matrix job (Linux x86_64, macOS aarch64, Windows x86_64 with `--no-default-features`) that stages binaries as `binaries-<suffix>` artifacts with 14-day retention. PRs remain fast (the job is gated on `github.event_name == 'push'`). Separate rust-cache key (`release-<target>`) so release builds and test builds don't thrash each other. `needs: test` so broken code never produces binaries. (#81)
- **`release.yml` simplified to download + publish** — the old build matrix is gone. On `v*` tag push, `release.yml` fetches binaries from the CI run on the same commit via `dawidd6/action-download-artifact@v6` (`workflow: ci.yml`, `commit: github.sha`), then uploads to a GitHub Release and publishes the npm wrapper. Saves ~25 min per release by reusing the CI build cache. Release-mode build breaks on `main` now surface in CI instead of post-tag. (#81)

## [0.34.0] — 2026-04-12

Audit-driven release bundling v0.33 prep and v0.34 follow-ups. 8 PRs: #69, #70, #71, #72, #73, #74, #75, #76. Skipping the v0.33.0 tag — all v0.33 work is included here.

### Added
- **`codixing filter` CLI subcommand** — validate `.codixing/filters.toml` and run the filter pipeline on stdin without booting the MCP server. `check` and `run` actions. (#75)
- **`codixing sync --no-embed`** — escape hatch that temporarily stashes the embedder for the duration of sync, preventing runaway CPU on existing hybrid indexes. Canonical bad case: Linux kernel sync hit 68 min CPU before kill in v0.32. (#75)
- **CLI daemon proxy full coverage** — `symbols`, `usages`, `impact`, `graph --map` now auto-proxy through a running `codixing-mcp` daemon. ~10× speedup on warm daemon, matching v0.33's `search` speedup. (#75, builds on #73)
- **Windows named-pipe client** — the daemon proxy now works on Windows via `std::fs::OpenOptions` on `\\.\pipe\codixing-<hash>`. First time Windows users see the warm-daemon speedup. (#75)
- **Sync progress output** — `codixing sync` now emits `[sync +Xs] <stage>` lines instead of running silent for minutes. (#69)
- **Bash dogfooding hook** — new `PreToolUse` matcher on `Bash` that catches agents shelling out to `grep`/`rg`/`find`/`cat` against indexed files and redirects to the codixing CLI. Closes the biggest bypass in the v0.32 hook. (#69)
- **Plugin ships dogfooding hooks** — `claude-plugin/hooks/` now contains both hook scripts and `plugin.json` registers them, so downstream plugin users get enforcement automatically. (#69)
- **`codixing callers <file>` diagnostics** — distinguishes four cases: file not on disk, file on disk but not in graph (stale index), has callees but no callers (entry point), normal listing. (#74)

### Changed
- **`codixing init` default flipped to BM25+graph-only.** Embeddings are now opt-in via `--embed`. Rationale: embedding a 10K-file repo took 14 min in v0.32, and 63K Linux kernel files took 25 min — unusable defaults. Agent code exploration via `symbols`/`usages`/`callers`/`impact` works fine on BM25+graph alone. (#71) **Breaking for users who expected embedding by default.**
- **`Engine::open` writer-lock retry loop** — retries 10× with exponential backoff (1ms → 512ms, ~1s total) before falling back to read-only mode. Absorbs the intra-process drop-then-reopen race that caused the macOS `git_sync_no_op_when_already_current` flake. Tests no longer need `thread::sleep(100ms)` workarounds between `drop(Engine::init)` and `Engine::open`. (#76)
- **Warning on sync with missing graph** — older indexes predate graph support; sync now warns that graph-dependent features (`impact`, `callers`, `callees`, `graph --map`) will return empty until `codixing init` rebuilds. (#75)

### Removed
- **`codixing-mcp --compact`** — hard-removed. v0.33 accepted+ignored it with a warning; v0.34 rejects it at argument parsing with "unexpected argument". Users with `--compact` in `.mcp.json` must migrate to `--medium` (for clients without dynamic tool discovery) or remove the flag. Closes issue #67. (#70 deprecated, #76 hard-removed)
- **`codixing init --no-embeddings`** — same deprecation cycle as `--compact`. BM25+graph-only is now the default, so the flag has no remaining semantics. (#71 deprecated, #76 hard-removed)

### Fixed
- **Audit reported 0 files on populated graphs** — on large indexes (observed on the Linux kernel), `chunk_meta` hydration could partially fail, leaving `file_chunk_counts` empty while the graph was fully populated. `audit_freshness` now unions `file_chunk_counts.keys()` with `graph.file_paths()` so it never under-reports. Regression test included. (#72)
- **GraphML namespace URI** — was emitting `xmlns="http://graphml.graphstruct.org/graphml"` (a misspelled placeholder). Fixed to the official `http://graphml.graphdrawing.org/xmlns` so Gephi/yEd accept the output without schema warnings. Regression test pins the correct URI and asserts the old wrong one is absent. (#69)
- **Tantivy `Access is denied` flakes on Windows** — intermittent failures in `Engine::init` and `TantivyIndex::commit` when Windows Defender scans newly-created segment files. New `crates/core/src/index/windows_retry.rs` helper retries operations up to 10× with exponential backoff on recognized transient error codes (5, 32, 33). Zero-cost on Unix. Covers `create_in_dir_with_config`, `open_in_dir_with_config`, `commit()`, and `IndexReader::reload`. (#74)
- **`claude-plugin/.claude-plugin/plugin.json` and `.claude-plugin/marketplace.json`** had duplicate `"version"` keys (technically invalid JSON). Same issue fixed in `npm/package.json` and `docs/install.sh` this release. (#69, #76)
- **Docs drift** — README, CLAUDE.md, docs/index.html now report 1077 tests (was 1019) and 26 CLI commands (was 24/25), add the v0.31.1/v0.32 features that were undocumented on the landing page (GraphML/Cypher/Obsidian exports, git hooks, caller cascade, filter pipeline), and correct the Linux kernel file count from 73K to 63K C/H. (#69)

### Known gaps (v0.35 backlog)
- **Stale codixing index after Edit/Write** — the plugin doesn't yet auto-update the index after file edits, so `codixing symbols` and `codixing usages` can return stale line numbers between syncs. Tracked in issue #77. Workaround: run `codixing sync` or `codixing update` manually after a batch of edits.
- **Daemon proxy for `callers` and `callees`** — the MCP `symbol_callers`/`symbol_callees` tools are symbol-level while CLI works on files. No clean mapping yet; commands stay in-process.
- **`codixing sync` doesn't rebuild a missing graph** — only warns. A true rebuild is effectively a full init and the user should invoke it explicitly.

## [0.31.0] — 2026-04-11

### Added
- **Community detection** — Pure-Rust Louvain algorithm on the import graph; `codixing graph --communities` shows natural module clusters with modularity score
- **Shortest path** — `codixing path <from> <to>` finds the shortest import chain between two files via BFS
- **Surprise detection** — Scores edges by unexpectedness (cross-community, PageRank disparity, cross-directory, low confidence); `codixing graph --surprises N`
- **HTML graph export** — `codixing graph --html` generates a self-contained interactive visualization with force-directed layout, community coloring, confidence-styled edges, and surprise highlights
- **Edge confidence** — Every dependency edge tagged `Verified`/`High`/`Medium`/`Low` based on extraction method
- **PreToolUse hook** — Plugin ships a deterministic hook that denies Grep on code/doc/config files and redirects to codixing CLI (replaces the advisory PostToolUse reminder)

### Fixed
- Shortest path BFS excludes `__ext__` pseudo-nodes to prevent false paths through shared external imports
- Legacy graph deserialization derives edge confidence from edge kind instead of defaulting all to Verified
- HTML export escapes `</script>` in embedded JSON to prevent XSS breakout

## [0.21.0] — 2026-03-28

### Changed
- **Refactor:** Split `engine/mod.rs` (2,981 lines) into 4 focused submodules: `init.rs`, `indexing.rs`, `reload.rs`, `validation.rs` — mod.rs now ~1,100 lines
- **Fix:** Replace 28 lock `.expect()`/`.unwrap()` calls with poison-recovery across SharedSession, FederatedEngine, HNSW, and parallel grep
- **Feat:** Implement `Reranker` trait on concrete fastembed Reranker struct, unifying the reranker interface
- **Test:** Add 7 HTTP server integration tests (reindex, remove, repo-map, callees, call-graph, export, view) — 20/21 routes now covered
- **Docs:** Fix tool count inconsistency (was 49/53/54, now consistently 54), update test counts to 845+

## [0.19.0] — 2026-03-27

### Changed
- **Perf:** Kernel-scale performance — 11× smaller chunk_meta, lazy trigram loading via OnceLock, content dedup
- **Perf:** Mmap symbol table — zero-deserialization loading from flat binary
- **Perf:** 306× faster trigram build via batch mode + disk persistence

## [0.18.0] — 2026-03-25

### Added
- Multi-query RRF fusion for natural language queries
- Recency boost stage (+10% linear decay over 180 days)
- File path boosting (2.5× for explicit paths and backtick refs)
- Overlapping chunks at AST boundaries

## [0.17.0] — 2026-03-24

### Added
- Trigram pre-filtering for grep_code (110× faster at 1K files)

### Fixed
- Windows CI permanently fixed (single-threaded test runner)

## [0.16.0] — 2026-03-23

### Added
- 15 features: field BM25, search pipeline, LSP rename, complexity diagnostics, semantic tokens, CI coverage, federation auto-discovery, daemon mode, and more
- HTTP server with 21 REST endpoints + SSE streaming
- VS Code extension

## [0.15.1] — 2026-03-22

### Fixed
- Fix 2 security vulnerabilities: lz4_flex (RUSTSEC-2026-0041) and rustls-webpki (RUSTSEC-2026-0049) via dep update
- Fix Windows CI build failure: server crate now proxies usearch feature (matches mcp/lsp/cli pattern)
- Make cargo-audit CI job blocking (was `continue-on-error`) with explicit `--ignore` for unfixable transitive deps

### Changed
- Updated all transitive dependencies via `cargo update`
- Added `audit.toml` documenting ignored advisories with justification and resolution plan
- Document broader Windows Tantivy flake surface in CLAUDE.md
- Add "Adding a new crate" checklist to CLAUDE.md

### Known Issues
- `time 0.3.45` (RUSTSEC-2026-0009, medium severity) pinned by tantivy 0.22 — resolution planned for v0.16.0 tantivy bump

## [0.14.0] — 2026-03-21

### Added
- Post-v0.13.0 technical roadmap for stability, performance, quality, and ecosystem
- Quality rules in CLAUDE.md: mandatory verification triad, documentation-with-every-feature

### Fixed
- Ignore `multi_root_indexes_both_roots` test on Windows (Tantivy lock flake)
- Move implementation plans out of `docs/` to prevent Jekyll build failures

## [0.13.0] — 2026-03-15

### Added
- Symbol-level call graph for precise callers/callees with trait dispatch resolution
- Windows support via brute-force vector fallback (no usearch dependency)
- Read-only index access for concurrent engine instances
- MCP progress notifications for long-running tool calls
- `--medium` compact mode for MCP tool listing (between full and `--compact`)
- Claude Code plugin with 3 skills: `/codixing-setup`, `/codixing-explore`, `/codixing-review`
- Plugin marketplace manifest for self-hosted install
- OpenAI Codex CLI integration instructions

## [0.12.1] — 2026-03-10

### Added
- Initial public release
- 20 language support with full AST parsing via tree-sitter
- Hybrid search (BM25 + optional vector embeddings with RRF fusion)
- 53 MCP tools across 7 categories
- Daemon mode with Unix socket IPC and auto-fork
- Cross-repo federation with RRF fusion
- LSP server with hover, go-to-def, references, call hierarchy, complexity diagnostics
- GitHub Action for automated code review
- VS Code extension with LSP integration
- CLI binary with search, symbols, callers/callees commands
- Dynamic tool discovery with `--compact` mode (96.7% token reduction)
- Token budget management with adaptive truncation
- Single binary distribution (no external dependencies)

### Fixed
- Strip build paths from release binaries

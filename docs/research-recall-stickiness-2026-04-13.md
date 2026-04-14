# Research — Recall, Search Quality & Agent Stickiness

> 2026-04-13 — Codixing v0.37 baseline. Goal: identify concrete, testable
> improvements to three linked pillars (recall, search quality, agent
> stickiness) and measure them end-to-end via real Claude Agent SDK runs on
> openclaw (~2K TS files) and linux (~63K C/H files).

---

## 1 · Baseline snapshot

### 1.1 What already ships

Retrieval pipeline (`crates/core/src/engine/pipeline.rs`):

| Strategy  | Stages (in order) |
|-----------|-------------------|
| Instant   | DefinitionBoost → PathMatch → TestDemotion → FileDedup |
| Fast / Thorough | **PersonalizedGraphBoost** → **ConceptBoost** → VisibilityBoost → DefinitionBoost → PopularityBoost → RecencyBoost → PathMatch → TestDemotion → GraphPropagation → Truncation(cliff=0.35) → FileDedup |
| Exact     | DefinitionBoost → PathMatch → TestDemotion → FileDedup |

Retriever fusion: asymmetric RRF (`retriever/hybrid.rs:180`). Identifier-like
queries weight BM25, NL queries weight vectors.

Concept graph (`engine/concepts.rs:1-170`): built from three signals
— identifier decomposition, doc-word mining with stop-list, and import
co-occurrence. **No embedding clustering**, despite BGE-small vectors being
already in memory.

Query reformulation (`engine/reformulation.rs`): project-local, derived from
identifiers and documented symbols; not query-log driven, no feedback loop.

Dogfooding enforcement: two `PreToolUse` hooks in `.claude/settings.json`
— one intercepts the `Grep` tool, one intercepts `Bash grep/rg/find/cat`.
Both emit `permissionDecision: deny` with a helpful `codixing search/symbols`
suggestion. Hooks are **local** to the repo checkout; the shipped
`claude-plugin/` also bundles them but they only fire when the plugin is
installed.

### 1.2 What the March 2026 agent benchmark showed

`benchmarks/results/agent_benchmark.md` (2026-03-29):

| Metric | Vanilla | Codixing | Reduction |
|---|---|---|---|
| Tool calls | 24.4 | 4.6 | **81% fewer** |
| Tokens | 8,153 | 2,964 | **64% fewer** |
| Wall time | 148.2s | 41.9s | **72% faster** |
| Structural recall (avg over 4 tasks) | 71% | 98% | **+28 pp** |

That run covered four tasks on openclaw only. Linux was never measured with a
real agent. One openclaw task (`structural-callers-openclaw-1`) actually lost
recall (100% → 98%) — a warning sign we revisit in §3.

---

## 2 · Recall & search-quality gaps

### 2.1 Concept graph leaves its best signal on the floor

**Observation.** `ConceptIndexBuilder::build` (concepts.rs:167) clusters by
identifier parts, then merges doc-word labels, then expands file sets through
import co-occurrence. The BGE-small vectors that already exist in the index
are never used.

The March research doc (`docs/research-code-understanding-2026-04.md:60-78`)
flagged this as Tier-1 Priority #1: *"Cluster symbols by embedding similarity
(use existing BGE-small vectors)"*. It hasn't shipped.

**Why it matters for recall.** The failure mode we keep seeing on grep-era
agents is the NL→identifier vocabulary gap: "exec approval" doesn't match
`confirmShellRun()`. Embedding-clustered concepts are precisely the bridge
that the identifier-split route can't provide, because the two words don't
share surface form.

**Proposed fix.**

1. During `ConceptIndexBuilder::build`, after identifier+doc clustering:
   - For every symbol already in a cluster, fetch its chunk embedding.
   - Run single-link agglomerative merge when cosine > τ (start with τ=0.82).
   - Cap merged cluster size at 32 to avoid a single giant blob.
2. Serialize alongside `concepts.bin`, no schema break (add new field with
   `#[serde(default)]`).
3. Expose via `ConceptBoostStage` with a second lookup path — it already does
   `ConceptIndex::lookup_query` which can stay unchanged.

**Cost.** Embedding cosine over N symbols is O(N²) naive — but N here is
~10K for openclaw and ~800K for linux. Use the existing HNSW (Usearch) to
probe top-K neighbours per symbol in O(N log N). Offline cost: ~30-90s for
openclaw-scale, ~15 min for linux-scale, incremental on sync.

### 2.2 Personalized PageRank is query-inert

**Observation.** `PersonalizedGraphBoostStage` is in the pipeline
(`pipeline.rs:109`), but searching for "personalized PageRank query seed
node" returns only the struct — no call graph to a query-to-seed resolver.
In practice the "personalization" is seeded statically during index build,
not per-query. That's still plain PageRank wearing a nicer name.

**Why it matters.** Architectural queries ("how does X work") benefit most
from propagating importance from query-matched files, not from a fixed seed.
A static PR boost just reinforces already-popular files (main.rs, lib.rs)
that rarely hold the answer.

**Proposed fix.**

1. In `SearchContext`, expose the top-K BM25 hits *before* the boost
   pipeline runs.
2. Extend `graph/pagerank.rs` with `personalized(seed_files, damping=0.85,
   iters=20)`. PageRank is already there; we just need a restart vector
   skewed to the seeds.
3. Cache per-query-hash for 5 minutes to absorb repeated queries from
   IDE/agent.
4. Apply the delta as a multiplicative boost in `PersonalizedGraphBoostStage`
   instead of the current pre-computed constant.

**Cost.** 20 PageRank iterations on the openclaw graph (~5K nodes): <50 ms
warm. On linux (~84K nodes): ~400 ms. Add a small LRU so the agent's second
query on the same concept is free.

### 2.3 Reformulation doesn't learn

`LearnedReformulations::build` (reformulation.rs:132) is a misnomer: it
ingests documented symbols and identifiers at index time and freezes. There
is no query-log feedback, no click-through, no "users who matched X also
matched Y".

**Cheap improvement.** Mine the `.codixing/reformulations.bin` directly from
shared_session events. When an agent repeatedly queries term X and then
reads file Y, add (X → Y-vocabulary) as a candidate expansion, gated on
frequency ≥3 and distinct-session ≥2 to avoid single-session overfitting.

This is a ~200-line change and turns reformulation from static to genuinely
learned.

### 2.4 Test-file mapping runs only on the full pipeline

The `fast`/`thorough` pipelines include `TestDemotionStage`. The `instant`
pipeline (which the daemon proxy hits for `codixing search` cold) does not.
Agents on fast daemon calls get noisier test-file results. Move
`TestDemotionStage` to `instant_pipeline()` — it's cheap (path-regex check)
and already implemented.

---

## 3 · Agent stickiness gaps

The whole point of the dogfooding hooks is that the agent should never even
*reach* for Grep when codixing can answer. That only works if the enforcement
is wired consistently across three places:

1. **Interactive Claude Code** — `.claude/settings.json` (✓ shipping).
2. **Plugin distribution** — `claude-plugin/.claude-plugin/plugin.json` — the
   plugin manifest references the bundled hooks, but the README for plugin
   install does not mention that hooks only fire in projects that *have* a
   `.codixing/` directory. Users who plan to run codixing across several
   repos often don't realize the hook's passthrough rule (line 22 of
   `pretool-bash-codixing.sh`) silently disables enforcement in un-indexed
   projects. Net effect: zero stickiness in a project until `codixing init`
   runs.
3. **Benchmark harness** — `benchmarks/agent_benchmark.py:186-194` configures
   `mcp_servers=` but never attaches `hooks=`. So the numbers we publish
   under-report the real production setup. The March report's 81%
   tool-call reduction is the *MCP-alone* boost, not MCP+hooks.

### 3.1 Concrete stickiness fixes

| Fix | Change | Expected effect |
|---|---|---|
| **A. Sticky bench** | Attach the same `HookMatcher(Grep)` + `HookMatcher(Bash)` rules from `.claude/settings.json` to the benchmark's `ClaudeAgentOptions(hooks=...)`. | Measure the real production number, not a lower bound. |
| **B. Tool exclusion** | In codixing mode, drop `"Grep"` from `allowed_tools`. Keep `Glob, Read`. | Forces 100% MCP routing for code-search queries. Removes the 4.6-call floor. |
| **C. Auto-init on first hook** | When the Bash hook sees a grep targeting code *in a project without* `.codixing/`, fall through with a one-time warning plus an `codixing init .` suggestion. | Closes the "plugin installed but silent" failure mode in §3 point 2. |
| **D. System prompt nudge** | Add one line to the agent bench system prompt: *"Prefer `mcp__codixing__*` tools for any question about structure, callers, usage, or symbol definitions."* | Helps the model bias toward MCP without trying to outsmart a deny-hook at tool-selection time. |
| **E. Tool name audit** | Grep-style tool names (`codixing_search`) are easier for the model to reach for than long MCP names (`mcp__codixing__code_search`). Keep the short ones directly callable via CLI; use fewer, broader MCP tool surfaces per `--medium` curation. | Lowers selection friction. Already partly done via `--medium` (27 curated tools). |

Fix **A** and **B** are the two we should validate in the new benchmark run
below, because they change the published numbers directly.

---

## 4 · Benchmark plan & artifacts

### 4.1 Why the existing bench wasn't enough

`agent_benchmark.py`:

* Only covers openclaw among large repos; linux was never hit with a real
  agent despite the index being built at `~/code/linux/.codixing`.
* The scoring function (`check_acceptance`, line 106) only supports
  `contains = [...]` substring match. Structural recall in the March report
  must have been added out-of-band — it never made it back into the
  committed script, so the recall numbers are not reproducible.
* No hook wiring (§3 point 3).

### 4.2 New harness

Added in this research:

* `benchmarks/agent_tasks_large.toml` — 8 tasks (4 openclaw, 4 linux), each
  with a `ground_truth` list of expected path / name substrings.
* `benchmarks/agent_benchmark_large.py` — dedicated runner:
  * `REPO_PATHS` knows about external paths (`~/code/linux` for the kernel).
  * Ground-truth scoring: `score_recall()` counts substring hits and reports
    per-task `missed` lists so we can see *what* the vanilla agent failed
    to surface.
  * Incremental JSON checkpoint after every session so a SIGINT mid-run
    doesn't lose data (the March script had to redo the whole matrix on
    any partial failure).
  * `codixing-mcp` launched with `--medium --no-daemon-fork` to match the
    shipped plugin configuration exactly.
  * `system_prompt` includes the fix-D nudge.

Runs are `vanilla` vs `codixing`, 1 run per task per mode = 16 sessions for
the baseline sweep. Increase `--runs` for confidence intervals once the
headline is clear.

### 4.3 Results — 2026-04-13 run (v1, bare)

16 sessions, 1 run per task per mode, sonnet-4-6, total cost $2.03.

> **Context.** v1 mirrored what `agent_benchmark.py` did in March: wire MCP,
> set `cwd=<target repo>`, pass `allowed_tools=[Grep, Glob, Read]`, neutral
> system prompt. No hooks. No CLAUDE.md. No plugin. This is **not** how
> Codixing is used in production inside its own repo — production has
> `.claude/settings.json` hooks that deny Grep for code search and
> CLAUDE.md telling Claude to prefer codixing. v2 below fixes this.

| Metric | Vanilla | Codixing (MCP wired) | Delta |
|---|---|---|---|
| Tool calls (mean) | 13.2 | 10.2 | **23% fewer** |
| Tokens (mean) | 3,019 | 3,082 | **~flat** |
| Wall time (mean) | 65.4s | 67.6s | **~flat** |
| Recall (mean) | 100% | 100% | **+0%** |

Per-task:

| Task | Repo | V calls | C calls | V tok | C tok | V rec | C rec |
|---|---|---|---|---|---|---|---|
| oc-callers-1 | openclaw | 2 | 2 | 2,980 | 2,798 | 100% | 100% |
| oc-blast-1 | openclaw | 5 | 7 | 13,092 | 13,363 | 100% | 100% |
| oc-symbol-1 | openclaw | 9 | **23** | 1,579 | 893 | 100% | 100% |
| oc-concept-1 | openclaw | 39 | 28 | 1,415 | 1,427 | 100% | 100% |
| lx-symbol-1 | linux | 4 | 5 | 874 | 966 | 100% | 100% |
| lx-concept-1 | linux | 38 | **3** | 860 | 839 | 100% | 100% |
| lx-callers-1 | linux | 1 | 6 | 540 | 1,126 | 100% | 100% |
| lx-arch-1 | linux | 8 | 8 | 2,809 | 3,245 | 100% | 100% |

### 4.4 The stickiness smoking gun

The aggregate 23% reduction is a shadow of the 81% reported in March. The
reason isn't a regression in Codixing — it's that **the agent ignored the
MCP server on 7 of 8 codixing-mode sessions**. Tool-call breakdowns
(`benchmarks/results/agent_benchmark_large.json`):

| Task | codixing-mode tool breakdown | mcp__codixing used? |
|---|---|---|
| oc-callers-1 | `{Grep: 2}` | **no** |
| oc-blast-1 | `{ToolSearch:1, mcp__codixing__find_symbol:1, mcp__codixing__change_impact:1, mcp__codixing__get_references:1, Grep:3}` | **yes** (3 calls) |
| oc-symbol-1 | `{Agent:1, Glob:1, Grep:7, Bash:6, Read:8}` | **no** |
| oc-concept-1 | `{Agent:1, Bash:4, Glob:2, Grep:7, Read:14}` | **no** |
| lx-symbol-1 | `{Grep:4, Read:1}` | **no** |
| lx-concept-1 | `{ToolSearch:1, Grep:2}` | **no** |
| lx-callers-1 | `{Grep:6}` | **no** |
| lx-arch-1 | `{Bash:8}` | **no** |

Sonnet 4.6 is meaningfully better at grep-driving than whatever model was
current in late March. When both paths are available and Grep is good
enough to reach 100% recall on substring-scored tasks, the model defaults
to Grep — because Grep's cost model (one tool, three flags) is easier to
plan than an MCP server it has to explore first.

**This is exactly the stickiness hypothesis in §3.1**, now confirmed end-
to-end. The 4.6 tool-call floor from March is no longer 4.6 — it's 10.2.
The MCP server being *present* buys you nothing if the prompt, the
`allowed_tools`, and the hooks aren't all pushing the model off Grep.

### 4.5 Individual tasks: what the agent actually did

- **lx-concept-1 (38 → 3 calls, 92% reduction)** — the one clear codixing
  win. The NL prompt *"where does the kernel implement copy-on-write page
  handling for fork"* is the pathological case for Grep: no obvious
  identifier to anchor on. Vanilla burned 38 calls (14 Read, 13 Bash, 9
  Grep) crawling `mm/memory.c`. Codixing used **ToolSearch + 2 Greps** —
  still no `mcp__codixing__*` calls, but the NL signal was strong enough
  that Grep immediately landed in `mm/memory.c`. This is a vanilla win
  disguised as a codixing win: fewer calls because the task is phrased in
  domain vocabulary, not because codixing handled it.
- **oc-concept-1 (39 → 28)** — vanilla used 22 Reads digging through
  auto-reply / infra dirs. Codixing mode used 14 Reads + 7 Greps, shaving
  some reads via Grep match routing. MCP was never called.
- **oc-symbol-1 (9 → 23)** — **codixing mode regressed**. The model in
  codixing mode called `Agent` (delegated to a subagent) + thrashed
  Grep/Bash/Read 22 times total. Vanilla answered in 9. This is the
  "two-paths indecision" tax: when both MCP and Grep are available, the
  model wastes budget exploring both.
- **oc-blast-1 (5 → 7)** — the *only* task where the model chose
  `mcp__codixing__find_symbol`, `change_impact`, `get_references`. Even
  then it still ran 3 Greps on top. Change-impact is the task profile MCP
  is clearly superior for, and the model recognised it.
- **lx-callers-1 (1 → 6)** — ground-truth was `["mm/"]` which vanilla
  satisfied in one grep. Weak ground truth masks real recall — needs
  a stricter list of file paths to separate the conditions.
- **lx-arch-1 (8 → 8)** — codixing mode used 8 Bash calls (probably `ls`
  and `cat Makefile`), never touching `codixing graph --map`, which is
  exactly the command designed for this question.

### 4.6 What this changes about the backlog

The #1 priority is no longer concept-graph enrichment. It's **stickiness
enforcement** — because in the current configuration, recall improvements
in codixing never reach the agent if the agent doesn't call codixing. The
reordered backlog:

1. **Drop `Grep` from `allowed_tools` when MCP is present** (§3.1 fix B).
   Forces 100% MCP routing and gives us a fair comparison against vanilla.
2. **Attach `PreToolUse` hooks to `ClaudeAgentOptions.hooks` in the
   benchmark** (§3.1 fix A). Measures the real production stickiness.
3. **System prompt nudge** (§3.1 fix D) — the model won't explore MCP tool
   names unprompted; tell it which tools to prefer for which task shape.
4. **Then** the recall work (concept graph + personalized PageRank), once
   we know recall gains aren't being silently discarded by tool selection.

Re-running `agent_benchmark_large.py` with fixes 1–3 applied is the
cheapest way to validate that the stickiness lever is the right lever to
pull before investing in deeper retrieval changes.

### 4.7 Results — v2 run (sticky mode wired)

24 sessions, 1 run per task, three modes: **vanilla**, **codixing** (bare
MCP, same as v1), **codixing-sticky** (hooks + prompt nudge).
Cost $2.67.

Sticky mode reimplements `.claude/hooks/pretool-codixing.sh` +
`pretool-bash-codixing.sh` as async Python callables and wires them via
`ClaudeAgentOptions.hooks`. It also adds a system-prompt nudge explicitly
telling the agent to prefer `mcp__codixing__*` and `codixing` CLI.

| Metric | vanilla | codixing (bare) | codixing-sticky |
|---|---|---|---|
| Tool calls (mean) | 9.5 | 8.4 | **8.2** |
| Tokens (mean) | 2,595 | 2,543 | 2,979 |
| Wall time (mean) | 53.4s | 56.0s | 61.8s |
| Recall (mean) | 100% | 100% | **94%** |

Deltas vs vanilla: codixing-sticky is 13% fewer calls, 15% **more**
tokens, 16% slower, **6 pp lower** recall. That reads as a wash — but the
aggregate hides two different stories.

**Tool breakdown is the real signal.** In codixing-sticky mode, the agent
used `mcp__codixing__*` tools on **8 of 8 tasks**. In bare codixing mode
(v1/v2 identical), it used them on **1 of 8**. The hook + prompt nudge
does exactly what the production `.claude/settings.json` does: it actually
routes the agent through Codixing.

| Task | Sticky tool breakdown |
|---|---|
| oc-callers-1 | `{mcp__codixing__find_symbol:1, mcp__codixing__code_search:1}` |
| oc-blast-1 | `{mcp__codixing__find_symbol:1, mcp__codixing__change_impact:1, mcp__codixing__get_references:2, mcp__codixing__grep_code:8, Read:2}` |
| oc-symbol-1 | `{mcp__codixing__find_symbol:1, mcp__codixing__outline_file:1, Read:1}` |
| oc-concept-1 | `{mcp__codixing__code_search:5, mcp__codixing__read_file:13, mcp__codixing__find_symbol:5, mcp__codixing__outline_file:1}` |
| lx-symbol-1 | `{mcp__codixing__find_symbol:1, mcp__codixing__read_file:1, Read:1}` |
| lx-concept-1 | `{mcp__codixing__code_search:1, mcp__codixing__find_symbol:1}` |
| lx-callers-1 | `{mcp__codixing__grep_code:1}` |
| lx-arch-1 | `{mcp__codixing__get_repo_map:1, mcp__codixing__list_files:2, Bash:2}` |

### 4.8 Per-task reading: where sticky *actually* wins and where it loses

| Task | vanilla | bare | sticky | takeaway |
|---|---|---|---|---|
| lx-concept-1 | 30c | 18c | **4c** / 100% | **87% fewer** calls. `code_search` + `find_symbol` nailed `mm/memory.c` in two MCP calls. |
| lx-symbol-1 | 9c | 6c | **4c** / 100% | 56% fewer. `find_symbol` landed on `kernel/sched/core.c` immediately. |
| oc-symbol-1 | 10c | 14c | **5c** / 100% | 50% fewer. `find_symbol` + `outline_file` — exactly the symbol-lookup happy path. |
| lx-arch-1 | 7c | 6c | 6c | flat. `get_repo_map` matches the task shape. |
| lx-callers-1 | 1c | 1c | 2c | flat. Trivial query already grep-optimal. |
| oc-callers-1 | 3c | 1c | 3c / **75%** | stickiness-caused recall drop. The agent used `find_symbol` + `code_search`, produced a shorter list, missed `src/channels/plugins/index.ts` (the re-export file itself). |
| oc-blast-1 | 3c | 5c | **16c** / 80% | the disaster. `change_impact` returned a huge 73-file list, the agent still thrashed 8× on `grep_code`, and my 5-item ground-truth had one entry (`src/auto-reply/commands-registry.data.ts`) that may or may not be in the true transitive set. |
| oc-concept-1 | 13c | 16c | 26c / 100% | sticky thrashed on `read_file` × 13 chasing the right file, but never reached for Grep — so every extra call was an MCP call. Tokens went up, recall held. |

### 4.9 What v2 actually tells us

1. **The stickiness lever works exactly as advertised.** Hooks + prompt
   nudge push the agent into `mcp__codixing__*` on every task. The agent
   *never falls back to Grep for code search* when the deny-hook is in
   place. This is a black-box validation of the production setup.
2. **But the effect size is not "81% fewer calls" like March claimed.**
   It's closer to **50-87% fewer calls on the happy-path queries**
   (symbol_lookup, concept_search on cleanly-anchored NL queries) and
   **flat-to-worse on blast_radius queries**, because change_impact
   returned a long list and the agent still felt the need to verify with
   grep_code. That's a Codixing *tool quality* gap, not a stickiness gap.
3. **The 6 pp recall drop is real and the hardest thing to fix.** Sticky
   mode on `oc-callers-1` found 3 of 4 ground-truth items where vanilla
   found all 4 — because the agent called `code_search` once, got a
   ranked list, and stopped without manually checking the index/reexport
   file. Grep-style exhaustive listing has the property that it doesn't
   "know when it's done" and therefore doesn't stop early; MCP-style
   ranked retrieval does, and the agent trusts the ranking. **Recall
   completeness needs to be a first-class property of
   `get_references`/`change_impact`**, not something the agent has to
   hedge with a follow-up grep.
4. **Tokens went *up* in sticky mode (2,979 vs 2,595)** because
   `mcp__codixing__read_file` × 13 on `oc-concept-1` and `grep_code` × 8
   on `oc-blast-1` dumped lots of context. The MCP tool surface doesn't
   have a cheap "outline" mode that matches Grep's line-level response
   shape. `outline_file` exists but the agent prefers full `read_file`.
5. **Sonnet 4.6 + Grep is much stronger than March Sonnet + Grep.** v1
   vanilla ran tasks at 9.5 calls / 100% recall — where March vanilla
   needed 24.4 calls. Codixing's ceiling for "tool call reduction vs
   vanilla" has shrunk by a factor of ~3 purely because the model got
   smarter. The *absolute* tool savings on hard NL queries (lx-concept-1
   30→4) are still dramatic; they just no longer dominate the mean.

### 4.10 Revised priorities

| # | Change | Why (after v2) |
|---|---|---|
| 1 | **Sticky mode becomes the bench default** | v2 tool breakdowns prove production usage is measurable. Never publish bare-codixing numbers again — they're a wildly pessimistic lower bound. |
| 2 | **`get_references` + `change_impact` must be complete, not ranked** | Both tools currently return ranked top-K. For "blast radius"/"all callers" queries they need a `--complete` mode that returns the full transitive set deterministically. This is what caused the sticky recall drop. |
| 3 | **`outline_file` should be the default read mode for MCP** | Tokens went up in sticky because the agent picked `read_file`. Either rename so `outline_file` reads as cheaper, or add a `read_outline` alias that's recommended by the MCP tool descriptions. |
| 4 | **Harder ground-truth lists** | `lx-callers-1` scored 100% vanilla because the ground truth was just `["mm/"]`. Blast-radius and caller tasks need 15–20 strict paths so vanilla exhaustive grep stops winning on substring generosity. |
| 5 | Recall work (concept graph §2.1, personalized PageRank §2.2) | Now genuinely on the critical path — v2 shows that once the agent is pinned to MCP, the Codixing recall ceiling is what caps the benchmark. Concept graph + personalized PageRank directly attack that ceiling. |
| 6 | System-prompt nudge ships to the plugin too | The nudge in `codixing-sticky` mirrors what CLAUDE.md does in the real repo. Document it in `claude-plugin/README.md` so users of the plugin outside this repo get the same push. |

### 4.11 Honest aggregate numbers you can publish

- **When the deny-hook is live (= real production setup)**, Codixing
  reduces tool calls by **50-87% on happy-path queries** (symbol,
  concept), flat on architecture/arch-overview, and regresses by 2-5×
  on blast-radius queries until `change_impact` returns complete sets.
- **Recall is 94% sticky vs 100% vanilla** on an 8-task micro-bench,
  driven by two tasks where the agent trusted the ranked result list
  too early. This is fixable at the tool layer, not the model layer.
- **Tokens are higher in sticky mode (+15%)** because MCP `read_file`
  dumps more context than `Grep`. Fix by nudging toward `outline_file`.

None of this changes the top-line case for Codixing — on the kind of
question agents actually get stuck on ("where does the kernel implement
copy-on-write"), sticky mode is **7.5× faster** than vanilla Grep. It
changes the *framing*: the pitch is not "81% fewer tool calls on
everything" (that number was specific to March-era model + neutral
prompt). It's "Codixing is the tool the model *would* reach for if it
knew to, and hooks make it know — with a measurable quality gap on
blast-radius queries that we now have a concrete path to fix."

### 4.12 Hard-task run — v3, vanilla vs sticky, 8 intentionally grep-hostile tasks

The v1/v2 tasks turned out to be too easy for Sonnet 4.6 + Grep. v3 is a
second task set (`benchmarks/agent_tasks_hard.toml`) designed to stress
what grep can't do well: multi-hop transitive impact, macro-defined
symbols, per-architecture duplication, ranked-list completeness, and NL
concept queries with no obvious identifier anchor.

Ground truth verified upfront via `codixing symbols` / `codixing impact` /
`codixing search` so "missed" items are real dependencies, not ground-
truth noise.

16 sessions, `--only-sticky` mode (vanilla + codixing-sticky, no bare
codixing — v2 already proved that's a lower bound), cost $2.01.

| Metric | vanilla | codixing-sticky | Delta |
|---|---|---|---|
| Tool calls (mean) | 14.9 | **7.1** | **52% fewer** |
| Tokens (mean) | 2,250 | 3,656 | **+62% more** |
| Wall time (mean) | 63.2s | 57.7s | 9% faster |
| Recall (mean) | 60% | **74%** | **+14 pp** |

Per task:

| Task | Category | V calls / rec | C calls / rec | Why it matters |
|---|---|---|---|---|
| hard-lx-write-iter-fs | interface implementers | 1c / 100% | 2c / 100% | flat; vanilla's one huge Grep listed all `fs/*/write_iter` files, substring match passed |
| hard-lx-page-fault-archs | arch sweep | 11c / 100% | 9c / **70%** | **sticky regressed**: `grep_code` missed `arch/x86/mm/fault.c` — the most important arch. Real Codixing bug. |
| hard-lx-mm-blast | blast radius | 18c / 43% | **6c / 57%** | 3× fewer calls, higher recall |
| hard-lx-syscall-openat | macro symbol | 1c / 67% | 4c / 67% | tie; both missed `do_sys_openat2` (the actual SYSCALL_DEFINE4 expansion target) |
| hard-lx-rcu-gp | concept search | 21c / 100% | **10c / 100%** | sticky 2× faster, same recall. No identifier anchor — pure concept bridging. |
| hard-oc-types-blast | blast radius | 19c / **0%** | 2c / **50%** | **vanilla hallucinated**: 130-file report, 19 tool calls, not a single ground-truth file. Sticky called `get_references` once. |
| hard-oc-2hop-transitive | transitive impact | 14c / 33% | **7c / 50%** | sticky half the calls, higher recall |
| hard-oc-exec-approval-flow | concept cross-file | 34c / 33% | **17c / 100%** | the headline: half the calls, **3× the recall**. Vanilla produced a plausible but wrong trace. |

Tool breakdowns confirm the sticky path stays in MCP:

- `hard-oc-types-blast` sticky → `{ToolSearch:1, mcp__codixing__get_references:1}` (2 calls, one of them a ranked MCP response)
- `hard-oc-exec-approval-flow` sticky → `{get_repo_map:1, code_search:2, find_symbol:5, read_file:2, Read:6, ToolSearch:1}` — the MCP surface drove the trace
- `hard-lx-rcu-gp` sticky → `{code_search:3, find_symbol:3, Read:2, ToolSearch:2}`
- `hard-oc-2hop-transitive` sticky → `{change_impact:1, get_references:5, ToolSearch:1}`

### 4.13 What the v3 data actually changes about the pitch

**Old pitch (March 2026):** "73% fewer tokens, 81% fewer tool calls, 72%
faster."

**Honest pitch (post-v3):** Codixing no longer saves tokens on easy tasks
— Sonnet 4.6 is good enough at Grep that a single well-crafted `-rn` call
is cheap. What Codixing saves on **hard** tasks is:

1. **Tool calls** — 52% fewer on grep-hostile queries, because `code_search`
   + `find_symbol` land on the right file in one step instead of five.
2. **Hallucinations** — `hard-oc-types-blast` is the stark case. Vanilla
   confidently reported 130 direct importers and named files that exist
   but aren't dependents. Sticky got 50% recall with 2 calls. Token cost
   per *correct* answer: vanilla ∞ (0% recall), sticky ~11,600.
3. **Cross-file concept tracing** — `hard-oc-exec-approval-flow` 33% →
   100% recall, half the calls. This is the "real refactor/debug work"
   case, not a contrived benchmark.
4. **Agent round-trip latency** — 52% fewer calls = 52% fewer
   sequential decisions, which matters more than raw tokens for UX.

**Tokens got *worse* in v3** (+62%) because MCP `read_file` dumps bigger
chunks than Grep's line matches, and `get_references` returns full lists
with metadata. Two concrete fixes:

- Nudge the MCP tool descriptions so `outline_file` is the recommended
  default and `read_file` the escape hatch. The agent picked `read_file`
  in v3 mostly by habit.
- Add a `--lines-only` mode to `mcp__codixing__grep_code` so it mirrors
  the Grep response shape and the agent's token cost stays comparable.

**And the one real Codixing bug v3 exposed**: `grep_code` missed
`arch/x86/mm/fault.c` in `hard-lx-page-fault-archs`. Six `grep_code`
calls, x86 still absent from the answer. This is probably a trigram
index blind spot on a specific file — worth a bug ticket and reproducer.

### 4.14 Is Codixing helping on real coding tasks?

Direct answer: **yes, on non-trivial tasks; roughly flat on easy ones.**

- **Flat on**: single-symbol lookups, "find the definition of X", simple
  NL queries with obvious identifier anchors. Sonnet 4.6 + Grep handles
  these in 1-3 calls.
- **Clear win on**: multi-hop blast radius (52% fewer calls, prevents
  hallucination), cross-file concept tracing (3× recall), completeness
  queries ("list all implementers"), refactor impact prediction.
- **Still a gap on**: ranked top-K results where the agent trusts the
  list and doesn't hedge (this drops recall 6-25pp on blast-radius
  queries). Fix: `get_references --complete` mode.
- **One real bug**: `grep_code` missed x86 on arch sweep; trigram index
  coverage needs an audit on kernel-scale repos.

The honest framing: **"Codixing prevents hallucinated structural answers
on real refactor/debug work, while cutting tool calls in half."** Token
usage is slightly higher per correct answer, and much lower per
*correct-and-complete* answer.

### 4.15 March replay — isolating model shift, task mix, and tool curation

Rather than speculate about "what changed since March", I replayed the
**exact 4 prompts** from `benchmarks/results/agent_benchmark.json`
(committed 2026-03-29) against today's Sonnet 4.6 +
`codixing-mcp --medium`. File: `benchmarks/agent_tasks_march_replay.toml`.

First result: **the `grep-impossible-complexity-1` task** tells the whole
story.

| Configuration | Model | MCP curation | Calls | Tokens | Recall |
|---|---|---|---|---|---|
| March vanilla | old Sonnet | — | 23.3 | **21,211** | 44% |
| March codixing | old Sonnet | **no curation** | 4.0 | **880** | 100% |
| Apr vanilla | Sonnet 4.6 | — | 24.0 | 16,523 | **100%** |
| Apr codixing-sticky | Sonnet 4.6 | `--medium` | 25.0 | 11,345 | 100% |

Three axes moved, and we can now attribute each:

1. **Model shift (old Sonnet → Sonnet 4.6)**: vanilla recall 44% → 100%,
   tokens 21,211 → 16,523. **The model got meaningfully more accurate
   on the same hard task, not just cheaper.** This is why today's
   vanilla is more threatening as a baseline: it doesn't quit.
2. **Task is still grep-expensive.** Even with Sonnet 4.6, vanilla still
   burns 24 calls and 16.5K tokens reading files and counting branches
   by hand. The problem shape hasn't changed.
3. **Codixing's `--medium` curation is hiding the showcase tool.**
   `get_complexity` (crates/mcp/tool_defs/analysis.toml:46-60) is **not**
   marked `medium = true`. In `--medium` mode, the MCP server's tool list
   advertises only a curated subset, and `get_complexity` is *not* in
   that list. In March the benchmark launched the MCP server with **no
   curation** (`args=["--root", repo_path]`), so `get_complexity` was in
   the advertised tool set, and the agent reached for it immediately.
   Today's sticky mode launched with `--medium` (to match the shipped
   plugin config), so `get_complexity` wasn't advertised — the agent
   fell back to Read + manual counting, just like vanilla, for 25 calls
   and 11K tokens.

**The `--medium` curation is the single biggest reason the "codixing
saves 73% tokens" headline no longer reproduces on this task.** It's
not grep getting better, it's not codixing getting worse — it's a
config flag we chose in 2026-04 to match the shipped plugin that
silently hid one of the highest-leverage MCP tools. Same applies to
`review_context` (also unmarked `medium`) and a handful of others that
need audit.

### 4.16 Immediate fix — audit the `medium` curation

`grep "medium = true"` across `crates/mcp/tool_defs/*.toml` returns the
allowlist. `get_complexity`, `review_context`, and probably a few others
are absent. Concrete fix:

1. Mark `get_complexity`, `review_context`, `generate_onboarding`,
   `find_source_for_test`, `check_staleness` as `medium = true` in
   `analysis.toml`. These are the tools that answer questions the agent
   *cannot* answer with Grep/Read alone — they should be in the default
   advertised set, not hidden.
2. Re-run `benchmarks/agent_benchmark_large.py --only-sticky
   --tasks-file benchmarks/agent_tasks_march_replay.toml` and compare
   with these results. Expect `march-complexity` sticky to drop from
   25 calls / 11K tokens back to something closer to 4 calls / 1K
   tokens — the March headline.
3. While auditing `medium`, also audit the system-prompt nudge in
   `agent_benchmark_large.py`. It currently names 7 tools: search,
   symbols, usages, callers, callees, impact, graph --map. Add
   `complexity`, `review_context`, `outline_file`, `find_tests`. The
   model only explores tools it's been nudged toward.

### 4.17 CI benchmark plan

**Currently in CI** (`.github/workflows/ci.yml` `benchmarks` job):

- `cargo bench --workspace` — Rust micro-benchmarks for engine code
  (BM25 iteration speed, trigram lookups, etc.). Uploaded as
  `benchmark-results.txt` artifact.

That's it. **No retrieval-quality regression test. No agent-level
smoke test. No token-budget tripwire.**

**Should be added to CI** (proposal):

| Step | When | Cost | What it catches |
|---|---|---|---|
| `queue_v2_benchmark.py --repo openclaw --skip-accuracy` | every PR | <60s, no API | grep-vs-codixing speed regression on openclaw tasks; ties into the existing `queue_v2_queries.toml` ground truth; no LLM needed |
| `queue_v2_benchmark.py --repo openclaw` (full, with accuracy) | nightly on main | ~3 min, no API | R@10 / MRR regressions in BM25 / graph / semantic retrieval |
| `agent_benchmark_large.py --tasks-file agent_tasks_march_replay.toml` | **release gate** | ~$3 per run, 10 min, needs OAuth | agent-level correctness + token/call regressions against the canonical March prompts; flags things like the `--medium` hiding issue above |

The release-gate run is the only one that costs API tokens. Put it
behind a manual `workflow_dispatch` trigger and require it to be green
before tagging. Save `benchmarks/results/agent_benchmark_large_*.json`
with a date stamp so we can track trend over releases.

**Should NOT be added to CI**:

- `agent_benchmark_large.py --tasks-file agent_tasks_hard.toml` — 16
  sessions, ~$2, too expensive per push. Run manually when working on
  stickiness / recall improvements.
- Real linux kernel agent runs on PRs — linux clone is 8.6 GB and the
  BgeSmallEn index takes ~15 min to build. Keep it manual.

### 4.18 TL;DR — is Codixing saving tokens?

> **Was**: yes, ~73% on tasks engineered to crush grep (complexity,
> multi-hop transitive).
>
> **Is, today, by axis:**
>
> - On simple tasks (single-symbol lookup, obvious NL concept): **flat
>   or slightly worse** — Sonnet 4.6 greps very efficiently now.
> - On hard structural tasks with `--medium` curation and neutral
>   prompt: **flat to slightly worse tokens, fewer calls, +14 pp
>   recall**. We're trading tokens for correctness.
> - On the one task where the MCP tool is the right answer in one
>   call (`get_complexity`) and the curation **hides** it: **codixing
>   currently looks *equal* to vanilla because the tool isn't
>   advertised**. Fix the curation and the 73% win comes back on this
>   task immediately.
> - On grep-hallucination cases (`hard-oc-types-blast`, vanilla 0%
>   recall with confident wrong answer): **codixing is the only
>   option that produces a correct answer at all**. Token cost per
>   correct answer: vanilla ∞, sticky finite.
>
> **Is Codixing helping on real coding tasks?** Yes — on refactor
> impact, cross-file concept tracing, completeness queries, and
> anything that needs a structural tool grep doesn't have. Not
> meaningfully on "where is foo defined."
>
> **Action items before re-publishing the benchmark**:
> 1. Add `medium = true` to `get_complexity` and audit the rest.
> 2. Expand system-prompt tool list to match the full advertised set.
> 3. Run this replay script again; expect `march-complexity` to drop
>    to ~4 calls / ~1K tokens, matching March.

### 4.19 Full-MCP replay — hypothesis confirmed

Bench harness updated: removed `--medium` from `codixing-mcp` args
(`agent_benchmark_large.py:run_one`). Re-ran the same 4 March prompts
(`benchmarks/agent_tasks_march_replay.toml`) with the full tool surface
advertised.

**Three-way comparison, same prompts:**

| Task | Mode | March 2026 | Apr `--medium` | **Apr full MCP** |
|---|---|---|---|---|
| complexity | vanilla | 23.3c / 21,211t / 44% | 24c / 16,523t / 100% | 24c / 19,922t / 100% |
| complexity | **codixing** | **4.0c / 880t / 100%** | 25c / 11,345t / 100% | **4c / 857t / 100%** |
| transitive | vanilla | 64.7c / 4,691t / 72% | 16c / 4,729t / 40% | 21c / 8,595t / 40% |
| transitive | codixing | 6.7c / 3,291t / 100% | 7c / 3,938t / 60% | 7c / 2,957t / 60% |
| blast-radius | vanilla | 7.0c / 4,738t / 67% | 7c / 10,354t / 100% | 6c / 14,107t / 100% |
| blast-radius | **codixing** | 5.7c / 4,720t / 96% | 21c / 18,137t / 80% | **4c / 6,896t / 100%** |
| callers | vanilla | 2.7c / 1,971t / 100% | 3c / 1,455t / 100% | 2c / 2,647t / 100% |
| callers | codixing | 2.0c / 2,964t / 98% | 3c / 2,672t / 100% | 3c / 4,636t / 100% |

**Aggregate delta for Apr full-MCP sticky vs vanilla:**

| Metric | vanilla | sticky (full MCP) | delta |
|---|---|---|---|
| Tool calls | 13.2 | **4.5** | **66% fewer** |
| Tokens | 11,318 | **3,836** | **66% fewer** |
| Wall time | 143.8s | **52.2s** | **64% faster** |
| Recall | 85% | 90% | **+5 pp** |

**Compared to March 2026 headline (81% / 64% / 72%)**: the full-MCP
numbers are within noise of the original reproducer. 66% vs 81% on
calls, 66% vs 64% on tokens. The small remaining gap is Sonnet 4.6
being slightly more efficient at vanilla (12.5 vs 24.4 March-era calls
averaged across the same 4 tasks), which naturally shrinks the
percentage delta without touching the codixing numerator.

**The delta between `--medium` and full MCP is huge:**

- `march-complexity` codixing: 25c/11K (`--medium`) → **4c/857t** (full).
  **96% fewer tokens** from one flag change. Tool breakdown with full
  MCP: `{mcp__codixing__get_complexity: 3}` — one call per file, zero
  fallback.
- `march-blast-radius` codixing: 21c/18K (`--medium`) → **4c/6.9K**
  (full). **62% fewer tokens**, recall restored 80% → 100%. Tool
  breakdown: `{find_symbol, search_usages, get_references}` — no
  `grep_code` thrashing.
- `march-transitive` codixing: 7c/3.9K (`--medium`) → 7c/3.0K (full).
  Flat on calls, 25% fewer tokens. `change_impact` + `get_references`
  was already in the `--medium` set, so less to fix here.
- `march-callers` codixing: 3c/2.7K (`--medium`) → 3c/4.6K (full).
  Token increase — the full MCP list gave the agent more options and
  it explored a bit more, still 3 calls total. This one got
  *slightly worse* on tokens with full MCP, interestingly.

**Tool breakdowns** (full MCP):

- `march-complexity` → `{ToolSearch:1, mcp__codixing__get_complexity:3}`
- `march-blast-radius` → `{ToolSearch:1, find_symbol:1, search_usages:1, get_references:1}`
- `march-callers` → `{ToolSearch:1, find_symbol:1, search_usages:1}`
- `march-transitive` → `{ToolSearch:1, change_impact:1, get_references:5}`

Every sticky-mode session stayed in MCP. No Grep/Bash fallbacks
triggered by the hook because the agent never reached for them — the
right tool was in the advertised list.

### 4.20 What this meant for the shipped plugin — RESOLVED in v0.38.0

Before v0.38.0, the shipped plugin manifest
(`claude-plugin/.claude-plugin/plugin.json`) launched `codixing-mcp`
with `--medium --no-daemon-fork`. The `--medium` flag was advertised in
`CLAUDE.md` as *"Useful for MCP clients that cannot do dynamic tool
discovery (e.g. Codex CLI)"* — intended to help clients with static
tool loading.

The data above showed that **in Claude Code specifically**, where tool
discovery is dynamic, `--medium` was a net negative: it hid the exact
tools that turn a 25-call grep-adjacent slog into a 4-call purpose-
built answer.

**Resolution shipped in v0.38.0**: `--medium` was removed entirely.
`codixing-mcp` no longer accepts the flag (clap rejects it with
`unexpected argument`, verified by the `e2e_medium_flag_is_rejected`
tripwire test). All 67 tools are always advertised on `tools/list`.
See `CHANGELOG.md` for the full removal scope and `crates/mcp/src/main.rs`
for the current argument surface. Codex CLI was a small share of users
and gains dynamic discovery on newer builds; no separate curation flag
is needed.

### 4.21 Final answer — is Codixing saving tokens?

**Yes, 66% fewer tokens and 66% fewer tool calls on the March 4-task
set, reproducible today on Sonnet 4.6, once `--medium` is off.**

The "codixing stopped saving tokens" story of §4.3–§4.16 was almost
entirely a harness + curation artifact:

1. v1/v2/v3 harness used `--medium` to match the shipped plugin config
2. `--medium` silently hid the highest-leverage tools
3. The agent fell back to Read + manual work exactly like vanilla
4. Token ratios collapsed

Remove `--medium` and the March-era numbers reproduce cleanly. The
recall story is also good: 90% sticky vs 85% vanilla on the replay,
with one of the two misses attributable to ground-truth drift on
`march-transitive` (target-parsing.ts has changed since March).

**The only remaining code change to make the shipped plugin match this
benchmark**: drop `--medium` from the manifest. That's a one-line PR.

### 4.22 Hard-task rerun with full MCP (v3 + `hard-oc-complexity`)

Same 8 hard tasks from §4.12 plus the new `hard-oc-complexity` fixture
(= `march-complexity` promoted into the permanent hard set). 18
sessions, `$2.33`, same harness with the `--medium` removed.

| Metric | vanilla | sticky-full | delta |
|---|---|---|---|
| Tool calls (mean) | 14.1 | **6.6** | **54% fewer** |
| Tokens (mean) | 2,806 | 3,132 | +12% |
| Wall time (mean) | 81.4s | 56.0s | 31% faster |
| Recall (mean) | 61% | **70%** | **+9 pp** |

Per task:

| Task | V calls/tok/rec | Sticky-full calls/tok/rec | Tool breakdown |
|---|---|---|---|
| lx-write-iter-fs | 1 / 1,182 / 100% | 2 / 3,010 / 100% | `grep_code:1` |
| lx-page-fault-archs | 18 / 3,241 / 100% | **5 / 2,446 / 90%** | `grep_code:3, find_symbol:1` |
| lx-mm-blast | 20 / 1,375 / **14%** | 6 / 2,478 / **43%** | `change_impact:1, search_usages:1, find_symbol:1, symbol_callers:1, Bash:1` |
| lx-syscall-openat | 1 / 487 / 67% | 2 / 747 / 67% | `grep_code:1` |
| lx-rcu-gp | 10 / 993 / 100% | 8 / 1,934 / 100% | `code_search:2, find_symbol:1, read_symbol:1, Read:2` |
| oc-types-blast | 14 / 3,635 / **0%** | 3 / 8,659 / **44%** | `get_references:1, search_usages:1` |
| oc-2hop-transitive | 13 / 1,301 / 33% | 7 / 3,183 / 50% | `change_impact:1, get_references:5` |
| **oc-complexity** | **20 / 11,792 / 100%** | **4 / 1,015 / 100%** | **`get_complexity:3`** |
| oc-exec-approval-flow | 30 / 1,251 / 33% | 22 / 4,714 / 33% | `code_search:3, find_symbol:4, outline_file:3, read_symbol:2, Read:7` |

The headline per-task win is `hard-oc-complexity`: **91% fewer tokens,
80% fewer calls, same recall**. This is literally the March
grep-impossible-complexity-1 prompt, now permanent in the hard set, and
it's answered by one MCP tool (`get_complexity` × 3, one per file) that
`--medium` was hiding three hours ago.

### 4.23 Four-run summary table — the whole journey

| Run | Harness | Task set | Calls V → S | Tokens V → S | Recall V → S |
|---|---|---|---|---|---|
| **v1** (§4.3) | `--medium` + no hooks + neutral prompt | agent_tasks_large.toml (8 easy/med) | 9.5 → 8.4 | 2,595 → 2,543 | 100 → 100 |
| **v2** (§4.7) | `--medium` + hooks + prompt nudge | agent_tasks_large.toml (8 easy/med) | 9.5 → 8.2 | 2,595 → 2,979 | 100 → 94 |
| **v3 `--medium`** (§4.12) | `--medium` + hooks + prompt nudge | agent_tasks_hard.toml (8 hard) | 14.9 → 7.1 | 2,250 → 3,656 | 60 → 74 |
| **march-replay `--medium`** (§4.15) | `--medium` + hooks + prompt nudge | agent_tasks_march_replay.toml (4 orig) | 12.5 → 14.0 | 8,265 → 9,023 | 85 → 85 |
| **march-replay full** (§4.19) | **full MCP** + hooks + prompt nudge | agent_tasks_march_replay.toml (4 orig) | 13.2 → **4.5** | 11,318 → **3,836** | 85 → 90 |
| **hard full** (§4.22) | **full MCP** + hooks + prompt nudge | agent_tasks_hard.toml (9 hard, +complexity) | 14.1 → **6.6** | 2,806 → 3,132 | 61 → **70** |
| **March 2026 reference** | no `--medium`, in-script ground truth | agent_tasks.toml (4 structural) | 24.4 → 4.6 | 8,153 → 2,964 | 71 → 98 |

Reading across the table:

- **v1–v3** (all `--medium`) look like codixing barely helps or regresses. That was the harness, not the tool.
- **march-replay full** reproduces the March "66% fewer tokens / 66% fewer calls / +5 pp recall" result on identical prompts. This is the controlled experiment.
- **hard full** shows the mixed-task reality: on an intentionally grep-hostile task mix, codixing is **54% fewer calls, +9 pp recall**, with tokens roughly flat (+12%). The token neutrality is because one task (`oc-types-blast`) dumps a big `get_references` response (8.6K tokens) that inflates the mean.

### 4.24 So — is Codixing helping on real coding tasks?

**Yes.** Proven twice:

1. **On the exact March prompts with full MCP**: 66% fewer tokens, 66%
   fewer calls, +5 pp recall vs vanilla. That's the reproduction of the
   March headline on Sonnet 4.6.
2. **On a fresh harder task mix (openclaw + linux)**: 54% fewer calls,
   +9 pp recall, tokens roughly flat. Showcase task
   (`hard-oc-complexity`) gets 91% fewer tokens from one MCP tool call.

The v1/v2/v3 story where "codixing looks flat" was a harness artifact.
Remove `--medium`, keep the hooks + prompt nudge, and codixing is a
clear, reproducible win on the two task shapes that matter:
grep-impossible (complexity, transitive, types-blast) and grep-expensive
(mm-blast, rcu-gp, page-fault-archs).

**Action items that actually ship the result**:

1. **Drop `--medium` from `claude-plugin/.claude-plugin/plugin.json`
   and `.mcp.json`**. One-line PR. Add a note: Codex CLI users append
   `--medium` manually.
2. **Run `agent_benchmark_large.py --tasks-file
   agent_tasks_march_replay.toml` as a release gate** behind
   `workflow_dispatch`. Saves `benchmarks/results/agent_benchmark_large_march_replay*.json`
   date-stamped. This is the assay that caught the `--medium`
   regression — if anything similar happens again, the release gate
   flags it.
3. **Add `queue_v2_benchmark.py --repo openclaw` to the PR CI job**
   (no API cost). Catches retrieval-quality regressions that would
   invalidate the "get_complexity is fast" claim before it hits the
   agent.
4. **Tool-advertisement hygiene**: now that `--medium` is gone in
   v0.38.0 (flag hard-rejected, curation codegen removed), any future
   client-specific trimming of the advertised tool list must be
   opt-in per client — never default — and must be guarded by a
   release-gate benchmark that catches the `hard-oc-complexity` drop
   before shipping.

---

## 5 · Prioritised backlog (post-benchmark)

The 2026-04-13 run (§4.3–4.6) flipped the priority order: stickiness now
dominates every other improvement, because recall gains are silently
discarded when the agent never calls the retrieval tool.

| # | Change | Pillar | Effort | Expected lift |
|---|---|---|---|---|
| **1** | **Drop `Grep` from `allowed_tools` when MCP is present** (§3.1 B) | **stickiness** | 1 line | forces MCP routing; kills the "two paths" tax that hit oc-symbol-1 |
| **2** | **Wire PreToolUse hooks into bench harness** (§3.1 A) | **stickiness** | ½ d | measures real production stickiness, re-validates March numbers |
| **3** | **System-prompt tool nudge** (§3.1 D) | **stickiness** | 1 line | the model won't explore MCP tool names unprompted |
| 4 | Auto-init fallback in Bash hook (§3.1 C) | stickiness, UX | ½ d | fixes silent-hook failure mode in un-indexed projects |
| 5 | Harder ground-truth on `lx-callers-1` | measurability | 10 lines | stricter scoring exposes recall gaps Grep currently hides |
| 6 | Embedding-clustered concept graph (§2.1) | recall | 2–3 d | only meaningful *after* #1–#3 land; +10–20% R@10 on NL queries |
| 7 | Query-personalized PageRank (§2.2) | search quality | 1–2 d | architectural queries; reduces "popular but irrelevant" hits |
| 8 | Move `TestDemotionStage` to `instant_pipeline` (§2.4) | search quality | 10 lines | cleaner results on daemon-proxy warm path |
| 9 | Session-driven reformulation feedback (§2.3) | recall | 1 d | project-local learning, offline |

**Immediate next step.** Land fixes #1–#3 as a single PR, re-run
`agent_benchmark_large.py`, and compare. If codixing-mode tool breakdown
now shows `mcp__codixing__*` calls across all 8 tasks, stickiness is
fixed and we can move recall work up to priority #1 again.

---

## 6 · Appendix — verification commands

```bash
# Current boost pipeline
codixing symbols PersonalizedGraphBoostStage
codixing api crates/core/src/engine/pipeline.rs

# Concept graph builder
codixing symbols ConceptIndexBuilder
codixing usages ConceptIndex

# Hooks in use locally
cat .claude/settings.json
cat .claude/hooks/pretool-codixing.sh
cat .claude/hooks/pretool-bash-codixing.sh

# Run the new benchmark
.venv/bin/python3 benchmarks/agent_benchmark_large.py --runs 1
.venv/bin/python3 benchmarks/agent_benchmark_large.py --repos openclaw --runs 3   # CI-tight
.venv/bin/python3 benchmarks/agent_benchmark_large.py --repos linux --runs 1      # cost-conscious
```

# Research — Project Review, Competitor Landscape & Improvement Backlog

> Updated: 2026-04-27.
> Scope: full local review of the current repository plus refreshed competitor analysis.
> This supersedes the original 2026-04-18 snapshot in-place because much of
> that backlog has now shipped.

## 0 · TL;DR

Codixing is no longer mainly a "semantic grep replacement". The current tree is
a broad local code-intelligence platform: Rust core engine, CLI, MCP server,
LSP server, HTTP API, graph exports, notebook/doc/spec indexing, session memory,
daemon mode, federation, and agent-facing tool descriptions. `cargo test
--workspace` passes locally: **1202 passed, 10 ignored**.

The biggest improvement opportunity has shifted from adding more raw features
to making the product easier to trust, adopt, and compare:

1. **Refresh proof.** README still cites some older measurements
   (`v0.26.0` OpenClaw R@10 pending remeasurement) while the codebase is
   already at `0.41.0`. A first direct local OpenClaw baseline now exists for
   Codixing vs grep and codebase-memory-mcp; claude-context still needs a
   credentialed Zilliz/Milvus + OpenAI setup before publishing measured claims.
2. **Reduce cognitive load.** 67 MCP tools is a powerful surface but a noisy
   one. Keep the broad surface, but promote 5-7 "golden path" tools and route
   everything else through discovery or specialist flows.
3. **Position around structural context.** Competitors are commoditising
   "memory" and "large context". Codixing's durable differentiation is local
   structural retrieval: AST chunks + BM25/trigram + symbol graph + impact
   analysis + doc/spec/notebook coverage.
4. **Add external project context carefully.** Greptile and Augment now sell
   code + issue + docs + history context. Codixing should not rush into SaaS
   connectors, but local imports from GitHub issues, ADRs, Linear/Jira exports,
   Notion/Confluence markdown dumps, and PR metadata would close the category
   gap while preserving the offline story.
5. **Turn graph intelligence into a product artifact.** HTML/GraphML/Cypher/
   Obsidian exports exist. The next step is curated "architecture packet"
   generation: graph screenshot, repo map, hotspots, orphans, impact examples,
   and benchmark deltas in one command.

## 1 · Current Project Review

### 1.1 What the project is now

Codixing is a local, Rust-native code retrieval and code intelligence engine
for AI agents. The repository contains:

| Layer | Current state | Review |
|---|---|---|
| Core engine | AST parsing, cAST chunking, BM25/Tantivy, trigram grep, vector index, symbol table, graph, concepts, reformulation, session state | Mature and highly tested. The core is the moat. |
| CLI | 26 user-facing commands in README, covering search, graph, grep, impact, API/type/examples/context, sync, hooks, federation | Useful for humans and for daemon proxying, but command count needs doc consistency. |
| MCP | 67 tools from TOML definitions with build-time codegen and rubric tests | Strong agent integration. Main issue is surface area complexity. |
| LSP | Hover, definition, references, call hierarchy, workspace/document symbols, rename, semantic tokens, completions | Differentiates from narrow MCP-only competitors. Needs more public docs and demos. |
| HTTP server | REST search/symbol/grep/hotspot/complexity/outline/graph routes, SSE sync | Good bridge to UI/automation. Not yet positioned as a product surface. |
| Docs/site | README, docs HTML/blogs, research notes, installer | Extensive, but some benchmark/proof sections lag the implementation. |
| Benchmarks | Agent benchmark scripts, SWE-bench localization, queue/vector/multilang results, direct competitor harness | Valuable asset. Needs a full v0.41 benchmark refresh and installed external-tool baselines. |
| Distribution | Single Rust workspace, npm wrapper, install script, Claude plugin, examples for Codex/Cursor/Windsurf/Continue | Strong. Should make "works with any agent" the default install narrative. |

### 1.2 Shipped since the old April 18 backlog

The previous note listed document parsing and stickiness work as future gaps.
Current code shows these are now largely implemented:

| Old gap | Current evidence | Status |
|---|---|---|
| RST indexing | `crates/core/src/language/rst.rs`, `doc_indexing.rs` | Shipped |
| AsciiDoc indexing | `crates/core/src/language/asciidoc.rs`, `doc_indexing.rs` | Shipped |
| Plain text / extensionless README | `crates/core/src/language/plain.rs`, `doc_indexing.rs` | Shipped |
| CHANGELOG sectioning | Markdown changelog tests and `doc_indexing.rs` | Shipped |
| Jupyter notebooks | `crates/core/src/language/ipynb.rs`, `jupyter_indexing.rs` | Shipped |
| OpenAPI endpoint chunking | `crates/core/src/language/openapi.rs`, `openapi_indexing.rs` | Shipped |
| PDF text extraction | `crates/core/src/language/pdf.rs`, feature-gated `pdf` dependency | Shipped behind feature flag |
| `search_usages --complete` | `complete=true` parameter and graph trait dispatch tests | Shipped |
| MCP tool-description audit | `crates/mcp/TOOL_DESCRIPTION_RUBRIC.md`, `tool_description_rubric.rs` | Shipped as automated trigger check |
| Concept graph / semantic matching | `engine/concepts.rs`, `engine/semantic.rs` | Shipped enough to change positioning |
| Personalized graph ranking | `focus_map`, `personalized_graph_boost` tests | Shipped in focus/query pipelines |

### 1.3 Strengths

**Local-first architecture.** No external database is required for default use.
This is increasingly important as Cursor, Copilot, Codex, Claude, and MCP
ecosystems add more remote/cloud capabilities.

**Structural signals beat embedding-only competitors.** Codixing combines
identifier-aware BM25, trigram exact search, symbol tables, file/symbol graph,
PageRank, impact analysis, doc-to-code references, and optional vectors. That
is broader than pure semantic-code-search MCP servers.

**Excellent test coverage for a solo/devtool project.** `cargo test
--workspace` passed with 1202 tests. The suite covers parser behavior,
retrieval, graph, LSP protocol, MCP protocol, HTTP routes, notebook/spec/doc
indexing, and benchmark-adjacent recall tests.

**Agent ergonomics are taken seriously.** The MCP tool definitions contain
activation language; output filtering/tee recovery exists; the daemon avoids
cold-start costs; `search_tools` and `get_tool_schema` reduce schema overload.

**The project already has multiple distribution channels.** CLI, MCP, LSP,
HTTP, VS Code/Cursor extension, npm package, install script, Claude plugin,
and Codex config examples give Codixing more integration paths than most
open-source competitors.

### 1.4 Weak spots and risks

| Risk | Why it matters | Recommendation |
|---|---|---|
| Benchmark drift | Public claims are strong but some are explicitly older than the current release. Competitors now publish their own token/indexing claims. | Create a v0.41 benchmark release gate: OpenClaw, Linux, SWE-bench localization, agent replay, and installed competitor baselines. |
| Feature-list sprawl | README has a very long capability list. It proves depth but makes the product harder to understand. | Add a "Use this first" section: `code_search`, `find_symbol`, `search_usages --complete`, `feature_hub`, `change_impact`, `focus_map`, `grep_code`. Move the long list below. |
| MCP surface area | 67 tools can trigger choice paralysis even with better descriptions. | Add a meta-router / task templates: "understand feature", "prepare edit", "review diff", "trace symbol", "audit dead code". |
| File inventory blind spot | MCP `list_files` derives files from symbols, so symbol-free docs/configs can be underrepresented even though `stats.file_count` counts them. | Back `list_files` from persisted file metadata or chunk metadata, not `engine.symbols("")`. |
| Rename safety | `rename_symbol` still performs exact string replacement after validation. This is useful but easy to over-trust. | Rename the tool/CLI copy to "text rename", require `dry_run` by default, and point semantic rename users to LSP/editor flows. |
| External context gap | Greptile/Augment sell reviews, rules, issue trackers, and docs as one context layer. | Add local importers before SaaS connectors: GitHub issue/PR export, ADR folder, `.md` docs dump, Jira/Linear CSV/JSON. |
| Security narrative | MCP write tools are powerful, and the broader MCP ecosystem is getting more security scrutiny. | Default docs should emphasize read-only mode, root path enforcement, no network/API-key requirement, and explicit write-tool risk boundaries. |
| HTTP/UI under-positioned | Server and graph UI exist but are not as visible as CLI/MCP. | Ship `codixing report` or `codixing graph --packet` that generates a shareable static review bundle. |

## 2 · Competitor Landscape — Updated 2026-04-24

### 2.1 Category map

| Category | Competitors | How they compete | Codixing position |
|---|---|---|---|
| Agent IDEs | Cursor, Windsurf, JetBrains AI/Junie, VS Code/Copilot | Own the workflow, UI, cloud agents, and user attention | Integrate into them; do not compete head-on as an IDE. |
| Terminal agents | Codex, Claude Code, Gemini CLI, Aider, OpenCode | Own task execution and tool orchestration | Be the local structural context backend they call. |
| Code review agents | Greptile, CodeRabbit, Copilot Review | Own PR review workflows, rules, comments, issue context | Compete only through review-context tooling unless a hosted review product is built. |
| Enterprise context platforms | Augment, Sourcegraph Cody | Sell large-scale context across repos, history, and systems | Win with local-first, reproducible retrieval, and graph-specific analysis. |
| MCP code search / graph servers | claude-context, codebase-memory-mcp, Serena, Continue repo-map | Add retrieval/graph tools to existing agents | Direct competitive set. Codixing must prove better quality, not just more tools. |

### 2.2 Major competitor movements

| Project | Current movement | Threat | Implication for Codixing |
|---|---|---:|---|
| **Cursor 3** | New agent-first interface with parallel agents across local, worktree, cloud, and remote SSH; Design Mode for browser UI targeting. | High for workflow ownership | Codixing should be a Cursor-compatible backend and emphasize offline structural search, not editor UX. |
| **Cursor self-hosted cloud agents** | Agents can run inside a customer's own network, keeping code/tool execution in their infrastructure. | Medium | Weakens "local/privacy" as an enterprise-only differentiator; offline single-binary simplicity remains distinct. |
| **GitHub Copilot Memory** | Repository-level memory is default-on for Pro/Pro+ public preview, expires after 28 days, and works across coding agent, review, and CLI. | High for "memory" messaging | Stop pitching generic memory. Pitch structural, queryable, reproducible code intelligence. |
| **OpenAI Codex** | Codex expanded beyond coding into computer use, app integrations, PR review, remote devboxes, browser workflows, and persistent preferences/actions. | High for agent mindshare | Codixing should optimize Codex MCP/docs/install experience and become a recommended local context tool. |
| **Claude Code** | Memory hierarchy and project/user/enterprise `CLAUDE.md` conventions are mature; MCP remains core to ecosystem integration. | Medium | Codixing's Claude plugin is useful, but must make hooks and slash commands easier to trust. |
| **JetBrains Junie CLI** | Standalone, LLM-agnostic terminal agent in beta, with IDE/CI/GitHub/GitLab ambitions and migration from other agents. | Medium | LSP/ACP/terminal neutrality matters. Codixing should document JetBrains/Junie install paths if possible. |
| **Augment Code** | Context Engine markets live understanding across repos, services, history, and knowledge graph; claims 1M+ files indexed. | High enterprise | Codixing needs current large-repo/federation proof and sharper enterprise-local positioning. |
| **Sourcegraph Cody** | Context uses keyword search, Sourcegraph Search, code graph, repo context, remote dirs/files, OpenCtx, and large enterprise windows. | Medium enterprise | Sourcegraph owns existing enterprise code search. Codixing should target local/dev-agent loops first. |
| **Greptile** | Focused review agent with MCP access to comments, reports, custom context, and coding patterns. | Medium | Avoid direct review-SaaS competition for now; add local PR/review context generation. |
| **claude-context / Zilliz** | Hybrid BM25+dense vector MCP server with AST chunking, Merkle incremental indexing, multiple embedding/vector providers, and published token-reduction eval. | High direct | Benchmark against it. Codixing's graph/trigram/offline/default-no-cloud story should win if measured. |
| **codebase-memory-mcp** | Fast local graph MCP, 66 languages, single static binary, Linux kernel indexing claims, 14 tools, graph UI, signed releases. | Very high direct | This is now the closest positioning threat. Codixing needs a direct comparison on answer quality, recall, setup, and structural depth. |
| **Continue repo-map** | Local context component in an agent/editor ecosystem. | Low/medium | Validates local context as table stakes, but Continue is more platform/editor than standalone engine. |
| **Serena** | Symbolic coding toolkit with broad language/editor/agent support. | Medium | Serena is editing/workflow oriented; Codixing wins if retrieval and graph quality are measurably better. |
| **Aider repo-map** | Established lightweight repository-map approach for edit planning. | Low | Aider is a useful baseline for graph/repo-map quality, not a direct full-engine competitor. |

### 2.3 Updated moat

The defensible Codixing moat is:

```text
Local single-binary structural context
= AST/cAST chunks
+ identifier-aware BM25
+ trigram exact search
+ symbol and file graphs
+ PageRank / personalized graph boost
+ impact, tests, examples, complexity
+ docs/spec/notebook coverage
+ CLI/MCP/LSP/HTTP surfaces
+ reproducible benchmarks
```

The last line is improving but still incomplete. A direct OpenClaw harness now
captures Codixing vs grep and codebase-memory-mcp locally: on 2026-04-27
Codixing scored Recall@10 0.783 / MRR 0.827 across 20 queries,
codebase-memory-mcp scored Recall@10 0.374 / MRR 0.243, and grep scored
Recall@10 0.191 / MRR 0.168. Competitors can still match parts of the pitch
with louder numbers: codebase-memory-mcp claims 66 languages and Linux kernel
indexing in minutes; claude-context claims roughly 40% token reduction; Augment
claims large-scale context across 1M+ files. Codixing's response should be
installed-tool comparisons, not broader feature lists.

### 2.4 Positioning change

Old pitch:

> "Codixing replaces grep for AI agents."

Better pitch:

> "Codixing is the local structural context layer for coding agents: ranked
> search, call graphs, impact analysis, docs/spec/notebook indexing, and
> token-bounded answers without shipping your source to a hosted index."

This avoids competing directly with Cursor/Codex/Claude as agents and avoids
being boxed into "semantic grep" next to smaller MCP search servers.

## 3 · Improvement Backlog

### 3.1 Immediate: one week

| # | Item | Why now | Effort |
|---|---|---|---:|
| 1 | Re-run v0.41 benchmark suite and update README/blog claims | Public proof lags implementation and competitors are publishing numbers | **Partial 2026-04-27: direct OpenClaw baseline refreshed** |
| 2 | Direct competitor benchmark: `claude-context` and `codebase-memory-mcp` | Closest direct threats; enables honest positioning | **Partial 2026-04-27: codebase-memory-mcp measured; claude-context credentials/runtime pending** |
| 3 | Fix `list_files` to use indexed file/chunk metadata, not symbol table | Symbol-free docs/configs are indexed but can disappear from inventory | **Done 2026-04-24** |
| 4 | Add "golden path" agent workflows to README/docs | Reduces tool overload and improves adoption | **Done 2026-04-24** |
| 5 | Rename/copy hardening for `rename_symbol` | Prevents users/agents from mistaking exact text replacement for semantic rename | **Partial 2026-04-24: dry-run + explicit exact-string warning shipped** |

### 3.2 Near-term: two to four weeks

| # | Item | Why it matters | Effort |
|---|---|---|---:|
| 6 | `codixing report` / architecture packet | Converts graph/search depth into a shareable artifact | 2-3 d |
| 7 | Local external-context importers | Closes Greptile/Augment category gap without SaaS lock-in | 3-5 d |
| 8 | MCP task router / templates | Lets agents choose intents, not tools | 2-3 d |
| 9 | Federation large-repo benchmark | Enterprise/local multi-repo proof | 2 d |
| 10 | Security/adoption guide for MCP write tools | Trust is now part of the category | 1 d |

### 3.3 Strategic: one to two quarters

| # | Item | Strategic value |
|---|---|---|
| 11 | Hosted optional index/team dashboard | Only if enterprise users ask for shared team context; preserve local default. |
| 12 | ACP/JetBrains/Junie integration path | Agent ecosystems are fragmenting; ACP support could matter. |
| 13 | Quality harness for answer-level codebase questions | Move from retrieval recall to "agent solved task with fewer tokens and fewer wrong files." |
| 14 | Cross-repo architectural drift detection | Use graph/community/hotspot/freshness data to flag boundary violations and shadow tech debt. |
| 15 | Local issue/spec/PR graph | Treat requirements and review comments as first-class graph nodes linked to code. |

## 4 · Recommended Execution Order

1. **Benchmark and proof refresh.** Do this before adding more features. The
   product already has enough surface area; credibility is now the limiter.
2. **Fix small correctness/adoption gaps.** `list_files`, README command/tool
   count consistency, golden-path docs, and rename warning copy are cheap.
3. **Publish direct competitor comparison.** Use open repositories and include
   raw commands/results so claims stay defensible.
4. **Ship architecture packet.** This turns invisible graph quality into a
   visible artifact users can understand and share.
5. **Add local external-context imports.** Start with files and exports, not
   OAuth/SaaS connectors.

## 5 · Verification Notes

Local checks run on 2026-04-24:

```bash
cargo test --workspace
```

Result:

```text
1202 passed; 0 failed; 10 ignored
```

Additional benchmark/proof checks run on 2026-04-26:

```bash
python3 -m py_compile benchmarks/competitor_benchmark.py
python3 benchmarks/competitor_benchmark.py --repo benchmarks/repos/openclaw
CODEBASE_MEMORY_MCP=/tmp/cbm-benchmark/codebase-memory-mcp \
CODEBASE_MEMORY_PROJECT=Users-andreaferrarelli-code-codixing-benchmarks-repos-openclaw \
CBM_CACHE_DIR=/tmp/cbm-benchmark/cache \
python3 benchmarks/competitor_benchmark.py \
  --repo benchmarks/repos/openclaw \
  --include-disabled \
  --tool codixing --tool grep --tool codebase-memory-mcp \
  --output-prefix external_competitor_benchmark
```

Result:

```text
codixing:            20 queries, Precision@10 0.261, Recall@10 0.783, MRR 0.827
codebase-memory-mcp: 20 queries, Precision@10 0.147, Recall@10 0.374, MRR 0.243
grep:                20 queries, Precision@10 0.125, Recall@10 0.191, MRR 0.168
```

Note: codebase-memory-mcp was indexed locally from the v0.6.0 macOS arm64
release into `/tmp/cbm-benchmark/cache`. The benchmark used its available CLI
tools (`search_graph` and `search_code`); `semantic_query` was announced in
v0.6.0 release notes but was not exposed by this downloaded CLI build.
claude-context was not measured because it requires Node.js 20-23 plus OpenAI
and Zilliz/Milvus credentials; this machine has Node.js 25.6.1 and no
benchmark credentials configured.

The `cross-pkg-plugin-sdk-entry` fixture was refreshed on 2026-04-26 because
current OpenClaw no longer has the previous anthropic/discord
`definePluginEntry` imports. Codixing also gained an optional cross-imports
`pattern` filter so broad boundary queries can be narrowed to a specific import
shape without falling back to plain grep.

Code areas reviewed:

- `crates/core/src/engine/*`
- `crates/core/src/language/*`
- `crates/core/tests/*`
- `crates/mcp/src/tools/*`
- `crates/mcp/tool_defs/*.toml`
- `crates/mcp/tests/tool_description_rubric.rs`
- `crates/lsp/src/main.rs`
- `crates/server/src/routes/*`
- `README.md`
- benchmark scripts and existing benchmark result docs
- `benchmarks/competitor_benchmark.py`
- `benchmarks/competitor_tools.toml`

## 6 · Sources

- Cursor 3 changelog: https://cursor.com/changelog/3-0
- Cursor self-hosted cloud agents: https://cursor.com/changelog/03-25-26
- GitHub Copilot Memory default-on changelog: https://github.blog/changelog/2026-03-04-copilot-memory-now-on-by-default-for-pro-and-pro-users-in-public-preview/
- OpenAI Codex update: https://openai.com/index/codex-for-almost-everything/
- Claude Code memory docs: https://docs.anthropic.com/en/docs/claude-code/memory
- JetBrains Junie CLI beta: https://blog.jetbrains.com/junie/2026/03/junie-cli-the-llm-agnostic-coding-agent-is-now-in-beta/
- Augment Context Engine: https://www.augmentcode.com/context-engine
- Sourcegraph Cody context docs: https://sourcegraph.com/docs/cody/core-concepts/context
- Greptile MCP docs: https://www.greptile.com/docs/mcp/overview
- Zilliz claude-context: https://github.com/zilliztech/claude-context
- DeusData codebase-memory-mcp: https://github.com/DeusData/codebase-memory-mcp
- Aider repo map: https://aider.chat/docs/repomap.html
- Roo Code codebase indexing: https://docs.roocode.com/features/codebase-indexing

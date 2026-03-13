# Codixing — GTM Strategy & Development Roadmap

**Date:** 2026-03-13 (revised)
**Informed by:** [Competitive Landscape Analysis](./competitive-landscape.md) · [HydraDB Deep Dive](./hydradb-competitive-analysis.md) · [context-mode](https://github.com/mksglu/context-mode) analysis

---

## Executive Summary

Codixing's competitive advantage is not any single feature — it's the **combination** of local-first architecture, multi-interface exposure (CLI + MCP + LSP + REST), hybrid retrieval (BM25 + embeddings + graph), and deep structural analysis (impact prediction, complexity, call graphs) in a single Rust binary. No competitor ships all of these together.

The GTM strategy exploits three market openings simultaneously:

1. **Sourcegraph's retreat from individuals/small teams** (enterprise-only since July 2025)
2. **MCP becoming the standard interface** for code context (Linux Foundation, Dec 2025)
3. **AI coding tools needing better context backends** (Continue.dev deprecated built-in indexing for MCP)

The development roadmap carves a defensible niche as **"the code context engine"** — not an IDE, not a review tool, not a memory layer — the infrastructure that powers all of them.

---

## Part 1: Positioning

### Current (Implicit)

> "Ultra-fast code retrieval engine for AI agents"

This is accurate but generic. It positions Codixing as a faster grep, which invites comparison with ripgrep, Sourcegraph, and every IDE's built-in search.

### Proposed

> **"The code context engine — structural intelligence for every AI coding tool"**

This positions Codixing as **infrastructure**, not a product. The framing shift:

| Old framing | New framing |
|---|---|
| Code search tool | Code context engine |
| Competes with Sourcegraph | Powers AI coding tools |
| Feature: "fast search" | Capability: "structural intelligence" |
| User: developer | User: developer + tool builder |

### Why This Works

- **"Context engine"** aligns with the hottest category in AI infra (Gartner: "context engineering is foundational")
- **"Structural intelligence"** differentiates from flat search (grep, embeddings-only tools)
- **"Every AI coding tool"** positions as a platform, not a point solution — Cursor, Continue, Aider, Claude Code are all potential consumers, not competitors
- Avoids the trap of competing with $29B Cursor or $2.6B Sourcegraph on their turf

---

## Part 2: GTM Strategy

### Target Segments (in priority order)

#### Segment 1: Claude Code Power Users (Immediate)

**Who:** Developers already using Claude Code who hit context limits or want better code navigation.

**Why first:** Zero-friction adoption. Codixing is already an MCP server — `claude mcp add` is one command. These users feel the pain of grep-based code exploration daily.

**Motion:**
- Publish benchmarks showing Codixing vs. default grep/find (the b2Vec2 case study already exists — productize it)
- Write "Claude Code + Codixing" setup guide, submit to Anthropic's MCP server directory
- Create 2-minute demo video: "Watch Claude Code solve a bug 3× faster with Codixing"
- Target r/ClaudeAI, Claude Code Discord, HackerNews

**Success metric:** 500 weekly active MCP connections within 3 months

#### Segment 2: Continue.dev Users (Month 1-3)

**Who:** Developers using Continue.dev (31.8K GitHub stars) who lost `@codebase` when it was deprecated in favor of MCP.

**Why:** Continue explicitly tells users to find an MCP server for codebase context. Codixing is the best-fit replacement. This is a **distribution channel handed to us**.

**Motion:**
- Write an integration guide: "Replace @codebase with Codixing MCP in Continue.dev"
- Submit PR to Continue.dev docs adding Codixing as a recommended MCP code context provider
- Engage Continue.dev team directly (they're 19 people, YC S23 — accessible)

**Success metric:** Listed in Continue.dev's official docs/recommended tools within 2 months

#### Segment 3: Cursor / Windsurf Users Who Want More (Month 2-4)

**Who:** Developers using Cursor or Windsurf who need deeper code understanding than the built-in indexing provides (call graphs, impact analysis, complexity).

**Why:** Cursor's indexing is cloud-dependent and limited to search. It doesn't expose call graphs, can't predict impact of changes, and can't compute complexity. Codixing fills these gaps as an MCP backend.

**Motion:**
- "What Cursor's indexing can't tell you" blog post — demonstrate call graph queries, impact prediction, and complexity analysis that Cursor doesn't offer
- Cursor MCP configuration guide (Cursor already supports MCP servers)
- Target Cursor's community forums and r/cursor

**Success metric:** 200 Cursor users with Codixing MCP configured within 4 months

#### Segment 4: Individual Developers / Small Teams (Month 3-6)

**Who:** The users Sourcegraph abandoned when it went enterprise-only in July 2025.

**Why:** Sourcegraph was the gold standard for code intelligence. Its mid-market ($19-59/user/month) is now unserved. These users want code navigation, cross-repo search, and symbol intelligence — all things Codixing does, locally and for free.

**Motion:**
- "Sourcegraph for your laptop" positioning in a dedicated landing page
- Feature comparison table: Codixing vs. Sourcegraph (local, free, Rust-native, MCP-native vs. cloud, enterprise-only, $$$)
- SEO play: target "Sourcegraph alternative" and "free code intelligence" keywords

**Success metric:** 1,000 GitHub stars, 300 weekly CLI users within 6 months

### Distribution Channels

| Channel | Priority | Action |
|---|---|---|
| **MCP server directories** | P0 | List on Anthropic MCP directory, mcp.so, awesome-mcp-servers |
| **Continue.dev docs** | P0 | Submit PR as recommended code context provider |
| **Claude Code ecosystem** | P0 | Benchmark posts, setup guides, CLAUDE.md examples |
| **HackerNews** | P1 | "Show HN" launch post with b2Vec2 benchmark story |
| **VS Code Marketplace** | P1 | Publish the existing extension (editors/vscode/) |
| **Homebrew / cargo install** | P1 | Already exists — ensure discoverability |
| **Cursor forums** | P2 | Integration guides, feature comparison |
| **Reddit** (r/ClaudeAI, r/cursor, r/neovim, r/programming) | P2 | Case studies and demos |
| **Dev.to / blog** | P2 | Technical deep dives on hybrid retrieval, call graphs |

### What We Don't Do

- **Don't build a cloud service yet.** Local-first is the differentiator. Cloud adds operational burden and puts us in Sourcegraph's arena. Revisit after 2K active users.
- **Don't build an IDE.** Cursor has $29B valuation and millions of users. We're the engine, not the car.
- **Don't go horizontal.** AI memory (Mem0, Zep, Letta) is a different category with $65M+ in funding. Stay vertical on code.
- **Don't chase enterprise sales.** No sales team, no SOC 2, no SSO. Win developers first, enterprise follows.

---

## Part 3: Revised Development Roadmap

The roadmap is restructured around **defensibility** — each phase deepens the moat in areas where competitors can't easily follow.

### Moat Analysis: What's Defensible?

| Capability | Competitors who match | Defensibility |
|---|---|---|
| Fast BM25 search | Many (ripgrep, Sourcegraph, etc.) | Low — table stakes |
| Semantic embeddings | Many (Cursor, Sourcegraph, CodeGrok) | Low — commodity |
| Call graph + PageRank | Few (code-graph-mcp, Greptile) | Medium |
| Impact prediction | **None** | **High** |
| Cyclomatic complexity | **None** in MCP space | **High** |
| Multi-interface (CLI+MCP+LSP+REST) | **None** | **High** |
| Local-first, single binary | Few (codebase-memory-mcp) | Medium |
| Hybrid BM25+vector+graph retrieval | **None** — all three together | **High** |
| Temporal code context (git-aware) | **None** | **High** (if built) |
| Session-aware retrieval | context-mode has flat session tracking; **none** have structural session intelligence | **High** |

The roadmap prioritizes **high-defensibility capabilities** that compound over time.

### Phase 12: Distribution & Polish (Next — Weeks 1-4)

**Goal:** Make Codixing trivially easy to discover and adopt. Zero new engine features — purely GTM execution.

| Task | Detail | Why |
|---|---|---|
| MCP directory listings | Submit to Anthropic MCP directory, mcp.so, awesome-mcp-servers, glama.ai | Discovery — users can't adopt what they can't find |
| VS Code Marketplace publish | Package and publish `editors/vscode/` as a real extension | Largest IDE market — reduces adoption friction to one click |
| Continue.dev integration PR | Write guide + submit docs PR | Captures the @codebase migration wave |
| Cursor MCP guide | Step-by-step `.cursor/mcp.json` setup | Second-largest AI IDE |
| One-command install validation | Ensure `curl | sh` works flawlessly on macOS ARM, macOS x86, Linux x86 | Broken install = lost user |
| README rewrite | Lead with positioning ("code context engine"), move benchmarks up, add 30-second GIF | First impression matters; current README is feature-list style |

### Phase 13: Temporal + Session-Aware Context (Weeks 4-8)

**Goal:** Build two capabilities no competitor has — **git-aware retrieval** that understands how code evolves, and **session-aware retrieval** that gets smarter the longer an agent works.

**Inspiration:** [context-mode](https://github.com/mksglu/context-mode) (4K stars) validates that session continuity is a top pain point — agents lose track of what they were doing after conversation compaction. Context-mode solves this with flat SQLite event logging. Codixing can do something far more powerful: feed session activity into the *structural retrieval pipeline* so search results are influenced by what the agent has been working on.

#### 13a: Session-Aware Retrieval (Weeks 4-6)

The core insight from context-mode: **what the agent touched this session should influence retrieval**. But where context-mode stores flat events, Codixing can propagate session signals through the code graph.

| Feature | Detail | MCP Tool |
|---|---|---|
| **Session event tracking** | Track every file read, symbol lookup, edit, and search in an in-memory session log (SQLite-backed for persistence across compactions) | Automatic — all existing tools emit events |
| **Session-boosted search** | Files/symbols the agent recently read or edited get a retrieval boost. If you've been editing `auth.rs` for 20 minutes, a search for "handler" prioritizes auth-related handlers. | Enhancement to `code_search` |
| **Graph-propagated session context** | Session boost propagates through the call graph. If the agent edited `auth.rs`, its callers and callees also get a mild boost — the agent is probably working in that subgraph. | Enhancement to `code_search` |
| **Session summary** | On-demand snapshot of what the agent has explored, edited, and searched this session — structured by module/subsystem, not flat chronological. Survives compaction. | `get_session_summary` |
| **Session-aware explain** | When explaining a symbol, highlight connections to other symbols the agent has already seen — "you looked at `verify_token` earlier; `authenticate` calls it here." | Enhancement to `explain` |
| **Progressive focus** | As the session deepens in one area, retrieval automatically narrows. After 5+ interactions with auth code, search results are implicitly scoped. Agent can reset with `session_reset_focus`. | `session_reset_focus` |

**Why this is better than context-mode's approach:**
- Context-mode stores flat events and rebuilds a text summary. Codixing propagates session signals through the **code graph** — structurally aware, not just chronological.
- Context-mode's progressive throttling is a blunt instrument (reduce results after N calls). Codixing's progressive focus is semantic — it narrows based on *what area* you're in, not *how many calls* you've made.
- Context-mode is a wrapper around other tools. Codixing's session awareness is *inside* the retrieval engine — it changes ranking, not just output formatting.

#### 13b: Temporal Code Context (Weeks 5-8)

No existing tool (Sourcegraph, Cursor, Greptile, any MCP tool) integrates git history into code retrieval.

| Feature | Detail | MCP Tool |
|---|---|---|
| **Git-aware search boost** | Recent commits boost relevance — a function edited yesterday is more relevant than one untouched for 2 years | Enhancement to `code_search` |
| **Change velocity scoring** | Rank symbols/files by how frequently they change (hotspots) | `get_hotspots` |
| **"What changed" queries** | "What changed in the auth module since last release?" — combines git log + structural awareness | `search_changes` |
| **Blame-aware context** | When explaining a symbol, include who last modified it and the commit message | Enhancement to `explain` |
| **Diff-aware impact** | Given a branch, predict what else needs to change based on the diff + historical co-change patterns | Enhancement to `predict_impact` |

**Why Phase 13 as a whole is defensible:** Both session-awareness and temporal context require deep integration with the AST parser, graph engine, and retrieval pipeline. Context-mode proves the demand (4K stars) but solves it at the wrong layer — flat event logging can't understand code structure. Codixing's graph-propagated approach is architecturally impossible for tools that don't have a code graph. This is the moat.

### Phase 14: Language Breadth (Weeks 6-10)

**Goal:** Close the language gap with codebase-memory-mcp (64 languages) while maintaining Codixing's structural depth advantage.

| Tier | Languages to Add | Priority |
|---|---|---|
| **Tier 3 expansion** | Lua, Dart, Elixir, Haskell, OCaml, R | High — covers Flutter, game dev, ML, FP ecosystems |
| **Tier 4 (parse-only, no graph)** | YAML, TOML, JSON, Markdown, SQL, HCL, Dockerfile | Medium — config files matter for DevOps/IaC users |
| **Tier 5 (via tree-sitter grammars)** | Bash, Perl, Objective-C, MATLAB | Low — long tail |

**Approach:** tree-sitter has grammars for 100+ languages. Tier 4 (config files) only needs parsing and BM25 — no import extraction or call graph. This is low-effort, high-coverage work.

**Target:** 30+ languages with structural support, 50+ with basic parse+search. This eliminates the "but does it support X?" objection.

### Phase 15: Retrieval Quality & Benchmarks (Weeks 8-12)

**Goal:** Publish formal, reproducible benchmarks that establish Codixing as the quality leader. This is both a GTM and defensibility play.

| Task | Detail |
|---|---|
| **Public benchmark suite** | Curated set of code search queries across 5 real-world repos (varying size, language, style). Open-source the harness so anyone can reproduce. |
| **Head-to-head comparison** | Codixing vs. ripgrep, Sourcegraph (local), codebase-memory-mcp, Aider's repo-map on the same queries. Measure: MRR@10, Recall@K, tokens consumed, latency. |
| **Publish results** | Blog post + HackerNews + Reddit. Let the numbers speak. |
| **Continuous regression suite** | Every PR runs the quality suite. If MRR drops, the build fails. This prevents quality regressions as new features land. |

**Why this matters for GTM:** Developers trust benchmarks. A reproducible, fair comparison that shows Codixing winning on quality + token efficiency is the most powerful marketing asset possible.

### Phase 16: Visualization (Weeks 10-14)

**Goal:** Add a visual layer to the existing Graph Atlas to match/exceed Axon and fill the gap left by CodeSee.

The server already has `/graph/view` with 3D orbit, call graph overlay, and topology groups. Extend it:

| Feature | Detail |
|---|---|
| **Symbol-level drill-down** | Click a file node → expand to see its symbols → click a symbol → see callers/callees |
| **Change heatmap overlay** | Color nodes by recent change frequency (from Phase 13's hotspot data) |
| **Impact blast radius** | Select a symbol → highlight all transitively affected files in red/orange/yellow |
| **Shareable snapshots** | Export current view as SVG/PNG for documentation, PRs, onboarding |
| **Embeddable widget** | `<iframe>` snippet for embedding in docs/wikis |

**Why now:** Visualization is the most viral feature. A beautiful graph visualization gets shared on Twitter/Reddit/HN far more than a benchmark table. It's also the feature most requested by developers who want to understand unfamiliar codebases.

### Phase 17: Team Features & Hosted API (Weeks 14-20)

**Goal:** Unlock team adoption and revenue without abandoning local-first.

| Feature | Detail |
|---|---|
| **Shared index** | Team members point at a shared `.codixing/` on a network volume or S3 bucket. One person indexes, everyone queries. |
| **Codixing Cloud API** | Hosted REST API where teams push their repo → get an API key → query from any tool. Priced at $19/user/month (Sourcegraph's abandoned tier). |
| **Team onboarding mode** | New team members run `codixing onboard` → get a generated guide with architecture overview, key entry points, and "start here" pointers (builds on existing `generate_onboarding`). |
| **Multi-repo graph** | Connect call graphs across repos (the `--also` flag exists; extend to show cross-repo callers/callees). |

**Pricing model:**

| Tier | Price | What |
|---|---|---|
| **Open Source** | Free | CLI + MCP + LSP, local-only, unlimited |
| **Pro** | $19/user/month | Cloud API, shared indexes, priority support |
| **Team** | $49/user/month | Multi-repo, admin dashboard, SSO |

This mirrors the market gap: Sourcegraph starts at enterprise pricing, Cursor is $20-40/user but bundles an IDE, and the MCP-native tools are all free/hobby. Codixing Pro at $19/user fills the gap.

---

## Part 4: Competitive Response Playbook

### If Cursor improves its built-in indexing

**Risk:** High. Cursor has $29B valuation and millions of users.
**Response:** Emphasize what Cursor's built-in can't do: call graphs, impact prediction, complexity, temporal context. Position as "the context layer Cursor doesn't have." Cursor consuming Codixing via MCP is the ideal outcome.

### If Sourcegraph releases a free tier

**Risk:** Medium. Sourcegraph has deep technology but is focused on enterprise.
**Response:** Emphasize local-first (no data leaves your machine), single binary simplicity, and MCP-native. Sourcegraph's MCP server exposes fewer tools than Codixing's 24+.

### If codebase-memory-mcp gains traction

**Risk:** Medium. Most architecturally similar competitor.
**Response:** Benchmark head-to-head on retrieval quality. Codixing's hybrid retrieval (BM25 + vector + graph) should outperform single-approach tools. Emphasize LSP integration (they don't have it), visualization (they don't have it), and structural analysis depth.

### If Continue.dev builds their own code context

**Risk:** Medium. They deprecated @codebase but could rebuild.
**Response:** Partnership > competition. Codixing as the recommended engine lets Continue focus on IDE UX while Codixing handles the hard retrieval problem. Propose formal integration.

### If context-mode expands into code intelligence

**Risk:** Low. Context-mode (4K stars, ELv2 license) solves context window management, not code understanding. Its TypeScript/SQLite stack can't replicate Codixing's Rust-native AST parsing, call graphs, or hybrid retrieval.
**Response:** Complementary positioning. Context-mode wraps tool outputs; Codixing generates intelligent code context. They can coexist — and Codixing absorbing session-aware retrieval into its own engine makes the "use both" argument weaker over time. The session summary feature in Phase 13a directly subsumes context-mode's value for code-specific workflows.

### If a well-funded startup enters the MCP code intelligence space

**Risk:** Low-medium (inevitable eventually).
**Response:** Ship faster. The roadmap above front-loads the hardest-to-replicate features (temporal context, hybrid retrieval quality, multi-interface exposure). By the time a new entrant appears, Codixing should have 6+ months of compounding quality improvements.

---

## Part 5: Success Metrics

### 3-Month Targets

| Metric | Target |
|---|---|
| GitHub stars | 1,000 |
| Weekly MCP connections | 500 |
| Listed in MCP directories | 3+ (Anthropic, mcp.so, awesome-mcp-servers) |
| Listed in Continue.dev docs | Yes |
| VS Code Marketplace installs | 200 |
| Languages supported (structural) | 25+ |

### 6-Month Targets

| Metric | Target |
|---|---|
| GitHub stars | 3,000 |
| Weekly MCP connections | 2,000 |
| Weekly CLI users | 500 |
| HackerNews front page | 1+ post |
| Published benchmark suite | Yes |
| Temporal code context shipped | Yes |

### 12-Month Targets

| Metric | Target |
|---|---|
| GitHub stars | 8,000 |
| Monthly active users (all interfaces) | 5,000 |
| Codixing Cloud paying customers | 50 teams |
| MRR | $15K |
| Languages supported | 50+ |
| Formal partnerships | 2+ (Continue.dev, Aider, or similar) |

---

## Part 6: Niche Definition

### What Codixing Is

**The code context engine.** Infrastructure that gives any AI coding tool — Claude Code, Cursor, Continue, Aider, custom agents — deep structural understanding of any codebase.

### What Codixing Is Not

- **Not an IDE** (Cursor, Windsurf own that)
- **Not a code review tool** (Greptile, Qodo own that)
- **Not a general AI memory layer** (Mem0, Zep, Letta own that)
- **Not an enterprise platform** (Sourcegraph owns that)

### The Defensible Niche

Codixing occupies the intersection of three properties that no competitor combines:

```
    ┌─────────────────────────────┐
    │     LOCAL-FIRST              │
    │   (no cloud dependency,     │  ← Sourcegraph, Cursor, Greptile
    │    data never leaves        │     are all cloud-dependent
    │    the machine)             │
    └──────────┬──────────────────┘
               │
    ┌──────────▼──────────────────┐
    │   STRUCTURAL INTELLIGENCE   │
    │  (call graphs, PageRank,    │  ← ripgrep, codebase-memory-mcp,
    │   impact prediction,        │     CodeGrok are flat search
    │   complexity, temporal)     │
    └──────────┬──────────────────┘
               │
    ┌──────────▼──────────────────┐
    │  SESSION-AWARE RETRIEVAL    │
    │  (gets smarter as the       │  ← context-mode has flat event logs;
    │   agent works; graph-       │     no one has graph-propagated
    │   propagated focus)         │     session intelligence
    └──────────┬──────────────────┘
               │
    ┌──────────▼──────────────────┐
    │   MULTI-INTERFACE ENGINE    │
    │  (CLI + MCP + LSP + REST,   │  ← All competitors are single-interface
    │   usable by any tool)       │
    └─────────────────────────────┘
```

**No competitor occupies this intersection.** Sourcegraph has structural intelligence but is cloud/enterprise. Context-mode has session tracking but no code understanding. Ripgrep and codebase-memory-mcp are local but flat. Cursor is an IDE, not an engine. This four-way intersection is the niche to own and defend.

---

## Sources

- [Competitive Landscape Analysis](./competitive-landscape.md)
- [HydraDB Deep Dive](./hydradb-competitive-analysis.md)
- [Sourcegraph Pricing](https://sourcegraph.com/pricing)
- [Continue.dev MCP Migration](https://docs.continue.dev/customize/context/codebase)
- [MCP Linux Foundation](https://modelcontextprotocol.io/)
- [Cursor MCP Support](https://cursor.com/docs)
- [context-mode](https://github.com/mksglu/context-mode) — session continuity MCP server (4K stars, ELv2)

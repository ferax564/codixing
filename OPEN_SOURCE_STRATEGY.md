# Codixing — Open Source Strategy & Validation

> **This document must be finalized before any public release.**
> Decisions made here are partially or fully irreversible.

---

## 1. What Cannot Be Undone

Once code is published in a public repository — even for one minute — the following is permanent:

| Action | Reversible? | Why |
|---|---|---|
| Publishing code as MIT | **NO** | Anyone who saw it can keep using it under MIT forever |
| Publishing code as AGPL | **NO** | Same — that version's license is permanent |
| Publishing Pro features in the public repo | **NO** | Even if deleted, it's in git history and web archives |
| Changing license of future commits | **YES** | Only affects new code, not already-released versions |
| Splitting repos after open sourcing | **YES** | But awkward and loses star count |
| Adding a feature to Community that was Pro | **YES** | Downgrade is always possible |
| Removing a feature from Community that was free | **NO** | Community will revolt; trust damage is permanent |

**Conclusion: get the repo structure and license right before pushing anything public. You are not past the point of no return yet.**

---

## 2. License Validation: MIT is Wrong

The proposed model uses MIT for the open-source community tier. **MIT is the wrong choice for Codixing.**

### Why MIT is too permissive

| Risk | Consequence with MIT |
|---|---|
| mgrep (Mixedbread AI) forks your community code | They get AST chunking, BM25 indexer, symbol table for free — 6–12 months of engineering — and build it into their closed cloud |
| Sourcegraph (Amp) incorporates your retrieval core | Your best engineering becomes their moat |
| A well-funded competitor emerges | They hire 10 engineers, add vector search in 3 months, and undercut you on price |
| An Anthropic or Google employee open-sources a wrapper | Claude Code ships a built-in retrieval layer based on your MIT code |

With MIT, **you are building open source for your competitors' benefit.** The community edition exists to drive adoption, not to give away your core engineering.

### License Options Compared

| License | Permissive? | Prevents SaaS forking? | OSI Approved? | Companies Using It |
|---|---|---|---|---|
| MIT | ✅ Very | ❌ No | ✅ | Everyone — too permissive |
| Apache 2.0 | ✅ Yes (+ patents) | ❌ No | ✅ | Google, Kubernetes |
| **AGPL v3** | ✅ For local use | **✅ Yes** | ✅ | Grafana, GitLab, MongoDB |
| BUSL 1.1 | ✅ Non-commercial | ✅ Yes | ❌ | HashiCorp (Terraform) |
| FSL | ✅ Non-compete 2yr | ✅ Yes | ❌ | Sentry, Sourcehut |
| ELv2 (Elastic) | ✅ Source-available | ✅ Yes | ❌ | Elasticsearch |

### Recommendation: AGPL v3 for the community repo

**AGPL v3** (GNU Affero GPL) is the right license because:

1. **Individual developer using Codixing locally to help their AI agent?** Completely unaffected by AGPL. Use it freely, forever.

2. **Company running Codixing internally for their developers?** Completely unaffected. Internal use is not "conveying" the software as a network service.

3. **Company (e.g., mgrep, Sourcegraph) trying to fork your code and sell it as a cloud service?** AGPL requires them to open-source their **entire** server stack. This is commercially unacceptable — they must buy a Pro/Enterprise license instead.

4. **Your dual-license model:** AGPL + commercial license. The CLA you already have grants maintainers rights under "any license the Maintainers choose" — this covers exactly this model. You grant yourself a commercial exception to the AGPL, which is how the Pro/Enterprise binaries are distributed without AGPL restrictions.

5. **Who else uses this model:**
   - **Grafana**: AGPL core, paid cloud (Grafana Cloud)
   - **GitLab**: AGPL CE edition, commercial EE edition
   - **MongoDB**: SSPL (AGPL-derivative) for server, commercial for cloud
   - **Nextcloud**: AGPL, commercial enterprise

**What to update:**
- `LICENSE` file: replace MIT text with AGPL v3 text
- `CONTRIBUTING.md`: update reference from "MIT license" to "AGPL v3 and commercial license"
- `CLA.md`: already correct — it says "any license the Maintainers choose" — no changes needed
- `README.md`: update license badge and section

**AGPL does NOT mean you cannot profit.** It means competitors cannot profit from your open-source work without contributing back or paying.

---

## 3. What Goes Public vs Private

### The Irreversible Decision: Feature Line

This is the most critical decision. The community (AGPL) tier must be:
- **Good enough** to generate GitHub stars, blog posts, and MCP ecosystem adoption
- **Limited enough** that power users and teams pay

The line must be drawn on features that are:
- Natural upgrade points (developer hits a wall, wants more)
- Hard to replicate from scratch (high engineering value in Pro)
- Team/collaboration features (cannot be local-only by nature)

### Public Repo: `codixing` (AGPL v3)

Everything listed here becomes open source. **You cannot take it back.**

```
codixing/ (public, AGPL v3)
├── crates/
│   ├── core/               # Community engine library
│   │   ├── src/
│   │   │   ├── indexer/    # BM25 (Tantivy) + custom code tokenizer
│   │   │   ├── ast/        # Tree-sitter parsing + AST chunking
│   │   │   ├── symbols/    # Symbol table (DashMap)
│   │   │   ├── graph/      # Import graph + call graph (basic, no PageRank)
│   │   │   ├── search/     # BM25-only search, instant strategy
│   │   │   ├── sync/       # Hash-based incremental sync (xxh3)
│   │   │   ├── watcher/    # File watcher (notify)
│   │   │   └── engine.rs   # Engine facade (community API only)
│   │   └── Cargo.toml
│   ├── cli/                # Community CLI binary
│   │   ├── src/
│   │   │   └── commands/   # init, search, symbols, callers, callees, sync, update
│   │   └── Cargo.toml
│   └── mcp/                # Community MCP server (10 core tools)
│       ├── src/
│       │   └── tools/      # code_search, grep_code, find_symbol, read_symbol,
│       │                   # read_file, outline_file, get_references,
│       │                   # list_files, index_status, write_file
│       └── Cargo.toml
├── editors/
│   └── vscode/             # VS Code extension (MIT — kept permissive for adoption)
├── docs/                   # Landing page
├── LICENSE                 # AGPL v3
├── CLA.md                  # Unchanged
├── CONTRIBUTING.md         # Updated: AGPL + commercial dual-license
└── README.md               # Updated: license badge, feature tier comparison
```

**Features intentionally included in Community:**
- BM25 full-text search (fast, no embeddings)
- Custom code tokenizer (camelCase, snake_case splitting)
- AST chunking (tree-sitter, all supported languages)
- Symbol table (exact + prefix lookup)
- Import graph + call graph (callers/callees, no PageRank)
- `codixing init`, `search`, `symbols`, `callers`, `callees`, `sync`, `update` CLI commands
- 10 core MCP tools
- File watcher (auto-reindex on save)
- Incremental sync (hash-based)
- VS Code extension (basic status bar)
- Single repo, up to 50,000 files

**Hard limits enforced in Community binary:**
- 50,000 files per index (enforced at init time with clear error message)
- BM25-only search strategy (no `--strategy fast/thorough/deep`)
- No daemon mode (each CLI call starts fresh)
- No REST API server
- No LSP server

### Private Repo: `codixing-pro` (Proprietary)

This repo is **never made public.** It is the source of truth for the Pro binary.

```
codixing-pro/ (private, proprietary)
├── crates/
│   ├── search-pro/         # Vector pipeline: fastembed, usearch HNSW, RRF, MMR
│   │   ├── src/
│   │   │   ├── embeddings/ # BGE-Base-EN-v1.5 ONNX inference
│   │   │   ├── vector/     # usearch HNSW index, int8 quantization
│   │   │   ├── reranker/   # BGE-Reranker-Base cross-encoder
│   │   │   └── fusion.rs   # Asymmetric RRF, MMR deduplication
│   │   └── Cargo.toml
│   ├── graph-pro/          # PageRank, Graph Atlas, transitive deps
│   │   ├── src/
│   │   │   ├── pagerank.rs # PageRank scoring algorithm
│   │   │   ├── atlas/      # Interactive web Graph Atlas server
│   │   │   └── impact.rs   # Predict impact, stitch context
│   │   └── Cargo.toml
│   ├── engine-pro/         # ProEngine: extends Community Engine
│   │   ├── src/
│   │   │   └── engine.rs   # Wraps community Engine, adds Pro capabilities
│   │   └── Cargo.toml
│   ├── cli-pro/            # Pro CLI binary (all commands)
│   │   ├── src/
│   │   │   └── commands/   # All community commands + git-sync, embed, graph,
│   │   │                   # dependencies, usages, serve, graph-atlas
│   │   └── Cargo.toml
│   ├── server/             # REST API server (axum)
│   ├── lsp/                # LSP server
│   ├── mcp-pro/            # Full 24 MCP tools (10 community + 14 Pro)
│   │   ├── src/
│   │   │   └── tools/      # get_repo_map, get_transitive_deps, search_usages,
│   │   │                   # rename_symbol, predict_impact, stitch_context,
│   │   │                   # enrich_docs, apply_patch, run_tests, symbol_callers,
│   │   │                   # symbol_callees, explain, delete_file, edit_file
│   │   └── Cargo.toml
│   ├── license/            # License key validation
│   │   └── src/
│   │       ├── validator.rs # Key parsing, hardware fingerprint, expiry
│   │       └── enforcer.rs  # Feature gate enforcement
│   └── cloud/              # Teams Cloud client (future)
│       └── src/
│           ├── sync.rs     # Cloud index sync daemon
│           └── auth.rs     # API key management
├── Cargo.toml              # Workspace; depends on codixing (public) as git dep
└── build/                  # Release scripts, signing, distribution
```

**Dependency direction:**
```
codixing-pro  →  depends on  →  codixing (public, AGPL)
```

The private repo imports the public crate as a dependency. This is standard open-core practice — the Pro binary contains both AGPL code (from the public repo) and proprietary code (from the private repo). Because the maintainer owns a commercial exception to the AGPL (via the dual-license model), this is legally sound.

### What Stays in the Public Repo But is NOT Released Yet

Some code currently in the monorepo needs to be **extracted to the private repo before going public:**

| Current location | Move to private repo | Why |
|---|---|---|
| `crates/core/src/` — vector search | `codixing-pro/crates/search-pro/` | Core Pro feature |
| `crates/core/src/` — PageRank impl | `codixing-pro/crates/graph-pro/` | Core Pro feature |
| `crates/core/src/` — reranker | `codixing-pro/crates/search-pro/` | Core Pro feature |
| `crates/server/` | `codixing-pro/crates/server/` | Pro only |
| `crates/mcp/` — 14 advanced tools | `codixing-pro/crates/mcp-pro/` | Pro only |

---

## 4. Engine Architecture: The Split

The current `Engine` struct in `crates/core/src/engine.rs` is the central facade for all features. Before open-sourcing, it must be split into two layers:

### Community Engine (public repo)

```rust
// crates/core/src/engine.rs (public, AGPL)
pub struct Engine {
    index: TantivyIndex,
    symbol_table: SymbolTable,
    graph: CodeGraph,       // basic: callers/callees only, no PageRank
    config: EngineConfig,
}

impl Engine {
    pub fn search_bm25(&self, query: &str, opts: SearchOpts) -> Vec<SearchResult> { ... }
    pub fn find_symbol(&self, name: &str) -> Vec<Symbol> { ... }
    pub fn callers(&self, path: &str) -> Vec<String> { ... }
    pub fn callees(&self, path: &str) -> Vec<String> { ... }
    pub fn sync(&mut self) -> SyncResult { ... }
}
```

### Pro Engine (private repo, extends community)

```rust
// codixing-pro/crates/engine-pro/src/engine.rs (private)
pub struct ProEngine {
    base: codixing_core::Engine,     // wraps the AGPL engine
    vector_index: HnswIndex,
    embedder: Embedder,
    reranker: Option<Reranker>,
    pagerank: PageRankScores,
}

impl ProEngine {
    pub fn search_hybrid(&self, query: &str, strategy: Strategy) -> Vec<SearchResult> { ... }
    pub fn get_repo_map(&self) -> RepoMap { ... }
    pub fn predict_impact(&self, diff: &str) -> Vec<ImpactedFile> { ... }
}
```

This composition pattern (Pro wraps Community) is clean, avoids AGPL infection of Pro code (the Pro struct owns the Community Engine, not the other way around), and maintains a clear API boundary.

---

## 5. Refactoring Steps Before Going Public

**Do these in order. Do not open-source anything until step 5.**

### Step 1: Create the private repo (Day 1)
```bash
# Create private GitHub repo: codixing-pro
# Do NOT make it public
```

### Step 2: Extract Pro features from `crates/core/` (Week 1–2)

Move out of the public repo and into private repo:
- All fastembed / usearch / HNSW code → `search-pro`
- BGE-Reranker code → `search-pro`
- PageRank algorithm → `graph-pro`
- Graph Atlas server → `graph-pro`
- `codixing-server` (REST API) → `server`
- 14 advanced MCP tools → `mcp-pro`

After extraction, `crates/core/` should compile and pass all tests **without** the Pro features.

### Step 3: Refactor Engine into Community + Pro (Week 2–3)

Split the Engine struct as described in section 4. Community `Engine` must be self-contained — no `use` of Pro code.

Create a clean `Engine` trait or boundary so Pro can wrap Community without the public crate knowing about Pro.

### Step 4: Update license files (Day 1 of Week 3)

In the public repo:
- Replace `LICENSE` content with AGPL v3 text
- Update `CONTRIBUTING.md`: "AGPL v3 and commercial license"
- Update `README.md` badge and license section

### Step 5: First public release (Week 3+)

Only after steps 1–4 are complete:
- Tag `v0.1.0-community` on the public repo
- Publish to crates.io (optional — crates.io requires open source, AGPL is fine)
- Announce on Hacker News, r/LocalLLaMA

---

## 6. Binary Distribution Strategy

### Two binaries, clear naming

| Binary name | Source | License | Distribution |
|---|---|---|---|
| `codixing` | Public repo | AGPL v3 | GitHub Releases, Homebrew, cargo install |
| `codixing-pro` | Private repo | Commercial | Direct download after purchase + license key |

### License key in Pro binary

The Pro binary validates a license key at startup:
- Key encodes: customer ID, expiry date, seat count, feature flags
- First run: validates against Codixing's licensing API (requires internet)
- Subsequent runs: validates locally against cached token (offline-capable for 30 days)
- Hardware fingerprint binds the key to a machine (1 transfer/year allowed)

The license enforcement code stays in the **private repo only** and is never open-sourced.

### No license key in Community binary

The community binary has **zero network calls, zero telemetry, zero sign-up.** This is critical for developer trust and is a marketing differentiator against mgrep (which requires cloud sign-in to use at all).

---

## 7. Model Validation: Stress Tests

### Stress test 1: "Community is too good, no one upgrades"

**Test:** Does BM25-only, 50K file limit genuinely drive upgrades?

- A 50,000-file limit hits most large commercial codebases. The Linux kernel is 70K files. Google's internal monorepo is millions. Even a mid-size company's multi-service repo can hit this.
- BM25 without vectors fails on semantic queries. Search for "authentication flow" when the code uses "login_handler" — BM25 returns nothing. Vector search returns the right file. This is a real, daily friction point.
- No graph/PageRank means the agent gets raw callers/callees but no ranked "most important files" view. On a 100K-line codebase this is noisy.

**Verdict: The limit is real. Community generates genuine desire to upgrade.**

### Stress test 2: "Someone forks the AGPL community repo and builds Pro features"

**Test:** Can a competitor replicate Pro features from the AGPL community code?

- The community repo contains AST chunking and BM25 — publicly documented algorithms. A capable Rust engineer could add fastembed and usearch in 2–4 weeks.
- **AGPL protection:** If they offer this as a cloud service, they must open-source their entire stack. Effectively forces them to either open-source their competitive advantage or buy from you.
- **Time advantage:** You stay 6–12 months ahead on graph intelligence (PageRank tuning, impact prediction, Graph Atlas UX). The community repo gives them the foundation but not the product.
- **Verdict: AGPL significantly slows this attack vector. MIT would not.**

### Stress test 3: "A big company (Anthropic/Microsoft) builds this natively into their agent"

**Test:** Can Claude Code or GitHub Copilot make Codixing irrelevant?

- They can build retrieval natively. Claude Code already has basic file reading.
- They cannot build it as well, as fast, on every language, with a local binary, as open-source. These are organizational constraints, not technical ones.
- More likely: they partner or acquire. An acquisition at $5–20M in year 2 is a legitimate exit.
- **Verdict: Real risk, not addressable by licensing. Addressable by shipping faster and building community.**

### Stress test 4: "Enterprise won't pay for AGPL software"

**Test:** Does AGPL create friction in enterprise sales?

- **Enterprise legal teams fear AGPL** because they worry it "infects" their proprietary code. This is actually not how AGPL works — using a tool internally is fine. But the perception is real.
- **Mitigation:** Enterprise customers buy the **commercial license** (Enterprise tier), which replaces AGPL entirely for them. The AGPL applies only to the community tier. Enterprise sales conversation: "You're not buying AGPL software. You're buying a commercial license."
- **Evidence:** GitLab EE, Grafana Enterprise, MongoDB Atlas — all sold to large enterprises despite AGPL core.
- **Verdict: Real friction, fully mitigated by the commercial license tier.**

### Stress test 5: "MCP marketplace requires MIT or Apache 2.0"

**Test:** Does AGPL block Codixing from being listed in Claude Code's MCP directory?

- Claude Code's MCP directory: no license restriction documented. MCP tools are external processes — AGPL applies to the server code, not to the protocol or to Claude Code itself.
- The VS Code extension is kept **MIT** (explicitly permissive) to ensure no extension marketplace issues.
- **Verdict: Not a blocker. Keep VS Code extension MIT, server/CLI as AGPL.**

---

## 8. Decision Summary: The Irreversible Choices

Make these decisions once. They cannot change after the first public commit.

| Decision | Choice | Rationale |
|---|---|---|
| License for community code | **AGPL v3** | Prevents SaaS competitors from forking; does not affect local users |
| License for VS Code extension | **MIT** | Required for marketplace trust; extension is thin wrapper only |
| License for Pro binary | **Proprietary commercial** | Via CLA-granted commercial exception to AGPL |
| File limit in Community | **50,000 files** | Hits most large commercial repos; clear upgrade trigger |
| Search strategies in Community | **BM25 only (instant strategy)** | Semantic search is the #1 upgrade motivator |
| Graph in Community | **Callers/callees only, no PageRank** | Graph Atlas and ranked repo maps are Pro |
| MCP tools in Community | **10 core tools** | Enough to demonstrate value, not enough for power users |
| Pro features in public repo | **Never** | Once public, always public — extract before day 1 |
| Telemetry in Community | **None** | Trust is the product; zero phone-home in community binary |

---

## 9. Timeline

```
Week 1–2: Extract Pro features from crates/core/ into private codixing-pro repo
Week 2–3: Refactor Engine into Community + Pro layers; all tests pass
Week 3:   Update LICENSE (MIT → AGPL v3), CONTRIBUTING.md, README.md
Week 3:   Internal review — does Community binary work standalone with no Pro code?
Week 4:   First public push to codixing repo (AGPL)
Week 4+:  Announce on Hacker News / r/LocalLLaMA
Month 2:  Build license key infrastructure; launch codixing-pro (Pro tier)
Month 3:  First paying Pro customers
Month 6:  Begin Teams Cloud design partner program
```

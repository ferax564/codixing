# Codixing — Business Model & Monetization Plan

> Strategy: Open-core with paid CLI tier + cloud tier.
> Open source drives discovery and ecosystem; paid tiers capture value from power users and teams.

---

## 1. The Core Tension

| Goal | Implication |
|---|---|
| Open source drives adoption | Contributors, MCP ecosystem integrations, press, word of mouth |
| Open source cuts monetization | Power users get everything free; no forcing function to upgrade |
| Closed CLI adds friction | Individual developers resist; hurts ecosystem growth |
| Cloud requires infra | Cost and operational complexity before revenue |

**Resolution:** The open-source community edition must be genuinely useful — good enough to build trust and drive GitHub stars — but not so complete that a solo developer or team never needs to pay. The line is drawn at: *features that matter at scale and in teams.*

---

## 2. The Four Tiers

### Tier 0 — Community (Open Source, MIT)

**Who:** Individual developers, students, open-source projects, contributors, AI researchers.

**Distribution:** GitHub, Homebrew, cargo install, pre-built binary (no sign-up).

**Price:** $0, forever.

**What's included:**

| Feature | Included |
|---|---|
| BM25 full-text search | ✅ |
| Symbol lookup (exact + prefix) | ✅ |
| `codixing init`, `search`, `symbols`, `sync` CLI commands | ✅ |
| Import graph (callers/callees) | ✅ (basic, no PageRank) |
| VS Code extension (basic status) | ✅ |
| MCP server — core 10 tools | ✅ (`code_search`, `grep_code`, `find_symbol`, `read_symbol`, `read_file`, `outline_file`, `get_references`, `list_files`, `index_status`, `write_file`) |
| Single repo only | ✅ |
| Up to 50,000 files | ✅ |
| Community support (GitHub issues) | ✅ |

**What is NOT included (upgrade required):**

- Vector/hybrid search (BM25+BGE embeddings)
- Deep reranker strategy
- PageRank + full graph intelligence
- Graph Atlas visualization
- Daemon mode (fast IPC)
- REST API server (`codixing-server`)
- LSP server (`codixing-lsp`)
- All 24 MCP tools
- Multi-repo support
- Git-aware sync
- Predict impact / stitch context / rename symbol

**Why this works:** BM25 search alone is already better than raw `grep` for AI agents — developers experience real value, share it, star the repo, write blog posts. The open-source code is the advertising. The pre-built binary removes all friction. But as soon as a developer hits a large repo, wants semantic search, or works in a team, they hit the ceiling.

---

### Tier 1 — Pro CLI ($19/month or $149/year per developer)

**Who:** Individual professional developers and freelancers using AI coding agents seriously.

**Distribution:** License key delivered via email after purchase. Binary activates features on key entry. Key is tied to one machine (transferable).

**What's added over Community:**

| Feature | Detail |
|---|---|
| Hybrid search (BM25 + BGE vector + RRF) | All 5 strategies: instant, fast, thorough, explore, deep |
| BGE-Reranker cross-encoder | `--strategy deep` high-precision reranker |
| Full code graph + PageRank | `get_repo_map`, `get_transitive_deps`, `predict_impact` |
| Graph Atlas visualization | Local web-based interactive graph viewer |
| Daemon mode | Unix socket IPC, ~6ms overhead vs 30ms cold start |
| REST API server | `codixing-server` binary |
| LSP server | `codixing-lsp` binary for Neovim, Emacs, JetBrains |
| All 24 MCP tools | Full tool suite including `rename_symbol`, `apply_patch`, `stitch_context`, `enrich_docs` |
| Multi-repo support | Up to 5 repos with path prefixing |
| Git-aware sync | `codixing git-sync` for fast post-pull reindex |
| Qdrant backend | Distributed vector store option |
| Up to 500,000 files per index | vs 50,000 in Community |
| Email support | 2 business day response |

**Why $19:** Anchored below Cursor Pro ($20/mo) and Windsurf Pro ($15/mo). Developers already paying $20/mo for their AI agent; adding $19/mo for better context retrieval is an easy add-on, not a replacement. Annual plan at $149 gives 35% savings and improves LTV.

**License enforcement:** The binary checks a license key at startup (offline-capable after first validation, re-validates every 30 days). The source code for community features remains MIT. The additional features in Pro ship as compiled code in the same binary but are gated behind the key — no separate download.

---

### Tier 2 — Teams Cloud ($29/user/month, minimum 3 seats)

**Who:** Engineering teams of 3–100 developers using AI coding agents on shared codebases.

**Distribution:** SaaS. Teams get a Codixing Cloud workspace. Local binary connects to the cloud workspace via an API key (similar to how `mgrep watch` syncs, but YOU control what syncs and nothing is required — cloud is additive, not mandatory).

**What's added over Pro CLI:**

| Feature | Detail |
|---|---|
| Shared team index | One index, queried by all team members |
| Web-based Graph Atlas | Shareable, browser-accessible dependency visualization |
| Index sync across machines | Developer A's changes reflect for Developer B within seconds |
| CI/CD integration | GitHub Actions / GitLab CI webhooks for auto-reindex on push |
| Access control | Who can query which repos; read/write permissions |
| Usage analytics dashboard | Query volume, latency, most-searched symbols per team member |
| Slack / GitHub integration | Share graph views, search results in Slack; link from PRs |
| Up to 20 repos per workspace | vs 5 in Pro |
| Unlimited files | No per-index file cap |
| Priority support | 4 business hour response |
| Includes Pro CLI license for all seats | No separate Pro subscription needed |

**Important:** The local binary still works 100% offline for local search. Cloud sync is opt-in per repo (`.codixingignore` for sensitive paths). Teams using cloud get the *collaboration* benefits, not the core search — which stays local and fast.

**Why this beats mgrep's cloud model:** mgrep requires uploading all code to run at all. Codixing Cloud is additive — you get the speed and privacy of local search, plus team collaboration on top. Regulated teams can whitelist only non-sensitive repos for cloud sync.

---

### Tier 3 — Enterprise ($79/user/month or annual site license)

**Who:** Engineering organizations of 50+ developers, especially in regulated industries (fintech, healthcare, defense, government).

**Distribution:** On-prem Docker/Kubernetes deployment OR Codixing Cloud (enterprise tenant). Custom contract.

**What's added over Teams Cloud:**

| Feature | Detail |
|---|---|
| On-prem deployment | Full stack runs inside your VPC or air-gapped environment |
| SSO / SAML / LDAP | Okta, Azure AD, Google Workspace integration |
| Audit logs | Who queried what, when — exportable for compliance |
| Unlimited repos and files | No caps of any kind |
| Custom embedding models | Bring your own ONNX model instead of BGE-Base |
| Dedicated Qdrant cluster | Your own vector store, not shared infra |
| SLA: 99.9% uptime | For cloud tenant; on-prem SLA covers support response |
| Dedicated support | Named customer success engineer, Slack channel |
| Custom contract / MSA | Data processing agreements, security reviews |
| Unlimited seats in site license | Negotiate flat annual fee |

**Why $79:** Anchored against Windsurf Enterprise ($60/user), GitHub Copilot Enterprise ($39/user), Cursor Teams ($40/user). At $79, it's the premium compliance-grade option — justified because no other retrieval tool offers fully on-prem deployment with this feature set.

---

## 3. Feature Allocation Summary

| Feature | Community | Pro CLI | Teams Cloud | Enterprise |
|---|---|---|---|---|
| BM25 search | ✅ | ✅ | ✅ | ✅ |
| Symbol lookup | ✅ | ✅ | ✅ | ✅ |
| Basic import graph | ✅ | ✅ | ✅ | ✅ |
| VS Code extension | ✅ | ✅ | ✅ | ✅ |
| MCP (10 core tools) | ✅ | ✅ | ✅ | ✅ |
| Single repo, 50K files | ✅ | ✅ | ✅ | ✅ |
| Hybrid search (vector) | ❌ | ✅ | ✅ | ✅ |
| Deep reranker | ❌ | ✅ | ✅ | ✅ |
| PageRank + full graph | ❌ | ✅ | ✅ | ✅ |
| Graph Atlas UI (local) | ❌ | ✅ | ✅ | ✅ |
| Daemon mode | ❌ | ✅ | ✅ | ✅ |
| REST API server | ❌ | ✅ | ✅ | ✅ |
| LSP server | ❌ | ✅ | ✅ | ✅ |
| All 24 MCP tools | ❌ | ✅ | ✅ | ✅ |
| Multi-repo (up to 5) | ❌ | ✅ | ✅ | ✅ |
| Git-aware sync | ❌ | ✅ | ✅ | ✅ |
| 500K file limit | ❌ | ✅ | ✅ | ✅ |
| Shared team index | ❌ | ❌ | ✅ | ✅ |
| Web Graph Atlas | ❌ | ❌ | ✅ | ✅ |
| CI/CD integration | ❌ | ❌ | ✅ | ✅ |
| Usage analytics | ❌ | ❌ | ✅ | ✅ |
| Access control | ❌ | ❌ | ✅ | ✅ |
| Up to 20 repos | ❌ | ❌ | ✅ | ✅ |
| SSO / SAML | ❌ | ❌ | ❌ | ✅ |
| Audit logs | ❌ | ❌ | ❌ | ✅ |
| On-prem deployment | ❌ | ❌ | ❌ | ✅ |
| Custom embedding models | ❌ | ❌ | ❌ | ✅ |
| SLA + dedicated support | ❌ | ❌ | ❌ | ✅ |

---

## 4. Revenue Model & Projections

### Pricing summary

| Tier | Price | Billing |
|---|---|---|
| Community | $0 | — |
| Pro CLI | $19/mo or $149/yr | Per developer |
| Teams Cloud | $29/user/mo | Per seat, min 3 seats |
| Enterprise | $79/user/mo | Per seat or site license |

### Conversion funnel assumption

```
GitHub stars (awareness)
        ↓  ~5–10% install the binary
Active Community users
        ↓  ~8% convert to Pro (developer who hits file/feature limits)
Pro CLI subscribers
        ↓  ~20% grow into Teams (added by employer or team adoption)
Teams Cloud subscribers
        ↓  ~15% of Teams at 50+ seats move to Enterprise
Enterprise customers
```

### Early milestone targets

| Milestone | Community Users | Pro CLI | Teams (users) | ARR |
|---|---|---|---|---|
| Launch | 500 | 0 | 0 | $0 |
| 6 months | 5,000 | 100 | — | ~$23K |
| 12 months | 20,000 | 500 | 150 (5 teams × 30) | ~$166K |
| 24 months | 100,000 | 2,000 | 1,500 (50 teams) | ~$980K |
| 36 months | 300,000 | 6,000 | 9,000 (300 teams) | ~$5.7M |

At 36-month targets, ARR breakdown: Pro CLI = $6K × 12 = $1.37M; Teams = 9K seats × $29 × 12 = $3.13M; Enterprise (5 accounts, 250 seats avg) = $1.19M.

---

## 5. Go-to-Market Sequence

### Phase 1 — Community Growth (Months 1–6)

**Goal:** 5,000 active community users, listed in Claude Code MCP marketplace.

Actions:
1. Polish the Community binary — zero-friction install (`curl | sh`, Homebrew, `cargo install`)
2. Submit to Claude Code's official MCP tool directory
3. Write "Codixing vs grep for AI agents" benchmark blog post (post to Hacker News, r/LocalLLaMA)
4. Open GitHub Discussions — build community before charging anyone
5. Reach out to Cline (5M users) and Continue for official MCP integration docs

**Do not** launch Pro yet — focus entirely on community traction.

### Phase 2 — Pro CLI Launch (Months 6–12)

**Goal:** 500 Pro subscribers, $113K ARR.

Actions:
1. Launch Pro with Stripe + license key delivery (keep it simple — no user portal needed yet)
2. Email Community users with >1,000 indexed files about Pro (natural upsell signal)
3. Add usage analytics to community binary: show users how many queries they've made, how many files, as a nudge
4. Write "How Codixing saved us 80% of Claude Code token costs" customer story (need 1–2 reference customers)
5. Price-test: run $15/mo vs $19/mo in split test for 60 days

### Phase 3 — Teams Cloud (Months 12–24)

**Goal:** 10 paying teams, $150K ARR incremental.

Actions:
1. Build the minimum viable cloud: shared index sync + web Graph Atlas only (defer access control, CI/CD)
2. Target engineering managers at companies already using Cursor or Claude Code at scale
3. Offer 30-day free trial for Teams — no credit card, just a workspace invite
4. Partner with a single enterprise design partner at 50+ seats for co-development of enterprise features
5. Pricing page: show Copilot ($39) + Codixing Teams ($29) = $68/user vs Cursor Teams ($40) alone — Codixing makes their existing tools work better

### Phase 4 — Enterprise & Fundraising (Months 24–36)

**Goal:** 3–5 enterprise contracts, $1M+ ARR, seed round.

Actions:
1. Approach regulated-industry companies (fintech, healthcare) who cannot use mgrep's cloud
2. SOC 2 Type II certification (required for enterprise sales)
3. On-prem Docker deployment as primary enterprise differentiator
4. Raise seed round ($2–3M) to fund cloud infrastructure, enterprise sales, and a second engineer

---

## 6. Source Code Strategy

**What stays MIT (open source on GitHub):**
- Core search engine: BM25 indexing, AST chunking, symbol table, basic graph
- Community CLI (feature-limited binary)
- MCP server (10 core tools)
- VS Code extension

**What is closed source (compiled into binary, feature-gated):**
- Hybrid search / vector pipeline / reranker
- Full graph intelligence (PageRank, Graph Atlas)
- Daemon, REST API, LSP servers
- Advanced MCP tools (14 additional)
- Multi-repo support

**Why not close everything:** Open-source core creates GitHub stars, blog posts, and MCP integrations — all of which are free distribution. Closed-source everything kills the ecosystem before it starts. The goal is: *the open source version makes developers want Codixing; the paid version makes them stay.*

**CLA (already in place):** The existing `CLA.md` already covers this — contributors license their work for both MIT and future commercial use. No changes needed.

---

## 7. Key Risks

| Risk | Mitigation |
|---|---|
| Community users expect everything free forever | Be explicit upfront: "Community is free forever; Pro features are paid. No bait-and-switch." |
| Cursor or Claude Code builds native bounded retrieval | Stay ahead on graph intelligence; open source community is harder to kill than a product feature |
| Cloud infra cost exceeds Teams revenue | Keep Teams MVP lean: shared index sync only, no heavy compute |
| License key cracking / piracy | Hardware fingerprinting + 30-day re-validation; tolerate piracy at small scale (they're still using the product) |
| Enterprise sales cycle is 6–12 months | Use design partners + freemium enterprise trials to compress cycle |
| mgrep gets VC funding and undercuts on price | Privacy/local-first moat; open-source moat; graph intelligence is 6–12 months of engineering to replicate |

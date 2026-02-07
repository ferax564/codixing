# CodeForge Roadmap

Last updated: 2026-02-07

## Current State

- Product definition is complete in PRD form.
- Implementation has not started; this project is the organization's main capability gap.

## Organization Goal

- Ship a practical Phase 1 MVP quickly, then integrate it into ForgePipe workflows.
- Prioritize retrieval correctness, indexing stability, and predictable latency before advanced features.

## Next Priorities

### P0 (Now - Phase 1 MVP)

1. Initialize workspace/crates (`core`, `cli`, `server`) with baseline CI.
2. Implement Tier-1 tree-sitter parsing pipeline.
3. Implement AST-aware chunking and BM25 indexing.
4. Ship CLI MVP:
   - `init`
   - `search` (BM25)
   - `symbols`
5. Implement incremental file update path and index persistence.
6. Add baseline test corpus and latency/recall sanity checks.

### Phase A Task Mapping (Current)

1. `CF-A1`: Phase 1 scaffold + BM25 contract-compatible stub for ForgePipe integration.

Dependencies:

1. `FP-A2` contract schema freeze for compatibility tests.

### P1 (Next - Phase 2)

1. Add vector index and hybrid retrieval fusion.
2. Add token-budgeted output formatting for agent workflows.
3. Expose REST API for ForgePipe worker integration.

### P2 (Later - Phase 3+)

1. Graph intelligence and repo-map features.
2. MCP/gRPC integrations and multi-repo operations.
3. Production hardening and benchmark program against retrieval competitors.

## Success Gates

- Phase 1 MVP returns relevant symbol-aware results reliably on real repositories.
- Index updates are incremental and stable under active file changes.
- ForgePipe can execute a code-aware workflow template using CodeForge as a worker.

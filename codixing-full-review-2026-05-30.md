# Codixing — Full Review & Strategic Roadmap (2026-05-30)

Reviewer synthesis. Every bug below was read against primary source and adversarially
verified (`is_real=true`). Every proposed feature respects the hard constraint:
**no hosted-LLM embeddings, ever** — BM25/Tantivy + Model2Vec static + local ONNX only.

---

## 1. Executive Summary

### Current state

Codixing is a mature, local-first code-index/search/graph engine at **v0.44.0** with
1274 tests, a genuine zero-copy mmap trigram index (110× speedups on selective literals),
a real Russ-Cox-style regex→trigram query planner wired into the live grep path, a
17-language tree-sitter parser layer, hybrid BM25F+vector retrieval with RRF fusion and a
multi-stage ranking pipeline, a PageRank/community/PPR graph layer, a 67-tool MCP server
with a 4-profile permission system and dynamic tool discovery, an LSP, an HTTP/SSE server,
and cross-repo federation. The lexical and indexing fundamentals are strong and measured
(63K-file Linux kernel: 1.57s cold / 0.79s open). The retrieval research discipline is real
(documented dead-ends, R@10=0.71 vs grep 0.345).

The weaknesses cluster in three places: **(a) the MCP write surface** (a path-traversal
hole and three UTF-8 panic sites on ordinary input), **(b) incremental-sync / persistence
durability** (non-atomic writes, an unlocked sidecar race, and a `--no-embed` path that
deletes vectors it promised to keep), and **(c) graph-layer scaling** (two O(N²) hotspots
that contradict the project's own monorepo-scale ambitions). Strategically, the biggest
unrealized levers are scope-resolved symbol identity (the precision moat vs SCIP/Kythe),
AST-pattern + lint query (the leapfrog vs ast-grep/Semgrep), and finishing the
already-scaffolded learned-sparse (SPLADE) retrieval path.

### Top 5 bugs (fix first)

| # | Severity | Bug | File:line |
|---|----------|-----|-----------|
| 1 | **P1** | `apply_patch` bypasses the path-escape guard — writes outside repo root | `crates/mcp/src/tools/files.rs:639` |
| 2 | **P1** | `read_file` / `git_diff` / `run_tests` slice on byte offsets, not char boundaries — panic on multibyte input | `crates/mcp/src/tools/files.rs:53,595,955` |
| 3 | **P1** | `max_resident: 0` in federation config → infinite loop + held mutex (input-controlled hang) | `crates/core/src/federation/mod.rs:242` |
| 4 | **P2** | `sync --no-embed` **deletes** changed files' vectors instead of leaving them stale (contradicts documented contract) | `crates/core/src/engine/sync.rs:244-249,335` |
| 5 | **P2** | Index metadata/graph/symbol persistence uses non-atomic `fs::write` — crash mid-write corrupts the index | `crates/core/src/persistence/mod.rs:415-622` |

### Top 5 strategic features (to win)

1. **Scope-resolved symbol identity (`SymbolId`)** — turn name-based find-refs into exact,
   binding-aware navigation. The single biggest precision gap vs SCIP/Kythe/rust-analyzer.
   100% AST + string hashing, no model.
2. **`codixing astgrep` + `codixing rules run`** — AST metavariable pattern search and a
   declarative lint/rule runner over the ASTs already built. Leapfrogs ast-grep/Semgrep,
   and the semantic concept graph can constrain metavariables (a fuzzy-structural query no
   AST tool offers). Deterministic; the only embeddings used are the local static ones.
3. **Finish wiring SPLADE-doc learned-sparse retrieval** — `splade.rs` (308 lines) already
   exists but is inert. Document-side expansion at index time keeps query latency at BM25
   speed and directly attacks the concept-recall gap (concept R@10 0.38). Local ONNX only.
4. **`codixing arch-check` + graph-backed LSP surfaces (codeLens/inlayHint/documentLink)** —
   architecture-conformance CI gate (vs dependency-cruiser) plus piping the graph the engine
   already computes into the editor surfaces users see. Pure graph + protocol plumbing.
5. **Parallelize the grep candidate scan + the O(N²) graph hotspots** — rayon over the
   trigram-narrowed file set (single strongest verified query-latency gap vs Zoekt) and a
   HashSet/incremental-degree rewrite of PageRank & Louvain. Pure CPU work, no inference.

**Full report file:** `/Users/andreaferrarelli/code/codixing/codixing-full-review-2026-05-30.md`

---

## 2. Confirmed Bugs (grouped by severity)

All findings below were verified against source. Where multiple findings shared a root
cause (e.g. the Obsidian-export `sanitize_filename` non-injectivity drives both the
note-overwrite and the wiki-link mis-resolution findings; the empty-`ChunkMeta.content`
post-load contract drives both the GraphPropagation and vector-only-rerank findings), they
are consolidated.

### P1 — fix immediately

#### P1-1. `apply_patch` path-traversal: writes outside the repo root
**`crates/mcp/src/tools/files.rs:638-669` (sink at 639/642/659); parser `parse_unified_diff` ~740-809 (761)**

Every other write tool (`write_file`/`edit_file`/`delete_file`) routes the path through
`resolve_safe_path()`, which normalizes `..` and rejects escapes. `call_apply_patch` does
**not**: it takes the path parsed from the diff's `+++ b/...` header (whitespace-trimmed
only, no `..` filtering) and does `let abs_path = root.join(&fp.path);` — and `PathBuf::join`
does not collapse `..`. A patch header `+++ b/../../../../etc/cron.d/x` resolves outside the
repo; `read_to_string` + `fs::write` then operate on it. The only gate is the read-only-mode
check at 609. Narrowing vs the original claim: `apply_hunks` requires the target to be
readable first, so it is a read+in-place-modify primitive over any existing file the process
can access — not arbitrary file creation. Still a write primitive on the exact trust boundary
the guard exists to protect.

**Fix:** run each `fp.path` through `resolve_safe_path(engine, &fp.path)` before read/write,
exactly as the sibling write tools do; reject any hunk whose path escapes root; validate the
stripped `b/` path is relative and non-empty.

#### P1-2. UTF-8 byte-slice panic in `read_file` / `git_diff` / `run_tests`
**`crates/mcp/src/tools/files.rs:52-53` (read_file), `~589-595` (git_diff), `~953-956` (run_tests)**

Three truncation paths index a `String` by a fixed byte count instead of a char boundary:
`&content[..max_chars]` (max_chars = token_budget×4, default 16000), `&stdout[..12000]`,
`&combined[combined.len()-8000..]`. If the cut lands inside a multibyte UTF-8 sequence the
slice panics (`byte index N is not a char boundary`). The data is fully input-controlled
(file contents, git diff output, test stdout/stderr) and routinely contains non-ASCII
(accented identifiers, CJK, emoji, smart quotes; `from_utf8_lossy` even injects 3-byte
U+FFFD). A single `read_file` on a large file with one multibyte char near the cut aborts
the MCP worker. No adversary needed.

**Fix:** a char-boundary-safe helper applied at every cut:
`fn truncate_chars(s,&max){ let mut i=max.min(s.len()); while !s.is_char_boundary(i){i-=1;} &s[..i] }`
and the symmetric ceiling variant for the `run_tests` tail slice.

#### P1-3. `max_resident == 0` federation config → infinite loop + held mutex
**`crates/core/src/federation/mod.rs:240-255`; config default `crates/core/src/federation/config.rs`**

`maybe_evict` loops `while lru.len() >= self.config.max_resident`. `max_resident` is a
deserialized `usize` with no lower-bound validation (`#[serde(default)]=5`, but a
user-supplied `codixing-federation.json` with `"max_resident": 0` parses fine). With
`max_resident==0`, once the deque empties `pop_front()` returns `None`, the body is skipped,
and `0 >= 0` re-tests true forever — a tight CPU spin **while holding `self.lru_order`**, so
every other thread that touches the LRU deadlocks. Reached from `ensure_loaded` → first
federated `search()`/`find_symbol()`. Confirmed: the operator is `>=`, not `>`.

**Fix:** clamp on load/construction (`max_resident = max_resident.max(1)`) and change to
`while lru.len() > cap { let Some(v)=lru.pop_front() else { break }; ... }`.

### P2 — fix this cycle

#### P2-1. `sync --no-embed` deletes changed files' vectors (contract violation)
**`crates/core/src/engine/sync.rs:244-249` (remove), `335` (re-add gate), `1172` (embedder stashed)**

`skip_embed` stashes **only** the embedder (`self.embedder.take()`, 1172); `self.vector`
stays `Some`. `reindex_file_impl` removes a modified file's vectors gated on the **vector
index** being present (246-247), not the embedder, so the removal runs. The only re-add/reuse
path is gated on the **embedder** being present (335), so with the embedder stashed it is
skipped entirely — no re-embed and no content-hash/stable-key reuse. Net: every changed file
ends with **zero** vectors, not stale vectors — directly contradicting the documented
`SyncOptions::skip_embed` contract ("only the vector index stays stale", 32-39) and the CLI
message at `crates/cli/src/main.rs:2477`. The original intent (Linux-kernel runaway-CPU note)
was to keep existing vectors. No test exercises `skip_embed:true`.

**Fix:** gate the vector `remove_file` (244-249) and ideally the `chunk_meta` drop behind
`self.embedder.is_some()`, leaving existing vectors in place (genuinely "stale"). Add a test:
init with embeddings → edit a file → `sync(skip_embed=true)` → assert the file still has vectors.

#### P2-2. Non-atomic persistence: crash mid-write corrupts the index
**`crates/core/src/persistence/mod.rs:415-622` (save_config 420, save_meta 439, save_graph 469, save_symbols 508, save_tree_hashes 523, save_tree_hashes_v2 541, save_tree_signatures 590, save_chunk_meta 620)**

All eight persist helpers use bare `fs::write(path, bytes)`, which truncates-then-writes;
SIGKILL/OOM/power-loss mid-write leaves a partial/zero file. Module-wide: zero `fs::rename`,
zero temp-file, zero `sync_all`. `config.json`/`meta.json` are rewritten on every `init` and
at the end of every `sync`; a truncated one makes `load_config`/`load_meta` hard-error
(serde, no fallback), and `graph.bin`/`symbols.bin` fail to deserialize on open. The
asymmetry is telling: `load_tree_signatures` and `load_tree_hashes_v2` *do* fall back
gracefully, but the others don't. The daemon watcher saving after every change batch
multiplies the window.

**Fix:** add `atomic_write(path,bytes)` = write `<path>.tmp.<pid>` → `sync_all()` →
`fs::rename(tmp,path)` (atomic same-fs replace on Unix+Windows); route all `save_*` through it.

#### P2-3. Lost-update race on the signature sidecar (concurrent mutators)
**`crates/core/src/engine/sync.rs:146-158` (invalidate), `999-1004`/`1114-1117` (sync load/save), also `git_sync` 1597-1620; persistence `mod.rs:586-613`**

`save_tree_signatures` is a plain unlocked `fs::write`. `invalidate_signature` does
load→filter→save (full-map rewrite); `sync`/`sync_with_progress`/`git_sync` each load
`old_signatures` early and write the full merged map at the end from that early snapshot.
No file lock, no merge between writers. Two cross-process mutators (e.g. a PostToolUse
`codixing update --file` overlapping a separate `codixing sync`) can have the sync's final
save resurrect a fingerprint `invalidate_signature` just removed — the exact resurrected-
baseline failure commit `93c39f0` added invalidate to prevent. Self-healing on the next
real-edit sync; bounded to one wrongly-classified-COSMETIC file. (Severity assessed P2→P3:
narrow interleave, recoverable, contingent on a cross-process precondition not fully provable
from this code.)

**Fix:** `flock` a `.lock` around the load..save critical sections in all four mutators, or
re-load + re-apply the removal immediately before sync's final save.

#### P2-4. PPR cache invalidation ignores edge-only graph changes (+ cross-repo collision)
**`crates/core/src/engine/pipeline.rs:141-152` (seed_cache_key), `218-227`, `130-133` (process-global static)**

The personalized-PageRank cache key folds in seeds + `graph.node_count()` only. An
incremental sync that rewires **edges** without changing the file count (the common refactor)
leaves the key identical, so `ppr_cache_get` returns the **pre-edit** PPR vector for up to
the 5-min TTL. `PersonalizedGraphBoostStage` is active by default (`boost_weight=0.3`) in both
`fast_pipeline` and `thorough_pipeline`, so rankings are computed against a stale graph.
`graph.edge_count()` exists and is unused. The cache is a process-global `static`, so in a
shared process two graphs with equal node_count and a same-path seed can collide (federation-
adjacent, unverified sub-claim).

**Fix:** fold `edge_count()` (or a per-graph generation counter) and a repo-root hash into
`seed_cache_key`; or clear the cache on every `sync`/`git_sync`/`rebuild_graph` that mutated the graph.

#### P2-5. PageRank adjacency build is O(N²·d)
**`crates/core/src/graph/pagerank.rs:39-45, 161-167, 279-285`**

All three PageRank variants build `out_edges` by, for each callee, doing
`nodes.iter().find(|&&n| n == c.as_str())` — a linear scan over the full node list. Total
O(N²·d), paid on every `graph --map`, full recompute, and PPR cache miss. At 63K nodes this
is billions of string comparisons. `compute_weighted_personalized_pagerank` already builds a
`HashSet<&str> node_set` (line 257) for seed filtering and **doesn't reuse it** for the
adjacency build — the fix is sitting right there.

**Fix:** build `let node_set: HashSet<&str> = nodes.iter().copied().collect();` once and use
`.contains()`. O(N²·d) → O(E).

#### P2-6. Louvain community detection is O(N²) per pass + O(N²) modularity
**`crates/core/src/graph/community.rs:113-135` (sigma_tot rescans), `186-210` (modularity double loop)**

Phase-1 recomputes `sigma_tot` from a full `(0..n).filter(...).sum()` scan for the current
and every candidate community of every node (~O(N²·iterations), max_iterations=100), and
`compute_modularity` is a strict `for i in 0..n { for j in 0..n { adj[i].get(&j) } }` (~4B
HashMap lookups at N=63K). Canonical Louvain keeps an incremental per-community total-degree
array (O(1)/move) and computes modularity over existing edges (O(E+C)). Output is numerically
correct, just unusable at scale.

**Fix:** maintain `community_total_degree: Vec<f64>` updated on each move; iterate modularity
over edges + per-community degree sums.

#### P2-7. `usearch` `add_mut` has no dimension check (cross-backend divergence)
**`crates/core/src/vector/mod.rs:107-120` (usearch, no check) vs `306-325` (brute-force, has check)**

The brute-force backend rejects wrong-dim vectors with a clear message; the usearch backend
(the primary insert path) passes the vector straight to `inner.add` with no `vector.len() ==
self.dims` check. A model/dimension change yields an opaque usearch error rather than an
early, actionable one. (Severity P3 in re-verification: usearch's own `add()` validates and
errors, and dimension changes normally trigger a full reindex, so harm is bounded; still a
real hardening gap.)

**Fix:** add the same `if vector.len() != self.dims { return Err(...) }` guard at the top of
the usearch `add_mut`/`add` so both backends reject identically.

#### P2-8. Late-chunking byte→token mapping pools the file prefix on `unwrap_or(0)`
**`crates/core/src/embedder/mod.rs:~1005-1028` (tok_start computation)**

In `embed_file_late_chunking`, `tok_start` uses `.position(...).unwrap_or(0)`. When no real
token has `byte_start >= chunk_start` (a tail chunk past the tokenizer's truncation point),
`tok_start` collapses to 0 (file start) while `tok_end_inclusive` is a high index; the guard
`end >= tok_start` passes, so the code mean-pools tokens `0..end+1` — embedding a large
**file prefix** and attributing it to the chunk. The no-end-token branch correctly emits a
zero vector; the no-start-token case should too. Highly plausible for long files (late
chunking's whole purpose).

**Fix:** make `tok_start` an `Option`; on `None`, push a zero vector and `continue` (symmetric
with the no-end-token branch).

#### P2-9. Doc chunker computes wrong byte/line ranges for paragraph-split sections
**`crates/core/src/chunker/doc.rs:92-150`**

`chunk_doc` is the production doc chunker (Markdown/HTML/rST/AsciiDoc/plain-text/OpenAPI/PDF).
When an oversized section splits at `\n\n`, the per-paragraph cursor advances by
`para_bytes + separator` but `acc` joins paragraphs without a trailing separator, so the
flush-time `acc_byte_end` over-counts by the 2-byte separator (points into the `\n\n`) and
`acc_prefix_start` is over-shifted. Result: `byte_start/byte_end/line_start/line_end` on every
non-final split chunk are off by the separator width — wrong for citation/highlighting; a
downstream `source[byte_start..byte_end]` slice reads into the following separator. (Constant
+2 over-shift per flush, not "accumulating" as originally worded; line_start only wrong when
the over-shifted bytes contain a `\n`.) Tests assert only chunk count.

**Fix:** track `acc_content_start` explicitly instead of reconstructing from
`offset_in_content - acc.len()`; compute `acc_byte_end = byte_start + acc_content_start +
acc.len()`. Add a 3-paragraph regression test pinning exact ranges.

#### P2-10. Graph-propagated neighbors surface with EMPTY content after a persisted load
**`crates/core/src/engine/pipeline.rs:443-456` (injection); contract `crates/core/src/retriever/mod.rs` (ChunkMeta.content empty post-load)**

`GraphPropagationStage` builds each injected 1-hop neighbor's `SearchResult` with
`content: meta.content.clone()` and no Tantivy fallback. But `ChunkMeta.content` is
`String::new()` after a compact-persistence load (documented contract; the
`ChunkMetaCompact→ChunkMeta` conversion hard-sets it). Every other read path
(`VectorRetriever::resolve_content`) falls back to Tantivy; this stage doesn't, and there's
no post-pipeline re-hydration. So on any normally-opened index, graph-propagated neighbors
appear in Fast/Thorough/Explore/Deep with blank content while organic hits have content. The
unit test seeds in-memory content that can't occur after a real load, so it's masked.
(Severity P1→P2: bounded — at most 3 damped neighbors per query, often truncated away;
signature/path/lines stay correct.)

**Fix:** when `meta.content.is_empty()`, resolve via the same Tantivy `lookup_chunks_by_ids`
fallback `VectorRetriever` uses; change the test to seed empty content and assert non-empty output.

#### P2-11. Cypher/GraphML export: control chars in paths corrupt line/XML output
**`crates/core/src/graph/cypher_export.rs:19-21, 74-82, 100-103`; `graphml_export.rs:18-30`**

`cypher_escape` handles only `\\` and `'` — not `\n`/`\r`/`\t`/NUL. Each MERGE/MATCH is one
`;\n`-terminated line, so a path with a newline (legal on Unix) splits a statement and yields
malformed Cypher silently. `xml_escape` handles only `& < > "` — raw control bytes are illegal
in XML 1.0 and break Gephi/yEd. Only filtering before emission is `__ext__:` exclusion.

**Fix:** escape `\n`/`\r`/`\t` and `\u`-encode/drop NUL in `cypher_escape`; numeric-escape or
strip XML-1.0-illegal control chars in `xml_escape`. Add a newline-in-path test.

#### P2-12. Obsidian export: filename collisions silently overwrite notes + break wiki-links
**`crates/core/src/graph/obsidian_export.rs:25-35 (sanitize_filename), 92/156-158 (write), 134/144/178/188/228 (links)`**

`sanitize_filename` is many-to-one: `/` and `\` → `-`, and 11 chars (`: * ? " < > | # ^ [ ]`)
all → `_`. Two distinct nodes differing only in those chars (or the realistic kebab-case
`src/foo-bar.tsx` vs `src/foo/bar.tsx` both → `src-foo-bar.tsx`) produce the same `.md`
filename; the second `fs::write` silently clobbers the first (frontmatter/callers/callees
lost), and `note_count` over-reports. All wiki-links derive from the same non-injective
function, so links mis-resolve to the surviving note. Reserved generated names
(`_COMMUNITY_<id>`, `_MOC`) share the flat dir with no namespace. (Both findings share the
`sanitize_filename` root cause; P1→P2: bounded to the opt-in export artifact, not the core
index.)

**Fix:** make `sanitize_filename` injective (percent-encode reserved chars, or append a short
stable hash of the original path on collision); namespace generated notes under a subdir;
detect collisions before writing.

### P3 — quality / smell / bounded-impact

- **`run_tests` never enforces its advertised timeout** (`files.rs:921-973`) — spawns
  `sh -c <cmd>` (arbitrary command; no `sh` on Windows), reads `timeout_secs`, prints
  `Timeout: Ns` in the header, but calls blocking `Command::output()` and never applies it.
  A hung command wedges the worker thread holding `&mut Engine`; the header is a false claim.
  *Fix:* `spawn()` + `wait_timeout` with kill on expiry, or stop advertising the timeout;
  portable shell selection.
- **`apply_patch` swallows the persist error** (`files.rs:700`) — `let _ =
  engine.persist_incremental();` then unconditional success, unlike write/edit which chain
  `.and_then` and emit a "run codixing sync to recover" message. Silent on-disk staleness +
  false success. *Fix:* mirror the sibling `and_then` pattern.
- **Daemon engine `RwLock` uses `.expect("engine lock poisoned")`** (`crates/mcp/src/daemon.rs:96-100,150`)
  — std poisoning RwLock; a panicking handler poisons it, then the watcher's next
  `.write().expect()` dies and auto-update silently stops. Violates the codebase-wide
  `unwrap_or_else(|e| e.into_inner())` invariant. *Fix:* use `into_inner()` recovery or parking_lot.
- **Daemon removes any existing socket before bind with no inline liveness probe**
  (`daemon.rs:48-54`) — unconditional `remove_file` + `bind`, remove→bind TOCTOU; the correct
  `socket_alive` probe (247-255) exists but isn't called here. The Windows twin uses
  `first_pipe_instance(true)`; Unix has no equivalent. *Fix:* probe `socket_alive` before
  removing; flock a `daemon.lock`.
- **Windows daemon pipe name mismatch** (`crates/cli/src/daemon_proxy.rs:150-159` vs
  `crates/mcp/src/daemon_windows.rs:33-41`) — client formats `codixing-<hash>`, server formats
  `codixing-<hash>-<profile>`. The Windows warm-daemon fast path is **always** a silent
  fallback to slow in-process open. Also: non-canonicalized path + unstable `DefaultHasher`.
  *Fix:* mirror the profile suffix, canonicalize root, use a fixed-seed stable hasher on both sides.
- **Vector-only top hits can surface empty content, poisoning the Deep reranker**
  (`crates/core/src/retriever/vector.rs:56-74`) — `resolve_content` returns `String::new()` on
  Tantivy `Err`/miss (Err arm swallowed); a vector-only hit keeps that empty content into the
  cross-encoder. *Fix:* warn/propagate on hydration failure; drop or fall back to
  signature/scope text.
- **`is_identifier_query` treats `->`, `::`, URLs as identifier lookups**
  (`crates/core/src/retriever/hybrid.rs:194-202`) — mis-weights RRF toward BM25 on degenerate
  inputs. *Fix:* also require `query.chars().any(|c| c.is_alphanumeric())`.
- **`ConceptBoost` multiplier is uncapped** (`crates/core/src/engine/pipeline.rs:486-501`) —
  `1.0 + 0.3 * cluster.score * hit_count` with no cap on `hit_count`, compounding with the
  fixed 3.5× DefinitionBoost; every sibling boost stage has a `.min(...)` cap and this one
  doesn't. *Fix:* clamp `hit_count`/the final multiplier; consider additive accumulation.
- **GraphPropagationStage runs before TruncationStage** (`pipeline.rs:662-697`) — damped
  neighbors (≤0.25× top) are below the 0.35 cliff and truncated away in the common case;
  the stage does work then discards it. *Fix:* run it after truncation with a length cap, or
  drop it. (The "empty-content of finding #1" sub-claim is rejected — not in the code.)
- **PPR cache cross-repo collision** — folded into P2-4 above (federation-adjacent, unverified).
- **No-op sync always rewrites `tree_hashes_v2`** (`sync.rs:1118-1124`) — the
  `skipped_by_mtime != unchanged` guard term is dead (`!current_hashes.is_empty()` is always
  true on a non-empty repo), so every poll rewrites the file. *Fix:* track an actual
  `mtime_drift` bool and gate on it.
- **Cosmetic-skip stat undercounts** (`sync.rs:403-406`) — `all_reused` requires
  `reused_via_stable_key > 0`, excluding the all-content-hash-reuse case where the embed
  round-trip *was* fully avoided. Statistic-only. *Fix:* gate on `reused > 0` (any reuse).
- **`reindex_file` removes old chunks before `fs::read`** (`sync.rs:242-266`) — on a read
  error (TOCTOU), the direct path leaves the live in-memory index half-mutated and skips the
  commit. Self-heals on restart. *Fix:* read first, remove second.
- **Signature fingerprint `body_span` is lexer-unaware** (`crates/core/src/engine/fingerprint.rs:119-153`)
  — backward brace match with no string/char/comment awareness; an unbalanced `}` inside a body
  literal can overshoot and mask signature bytes, hashing a later signature edit as COSMETIC →
  stale vector. Bounded by Rust-only gating, `has_nested_item`, and the stable-key reuse guard;
  documented as accepted residual risk. *Fix:* derive the body span from the tree-sitter
  `block`/`body` node instead of raw bytes.
- **HTML dashboard force sim is O(n²) + synchronous 120-iter warmup** (`crates/core/src/graph/html_export.rs:38,1273-1305,1448`)
  — ~240M inline pair-iterations at the 2000-node default cap freeze the tab on load. *Fix:*
  Barnes-Hut/quadtree above a few hundred nodes; move warmup into a frame-budgeted rAF loop.
- **HTML dashboard layer drill loses hidden state** (`html_export.rs:1058-1065`) — eye-toggle
  during a drill desyncs `hiddenLayers`/`.off` from `node.hidden` after exit. *Fix:* reconcile
  legend state on `exitDrill`, single source of truth.
- **LineChunker assumes single-byte terminators** (`crates/core/src/chunker/line.rs:20-40`) —
  CRLF offset drift; currently test-only/dead (not on the production dispatch). *Fix:* walk real
  byte offsets. Re-rate upward if wired as the plain-text fallback.
- **`.h` headers always route to C, never C++** (`crates/core/src/language/mod.rs:140-141`) —
  C++ classes/templates/namespaces in `.h` are missed/mis-kinded. *Fix:* content-sniff `.h`
  for C++ markers (mirrors the existing `.m` peek).
- **FilterPipeline drops the recovery hint when a stage rewrites without shrinking bytes**
  (`crates/core/src/filter_pipeline/mod.rs:79-91`) — tee/hint gated on `filtered.len() <
  output.len()`; a `replace` that grows bytes loses the original silently. Built-in rules all
  shrink, so custom-rule-only. *Fix:* gate on `filtered != output`.
- **Cyclomatic complexity double-counts `else if`** (`crates/core/src/complexity.rs:21-22`) —
  `matches("if ")` + `matches("else if")` both fire on `} else if`, +2 for one branch; can
  flip a function's risk band. *Fix:* `plain_ifs = matches("if ") - matches("else if")`.
- **Hotspot commit-header heuristic misclassifies 40+-hex paths** (`crates/core/src/temporal.rs:80-97`)
  — a file path whose first 40 chars are hex is parsed as a `%H %an` header, skewing
  author/frequency. *Fix:* use `--format=%x00%H%x00%an` / `-z` so headers start with NUL.
- **`PersistedSessionEvent` doc promises a `query` field that doesn't exist**
  (`crates/core/src/shared_session.rs:11-15,39-58`) — the `--learn-reformulations` session-
  mining contract is documented but the struct has no `query` member (records only `file_path`).
  Doc-only defect; the consumer is deferred to v0.43. *Fix:* add `query: Option<String>` and
  populate for Search events, or correct the doc.
- **Cross-process JSONL appends can interleave into torn lines** (`shared_session.rs:104,159-164,276-291`)
  — `writeln!` on a bare unbuffered `File` under a process-local mutex only; two processes in
  O_APPEND can interleave multi-syscall writes; readers silently `continue` on parse failure →
  silent session-event loss. *Fix:* build one `String` (incl. `\n`) and `write_all` once; for
  full safety `flock` the append.

---

## 3. Architecture Observations & Risks

1. **The MCP write surface is the weakest perimeter.** Three of four P1s live in
   `crates/mcp/src/tools/files.rs`. The path-escape guard exists and is used by three of four
   write tools — `apply_patch` is the lone bypass — and the truncation helpers were never
   centralized, so the same byte-slice panic was copy-pasted into three tools. **Recommendation:**
   route *all* write tools through one `resolve_safe_path` chokepoint and one
   `truncate_chars` helper; add a `#[test]` that every write tool rejects a `..` path.

2. **Persistence has no atomicity or cross-process locking layer.** Non-atomic `fs::write`
   (P2-2), unlocked sidecar RMW (P2-3), and torn JSONL appends (shared_session) are three
   instances of the same missing abstraction. The codebase already standardized on
   `unwrap_or_else(|e| e.into_inner())` for poison recovery — it should similarly standardize
   on an `atomic_write` + advisory-lock module. The daemon (saving after every change batch)
   amplifies every one of these.

3. **Two index fields that should move together don't.** P2-1's root cause is that
   `skip_embed` stashes `self.embedder` but the removal logic keys off `self.vector`; they are
   independent fields with no invariant tying them. Any future "skip X" flag risks the same
   class of bug. **Recommendation:** make vector mutation explicitly depend on embedder
   presence, or introduce an enum state (`EmbeddingMode::{Active, Frozen}`) that gates both.

4. **The graph layer's complexity contradicts the monorepo positioning.** PageRank O(N²·d)
   and Louvain O(N²) are fine at the tested 63K but the marketing targets 1M-file scale. The
   `node_set` HashSet already exists in one PageRank variant and isn't reused — these are
   low-risk wins that should land before any "monorepo scale" claim.

5. **Strong differentiators are scaffolded but inert.** `splade.rs` (308 lines) is not in the
   strategy path; the semantic concept graph is disconnected from any structural-query surface;
   the reranker infra is (correctly) disabled but a structure-aware reranker isn't built; the
   PPR cache lacks edge-sensitivity. There's a recurring pattern of building infrastructure to
   ~90% and not wiring the last 10% into the default path. The roadmap below prioritizes
   *finishing* over starting.

6. **The compact-persistence empty-content contract is a footgun.** `ChunkMeta.content` being
   empty post-load is correct for memory, but any new code path that reads `meta.content`
   directly (instead of `resolve_content`) silently degrades after a reload (P2-10). A
   `content()` accessor that *requires* a Tantivy handle and never returns the raw empty field
   would make this unrepresentable.

---

## 4. Feature Roadmap to WIN

Every item below stays local. Embeddings, where used, are the project's existing static
Model2Vec table or a local quantized ONNX model run at index time — **never a hosted LLM**.

### (a) Quick wins — S/M effort, high impact

| Feature | Effort | How it stays local |
|---|---|---|
| **Parallelize the grep candidate scan** — replace the single-threaded `'files:` loop in `grep_code_inner` (files.rs:86) with `par_iter()` + atomic early-exit at `opts.limit`, preserving ordered/count semantics. The single strongest verified query-latency gap vs Zoekt. | S | Pure CPU/IO over trigram-narrowed files + the `regex` crate. |
| **Fix the two O(N²) graph hotspots** (PageRank HashSet reuse; Louvain incremental degree + edge-based modularity). | S/M | Deterministic graph math. |
| **`codixing arch-check --rules arch.toml`** — forbidden-dependency + `no-cycles` (Tarjan SCC) rules over the existing Import/Call edges; path:line diagnostics + non-zero exit for CI; red overlay in HTML export. Beats dependency-cruiser as an architecture guardrail. | M | Glob matching + SCC over the persisted graph. No model. |
| **Graph-backed LSP surfaces** — codeLens (`N callers · M tests · complexity C`), inlayHint (hotspot/PageRank rank), documentLink (`EdgeKind::DocumentedBy` code↔docs). All data already computed; just protocol plumbing. | L (M each) | Reuses existing engine queries; no embeddings. |
| **Wire existing graph analyses into the HTML dashboard** — Cycles/Path-A→B/Hubs toolbar overlays from `community.rs`/`pagerank.rs`/`degree()`. | S | Deterministic graph algorithms, render-only. |
| **`--lang` / `--kind` / `--in-symbol` flags on `codixing grep`** — join the trigram candidate set with the existing symbol-span index so a regex can scope to a language or symbol body. Matches Sourcegraph `lang:`/`type:symbol`. | M | Joins two already-built local indexes. |
| **Make MCP `Minimal` profile the shipped default + `--profile=auto`** — the 10-tool lean set + runtime promotion infra already exist; just default to it and auto-promote on first out-of-set call. Cuts the per-session schema token tax. | S | Pure dispatch-state routing. |
| **In-band provenance/confidence tag on search hits** (`via: bm25f+graph-ppr | rank | grep-reachable: no`), behind a verbose flag. Teaches the agent why to pick Codixing over grep. | S | Metadata serialization from the existing pipeline. |
| **MCP task→tool routing cheatsheet as a static resource** — near-zero per-turn tokens, steers tool choice. | S | Static doc resource, zero inference. |
| **`codixing graph --schema` (JSON)** — emit the node/edge/confidence taxonomy + which languages populate which edge kinds. Makes exports interoperable. | S | Reflection over existing enums. |

### (b) Strategic bets

| Feature | Effort | How it stays local |
|---|---|---|
| **Scope-resolved `SymbolId` + exact find-refs/rename** — a scope-resolution pass over the existing tree-sitter trees assigns each definition a canonical `SymbolId = hash(corpus, path, scope-path, kind, arity)` and resolves each occurrence to one def_id (lexical scope chain → import map). Store edges as `(occurrence → def_id)`; find-refs becomes exact membership. Route LSP go-to-def/refs/rename through resolved ids; gate rename to single-resolution. **The precision moat vs SCIP/Kythe/rust-analyzer.** | XL | Pure static AST + symbol-table analysis + string hashing. |
| **`codixing astgrep` + `codixing rules run`** — metavariable AST pattern search (`$VAR`, `$$$ARGS`) over the ASTs already produced, pre-filtered by a per-node-kind posting index (the trigram idea applied to node kinds); then a declarative TOML rule runner (pattern / pattern-not / pattern-inside via the symbol containment graph) emitting SARIF + CI exit codes. **Leapfrogs ast-grep/Semgrep.** Add `--rewrite` for codemods (comby parity). | L (astgrep) + L (rules) | Deterministic tree-sitter matching + the existing filter_pipeline TOML parser. |
| **Finish SPLADE-doc learned-sparse retrieval** — run a quantized local ONNX SPLADE-doc model at **index time only** (document-side expansion; query stays lexical), store expansion-weighted terms as boosted Tantivy postings, add as a fusion input alongside BM25 + Model2Vec dense. Directly attacks concept recall (0.38). | L | Local ONNX at index time; query latency stays at BM25 speed. No hosted model. |
| **Extend the call/reference graph to Java/C/C++/C#/Ruby** — `graph/extract.rs` only covers Rust/Py/TS/Go; add tree-sitter visitors for the remaining compiled grammars to the existing `DefinitionInfo`/`ReferenceInfo` structs. Brings precise callers/callees to enterprise polyglot repos. | L | Deterministic tree-sitter walking. |
| **Local-only SCIP bridge (export + import)** — emit Codixing's resolved graph as SCIP protobuf and ingest external `.scip` files (rust-analyzer/scip-typescript/scip-java) to borrow compiler-grade precision where an indexer exists, BM25/graph elsewhere. Exact cross-repo navigation via monikers. | L | File I/O only; no network, no LLM. |
| **`codixing tune` — local coordinate-descent over (k1, b, field boosts, rrf_k, mmr_lambda)** maximizing R@10/MRR on user labels or auto-mined symbol-definition pseudo-labels. Plus query-adaptive weighted fusion (identifier→lexical-heavy, prose→dense-heavy) feeding the same optimizer. Free recall win. | M (+S fusion) | Arithmetic over the existing index. No API. |
| **Deterministic `StructuralRerankStage`** — re-rank top-K by symbol-definition match, call-graph hops to PPR seeds, exact-identifier match, centrality (the signals that *help* code, unlike the prose-biased rerankers already rejected). Optionally fit weights with `codixing tune`. | M | Reuses graph + symbol tables; zero inference. |
| **`codixing watch` + git-porcelain fast-path** — wire the existing `notify`-based watcher into a first-class incremental loop with O(changes) changed-set acquisition (vs an O(repo) walk). | M | Filesystem/git plumbing on existing deps. |
| **Auto-tier dense backend at scale** — above a chunk-count threshold, default to the in-tree Model2Vec static table (table lookup, no neural forward pass) instead of ONNX BgeSmallEn; collapses the documented 3-hour-at-47K-chunks wall. Extend `cosmetic_eligible` body-mask reuse beyond Rust to TS/Py/Go/Java. | M | Static lookup table; fully local. |

### (c) Moonshots

| Feature | Effort | How it stays local |
|---|---|---|
| **Intraprocedural def-use + bounded interprocedural taint** — `codixing taint --source <pat> --sink <pat>` over the AST + existing call graph (worklist propagation, depth-bounded, sanitizer patterns). The CodeQL security query, no hosted model. | XL | Deterministic graph traversal; zero embeddings. |
| **Native fact-query language (`codixing query`)** — a Datalog-ish/relational surface over the persisted `GraphData` (`defines`, `calls`, `imports`, `documented_by`, `member_of`) with composable joins. Replaces the one-shot Neo4j Cypher dump; brings Glean/Kythe queryability in-process. | XL | In-memory joins over petgraph. No DB, no LLM. |
| **Concept-constrained AST metavariables** — `$FN:concept=auth` matches callees in the locally-built "auth" concept cluster (from the existing static Model2Vec/ONNX concept graph). "Find auth-related sinks taking untrusted input" in one query — structurally precise AND semantically broad. **No AST-only tool can offer this.** | M (on top of astgrep) | Set-membership against precomputed local concept clusters. |
| **Directory-/size-bucketed content-addressed shards** over the existing roaring+mmap trigram primitives — per-shard incremental rebuild, fan-out query, bounded query RAM at 1M+ files; reuse federation merge intra-repo. Plus branch-tagged postings (index by git blob OID, no checkout; per-doc branch bitmask) for `--branch` scoping. Matches Zoekt's defining scale strength. | XL | Index/CPU + git-object work; no embeddings. |
| **Server-driven incremental graph (`codixing graph --serve`)** — stream only the focused symbol's k-hop neighborhood over the existing SSE channel; lazy-expand on click; remove the 500-node force-sim cap. Matches Sourcetrail navigation at full repo size. | L | Persisted graph + PageRank over existing server crate. |
| **Code-distilled Model2Vec table as the default dense backend** — one-time offline distillation from a local code encoder over a code-token vocabulary, shipped as a frozen lookup table; dense recall on code semantics at zero per-query inference, faster than 110s BgeSmallEn init. | L | Build-time distillation; runtime is a frozen lookup. No API. |

---

## 5. Competitor Scorecard

| Dimension | Leading competitor | Codixing today | What it takes to beat them |
|---|---|---|---|
| Trigram/regex code search at scale | Zoekt / Sourcegraph | Real mmap trigram index + Cox-style regex planner, wired & 52-110× on selective literals — but **candidate verification is single-threaded** and the build is single-threaded | Parallelize the candidate scan (S); shard the index over the existing roaring+mmap codec; measure at 1M-file scale |
| Regex character-class pre-filtering | Zoekt | Small classes → `MatchAll` (full scan) | Enumerate bounded classes (≤16) into trigram ORs in `build_query_plan` (M) |
| Scoped grep (`lang:`/`type:symbol`) | Sourcegraph / GitHub | grep is language/symbol-blind; symbol index exists separately | Join the two local indexes via `--lang`/`--kind`/`--in-symbol` (M) |
| Multi-branch indexing | Zoekt | Single checked-out tree only | Branch-tagged postings keyed by git blob OID (XL) |
| Precise cross-file find-refs / safe rename | SCIP / rust-analyzer | **Name-based** (trailing-identifier match) → superset, not exact; LSP inherits it | Scope-resolved `SymbolId` + def_id-keyed edges; gate rename to single resolution (XL) |
| Call-graph language coverage | SCIP indexers / rust-analyzer | Call/ref graph for **4 of 17** languages (Rust/Py/TS/Go); imports for ~17 | Add tree-sitter visitors for Java/C/C++/C#/Ruby (L) |
| Portable symbol interchange | SCIP / LSIF | None — bitcode-only | Local SCIP export + import bridge (L) |
| Long-tail language def indexing | universal-ctags / gtags | 17 tree-sitter langs; nothing for the tail (Lua/HCL/R/…) | ctags-style regex tag-table fallback for ~30 langs (M) |
| Occurrence roles (read/write/impl) | rust-analyzer / SCIP | Coarse `ReferenceKind` (Call/Import/Inherit/FieldAccess/TypeRef), only Call emitted | Tag occurrences with role bitset; `--role` filters (M) |
| Language-agnostic fact DB + query language | Glean / Kythe | Fixed CLI verbs; one-shot Cypher dump; name-based graph | Native `codixing query` Datalog-ish surface over `GraphData` (XL) |
| Versioned/diffable fact deltas | Glean | Reindex-in-place; visual diff overlay only | `graph --diff-base --json` emitting added/removed typed edges (M) |
| Scalable interactive graph viz | Sourcetrail | D3 force-sim, 500-node PageRank cap, O(n²) + sync warmup | Server-driven k-hop streaming, lazy expand, remove cap (L) |
| Architecture-conformance rules | dependency-cruiser | Sees every import edge, can't assert on them | `codixing arch-check --rules` + CI exit code (M) |
| Metric "city"/risk visualization | CodeCharta | Only size=PageRank, color=EdgeKind | Treemap/city: area=LOC, color=complexity/churn (M) |
| AST pattern search | ast-grep / comby | None — grep is text-only over rich ASTs | `codixing astgrep` with metavariables + node-kind prefilter (L) |
| Declarative lint rules over the index | Semgrep | None (filter_pipeline ≠ rules) | `codixing rules run` → SARIF + CI gate (L) |
| Taint / dataflow | CodeQL | Call graph only, no value flow | Def-use + bounded interprocedural taint (XL) |
| Structural codemod | comby / ast-grep | Read-only for code | `astgrep --rewrite` (M) |
| Local learned-sparse retrieval | SPLADE (self-hosted) | `splade.rs` exists but **inert** | Wire SPLADE-doc as an index-time fusion input (L) |
| BM25(F) / fusion tuning | Tantivy-tuned stacks | Hand-set global k1/b/boosts + single rrf_k | `codixing tune` coordinate descent + adaptive fusion (M/S) |
| Code-helpful reranking | code-trained rerankers | Reranker correctly disabled (general ones hurt) | Deterministic `StructuralRerankStage` (M) |
| Dense at monorepo scale | Tabby / Continue (GPU) | 3-hr wall at 47K+ chunks on the default ONNX path | Auto-tier to Model2Vec static + extend cosmetic-reuse beyond Rust (M) |
| Incremental update latency | Watchman + Tantivy | Watcher exists; changed-set cost unconfirmed | `codixing watch` + git-porcelain fast-path (M) |
| Memory footprint at scale | OpenGrok / Hound | PQ + mmap primitives exist; default/RSS unreported | Auto-engage PQ/mmap above threshold + `--max-memory` (M) |
| Agent toolset ergonomics | Serena / lean MCP kits | 4-profile system built; Minimal off by default | Default to Minimal + `--profile=auto` + provenance tags (S) |
| Editor reach | Cody / Zed agent panel | VSCode + Claude Code only | Thin Zed (MCP-native) + Neovim plugins over existing server (L) |

---

## 6. Suggested Next 2-Week Execution Plan (ordered)

**Week 1 — stop the bleeding (P1s + the highest-leverage P2s).**

1. **Day 1-2 — P1-1 + P1-2 (MCP write surface).** Route `apply_patch` through
   `resolve_safe_path`; add a centralized `truncate_chars` helper and apply it to
   `read_file`/`git_diff`/`run_tests`. Add tests: every write tool rejects a `..` path; each
   truncation site survives a multibyte char at the boundary. *These are security/crash bugs on
   the agent-facing surface — they ship first.*
2. **Day 2 — P1-3 (federation hang).** Clamp `max_resident.max(1)` on load + change the loop
   condition to `>`. One-line fix + a config-parse test with `"max_resident": 0`.
3. **Day 3-4 — P2-1 (`--no-embed` vector deletion) + P2-2 (atomic persistence).** Gate the
   vector remove behind `embedder.is_some()`; add the regression test. Introduce
   `atomic_write` (tmp + sync_all + rename) and route all eight `save_*` through it. *Highest-
   impact correctness/durability pair.*
4. **Day 5 — P2-5 + P2-6 (graph O(N²)).** HashSet reuse in PageRank; incremental degree +
   edge-based modularity in Louvain. Quick, isolated, removes the scaling embarrassment.

**Week 2 — close the remaining P2s and land the first quick-win moat piece.**

5. **Day 6 — P2-4 (PPR edge-staleness)** fold `edge_count()` + repo-root hash into the cache
   key (or clear on graph rebuild); **P2-10 (empty-content neighbors)** add the Tantivy
   fallback + fix the test.
6. **Day 7 — P2-8/P2-9 (chunker offsets)** late-chunking zero-vector symmetry + doc-chunker
   `acc_content_start` with exact-range regression tests; **P2-7** usearch dim guard.
7. **Day 8 — P2-3 (sidecar race) + shared_session torn-write + daemon `.expect` poison** — the
   concurrency cluster: flock the sidecar critical sections, `write_all` the JSONL line, switch
   the daemon lock to `into_inner()` recovery.
8. **Day 9 — P2-11/P2-12 (export hardening)** Cypher/XML control-char escaping; injective
   `sanitize_filename` + namespaced generated notes; the Windows pipe-name profile-suffix fix.
9. **Day 10 — first strategic quick win: parallelize the grep candidate scan** (rayon
   `par_iter` + atomic early-exit), with the provenance tag groundwork. This is the visible
   "Codixing beats grep at scale" win and unblocks the larger sharding bet.

**Sweep (each day):** run the verification triad (`cargo test --workspace`, `cargo clippy
--workspace -- -D warnings`, `cargo fmt --check`) before every commit; update test counts in
README/CLAUDE.md/docs as bugs land; one PR per logical cluster, smallest-first merge order.

After Week 2, the codebase is correctness-clean and the strategic arc begins with the two
moat builders — **scope-resolved `SymbolId`** and **`codixing astgrep`** — in parallel
feature branches.

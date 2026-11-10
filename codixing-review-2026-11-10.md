# Codixing review: token cost and large-codebase effectiveness

Date: 2026-11-10
Reviewed revision: `8c69b6e` (v0.48.0), release build
Scope: hands-on measurement of what an AI agent actually receives and waits for,
on three real codebases up to Linux-kernel size, plus root-cause reading of the
code behind each finding. This builds on the May, June, and July reviews and
does not repeat their findings.

## Verdict

Codixing's core promise holds for the case it was built around. When an agent
looks up a known identifier, `codixing search` finds the definition at rank 1
most of the time and returns 76–321 tokens where `rg -nw` returns 2,900–5,500
(median), a 17–53x reduction. Incremental sync is correct and the index is
compact and crash-safe.

Three things currently stop that from carrying over to large codebases and to
task-level work:

1. **The recommended entrypoint is the weakest one.** `codixing ask` finds the
   right file far less often than plain `codixing search` on natural-language
   tasks (MRR 0.07–0.16 vs 0.42–0.52) and costs about 3x the tokens. A wrong
   context pack is the most expensive output Codixing can produce, because the
   agent then reads the wrong files.
2. **At kernel scale every CLI call pays 4.5 s and 900 MB to reopen the index.**
   ripgrep answers the same literal query in 0.3 s. The daemon removes this
   cost, but it fails to start on long repository paths and many commands
   bypass it.
3. **Several outputs are unbounded or padded.** One `symbols` call returned
   126K tokens; `init` printed 19K tokens of third-party warnings; the `ask`
   pack carries about 7x more tokens than the information in it needs.

Most of the fixes are small and local. The ten changes in
[Priorities](#priorities) are ordered by expected effect per unit of work.

## Method

| Item | Value |
|---|---|
| Machine | 4 vCPU, 15 GB RAM, Linux cloud container, warm page cache |
| Toolchain | Rust 1.97.0, `cargo build --release --workspace` (7m34s) |
| Index mode | BM25-only (no ONNX Runtime in this environment) |
| Token counts | `tiktoken` `cl100k_base`, used as a proxy tokenizer |
| Timing | Single runs unless stated; wall clock of the whole process |

Test corpora:

| Corpus | Indexed files | Chunks | Symbols | `init` time | Peak RSS | Index size |
|---|---:|---:|---:|---:|---:|---:|
| Codixing (this repo) | 361 | 4,247 | 6,332 | 1.8 s | 143 MB | 15 MB |
| Django 5.0.6 | 3,851 | 33,903 | 29,137 | 6.1 s | 325 MB | 56 MB |
| Linux 6.6 | 71,025 | 691,189 | 827,306 | 223.6 s | 4,049 MB | 1.8 GB |

The retrieval evaluation uses 62 queries with known answers (30 identifier
lookups, 32 natural-language tasks). A hit means a result from the expected
file appears at that rank or better. The harness and task files are in
`benchmarks/review-2026-11-10/`.

Limits of this evidence:

- The query sets are small and written by the reviewer. The gaps reported
  below are large relative to that noise, but the exact percentages are not
  precise.
- No embeddings were tested. Hybrid search would likely improve the
  natural-language numbers on the two smaller repositories.
- The release binary is stripped, so open-time attribution comes from debug-log
  timestamps and code reading, not a sampling profiler.
- Linux `ask` timings in the spot check were taken while a compile was running
  and are inflated; the 19.5 s figure was measured on an idle machine.

## What works well

| Observation | Evidence |
|---|---|
| Identifier lookup is accurate and cheap | Django: hit@1 9/10, median 76 tokens vs 2,909 for `rg -nw` |
| Incremental sync is correct | Added, modified and removed symbols were reflected in `symbols`, `grep` and `search` after `sync` and `update --file` |
| One-file sync is cheap at mid scale | Django: 0.34–0.37 s, 129 MB |
| Incremental artifacts stay small | Linux one-file edit: one generation, 39 KB trigram delta, 157 KB symbol delta, index stayed 1.8 GB |
| Disk estimate is honest | `init --dry-run` predicted 1.2–2.4 GB for Linux; actual 1.8 GB |
| Warm queries are fast at kernel scale | Via daemon on Linux: `search` 95–110 ms, `symbols` 75 ms, `usages` 11 ms, `callers` 9 ms |
| Some budgets are exact | `search --format --token-budget 500` returned 500 tokens; `graph --map --token-budget 1500` returned 1,467 |
| Code health | `cargo fmt --check` and `cargo clippy --workspace -- -D warnings` pass; see [Verification](#verification) for the test suite |

## Retrieval results

`search` is `codixing search <q> --limit 10`; `ask` is `codixing ask <q>`
scored on its `must_read` list.

| Corpus | Query type | n | `search` hit@1 / @3 / @10 | `search` MRR | `ask` hit@1 / @3 / @10 | `ask` MRR |
|---|---|---:|---|---:|---|---:|
| Codixing | identifier | 8 | 7 / 7 / 8 | 0.89 | 6 / 7 / 7 | 0.81 |
| Codixing | natural language | 12 | 4 / 8 / 10 | 0.52 | 1 / 2 / 5 | 0.16 |
| Django | identifier | 10 | 9 / 10 / 10 | 0.95 | 7 / 9 / 10 | 0.82 |
| Django | natural language | 10 | 3 / 5 / 7 | 0.42 | 0 / 2 / 2 | 0.07 |
| Linux | identifier | 12 | 8 / 8 / 8 | 0.67 | not run in full | |
| Linux | natural language | 10 | 1 / 2 / 3 | 0.16 | not run in full | |

`ask` was not run in full on Linux because each call took 20 s or more. In four
spot checks it was correct once (`vfs_read`). For "x86 page fault handler" it
returned three Makefiles.

Median output tokens per query:

| Corpus | Query type | `search` | `ask` | `rg -nw` |
|---|---|---:|---:|---:|
| Codixing | identifier | 93 | 1,178 | 4,900 |
| Codixing | natural language | 691 | 2,248 | n/a |
| Django | identifier | 76 | 2,050 | 2,909 |
| Django | natural language | 637 | 2,068 | n/a |
| Linux | identifier | 321 | n/a | 5,471 (max 765,492) |
| Linux | natural language | 1,434 | n/a | n/a |

The Linux `search` token figures are higher partly because those calls went
through the daemon, which uses a more verbose format (finding B4).

## Findings

Each finding lists the measurement, the cause in the code, and the fix.
Severity reflects effect on an agent's token spend and wall time.

### A. Wrong or missing answers

A wrong answer costs more than any formatting overhead: the agent reads the
wrong files, then searches again.

#### A1. `ask` ranks worse than `search` (high)

Measured: see the table above. Example: for "how does incremental sync decide
which files changed", `search` puts `engine/sync.rs` at rank 1; `ask` returns
three test functions whose names contain "incremental", all with the identical
score 7562.5, and nothing from `sync.rs`.

Cause, in `crates/core/src/engine/context_assembly.rs`:

- `task_identifier_candidates` (L535–560) treats any prose word of 8 or more
  characters as an identifier: "incremental", "password", "database",
  "scheduler".
- If any such word exists, the strategy becomes `Goto` on the whole task
  sentence (L661–670). No symbol is named like a sentence, so this falls
  through to exact-substring search of the sentence.
- Each long word is then substring-matched against all symbol names. The first
  three matches, in line order, are pinned at score 25,000 or 50,000, above
  every real search hit (L676–721). In `understand` mode these three pins can
  fill the whole `must_read` list.

Fix: accept a token as an identifier only if it has identifier shape
(`snake_case`, `CamelCase`, `::`, digits) and an exact symbol-table entry.
Run the normal `search` pipeline on the task text and merge exact-name
definitions into it, rather than pinning substring matches above it. Add the
62-query set here as a regression test with the rule that `ask` must not score
below `search`.

#### A2. Definition-first ranking puts `impl` blocks ahead of the type (high)

Measured: `codixing search Engine` returns eight `impl Engine` fragments in
alphabetical file order before `struct Engine` at rank 9. The snippets shown for
those hits are arbitrary lines from inside the impl blocks. The MCP tools
`read_symbol` and `find_symbol` with name `Engine` return `EngineSlot` and
other substring matches first, and fill the 4,000-token envelope before
reaching the exact match.

Cause: `is_primary_definition_kind` includes `EntityKind::Impl`
(`engine/pipeline.rs:452`), although the doc comment on `search_goto` says impl
blocks rank below types. Ties are broken by file path
(`engine/search.rs:641–648`). For a multi-hundred-line symbol the code picks
the tightest overlapping chunk, which is an arbitrary inner one (L675–701).

Fix: rank in tiers, type definitions, then functions and constants, then impl
blocks. Within a tier prefer exact-name matches and non-test paths. For the
chunk, take the one containing the symbol's first line. Apply the same
exact-match-first ordering to `symbols`, `find_symbol`, and `read_symbol`.

#### A3. C and C++ type references are indexed as definitions (high at scale)

Measured: on Linux, `symbols task_struct --defs-only` lists 6,680 `Struct`
rows. The real definition is row 3,645. Lines such as
`struct task_struct *last_wakee;` are recorded as struct definitions.
`search` does not return `include/linux/sched.h` in the top 10 for
`task_struct`, and the same failure occurs for `sk_buff`, `file_operations`
and `kmalloc`. Those are the four identifier queries where ripgrep is most
expensive (64K to 765K tokens), so they are where an index should help most.

Cause: `match_entity_kind` returns `Struct` for every `struct_specifier` node
(`language/c.rs:90–98`; the same pattern is at `language/cpp.rs:138`).

Fix: emit a definition only when the node has a `body` field. Record bodiless
specifiers as references. This should also shrink the 298 MB Linux symbol
table and the open time in C1.

#### A4. A smaller `--limit` changes which result is first (medium)

Measured: `search "struct Engine" --limit 1` returns a Swift test helper. With
`--limit 3` or more, rank 1 is `engine/mod.rs`. Two of four queries tested
showed this.

Cause: the BM25 paths retrieve exactly `limit` candidates and only then apply
boosts and demotions (`engine/search.rs:172–173`, `208`). The multi-query path
over-fetches but truncates to `limit` before the pipeline runs (L458, then
L190).

Fix: retrieve `max(3 x limit, 30)` candidates, run the pipeline, then
truncate. This matters for token use because asking for fewer results is the
obvious way for an agent to save tokens.

#### A5. `usages` misses real call sites, including in `--complete` mode (high)

Measured: `rg -nw add_chunk` matches 16 Rust files (a few of them use an
unrelated function with the same name). `usages add_chunk --complete`,
described as deterministic and complete, reports 23 locations in two files. It
misses the two production call sites of the Tantivy method,
`engine/indexing.rs:195` and `engine/sync.rs:589`, and seven more calls in
`engine/pipeline.rs` and `retriever/bm25.rs`. The default ranked mode
lists 17 of 21 matching files, shows 45-line chunk ranges with a 40-character
preview, and does not show the call line.

Cause: complete mode takes its candidate files from ranked BM25 retrieval
(`engine/symbol_graph.rs:157`) and then parses only those files. Which stage
drops the missing files was not traced.

Fix: take candidate files from the file-trigram index, which already
guarantees recall for `grep`, then classify each hit by AST as definition,
call, import, or comment. Print `path:line: source line`. An agent that changes
a signature based on an incomplete caller list breaks the build and pays for
another round.

#### A6. Natural-language search is weak at kernel scale without embeddings (medium)

Measured: Linux MRR 0.16. Misses include queries whose answer is nearly spelled
out in the file path: "x86 page fault handler" (`arch/x86/mm/fault.c`),
"cubic tcp congestion control" (`net/ipv4/tcp_cubic.c`), "epoll wait for
events" (`fs/eventpoll.c`).

Embeddings are not a practical answer at this size: at the documented BgeSmallEn
rate (1,054 chunks in 110 s), 691K chunks is on the order of 20 hours.

Hypothesis, not verified: path segments are indexed (`index/tantivy.rs:349`)
but carry little weight against content matches. A file-level scoring pass,
which aggregates chunk scores per file and adds weight for query terms that
match path components and file-level symbol names, is the cheapest experiment.
Embedding only file summaries or top-PageRank files would bound the cost of a
hybrid option.

### B. Tokens spent on output that carries no information

#### B1. The `ask` pack is about 7x larger than its content (high)

Measured on this repository:

| Task | Printed | Compact JSON | Same facts as text | Self-reported |
|---|---:|---:|---:|---:|
| add a --json flag to the callers command | 3,648 | 2,627 | 482 | 2,446 |
| how does incremental sync decide which files changed | 1,776 | 1,262 | 304 | 1,227 |
| fix broken pipe panic when output is piped to head | 4,001 | 2,961 | 484 | 2,814 |
| Engine | 3,157 | 2,266 | 499 | 2,146 |

Causes:

- The CLI pretty-prints (`cli/src/main.rs:3680`), which adds about 38%.
- `repo_orientation.symbols` is the first six symbols of each file, which are
  usually imports: 45 of 60 entries in the first task were `import` or
  `module` lines (`context_assembly.rs:804–813`).
- `related_symbols` is the first eight symbols per file, again mostly imports
  such as `std::fs`, each as a five-key object (L830–847).
- Redundant fields: `range` plus `start_line` and `end_line`; `score` plus a
  `reason` that restates the score; `why: "Contains evidence handle E1"`;
  null `branch` and `risk_level`.
- The budget is enforced on compact-JSON bytes divided by four (L586), so the
  printed pack exceeds the reported size by 30–45%.

Fix: make compact line-oriented text the default and keep the JSON schema
behind `--json`. Drop imports, choose symbols near the evidence span, omit
empty and derived fields. Spend the saved budget on a few lines of source for
the top evidence items, which would let the agent skip a follow-up read.

#### B2. Unbounded outputs (high)

| Command | Output |
|---|---:|
| `symbols task_struct --defs-only` (Linux) | 126,278 tokens |
| `init` (Django), warnings on stdout | 19,110 tokens |
| `context sync.rs --line 2224`, default settings | 12,826 tokens |
| `audit` (this repo) | 8,203 tokens |
| `symbols Engine` (this repo; 85 of 146 rows are imports) | 3,687 tokens |

Causes and fixes:

- `symbols` has no limit and no ranking. Add a default limit of about 50 with
  a "N more, narrow with --file or --kind" trailer, order exact matches first,
  and make `--defs-only` the default.
- `context` defaults to a soft budget. In Rust the primary chunk for a method
  is the whole `impl` block (2,999 lines here), so the default call ran 3x over
  its own 4,096 budget. Default to strict, and choose the innermost enclosing
  function starting at its first line. The current strict window is centred on
  the line, so most of it showed the previous function.
- `audit` prints every stale file. Print counts per tier and the top entries.
  `build.rs`, hook scripts, and `install.sh` are reported as "likely dead code
  (certain)"; entry points need an exemption.

#### B3. Log lines go to stdout, with colour codes (medium)

Measured: `codixing init` on Django wrote 546 lines of
`WARN foster parenting not implemented` (from the HTML parser dependency) to
stdout with ANSI escapes.

Cause: the CLI's tracing subscriber has no stderr writer and applies level
`warn` to every crate (`cli/src/main.rs:1060–1062`). The MCP and LSP binaries
already write to stderr.

Fix: `.with_writer(std::io::stderr)`, `.with_ansi(false)` when not a terminal,
and a default filter of `warn` for Codixing crates and `error` for others.

#### B4. The same command prints different formats depending on the daemon (medium)

Measured on Linux: `search task_struct` is 281 tokens in-process and 763 via
the daemon. With `--limit 3` it is 151 vs 404. `grep` and `symbols` also differ
in format and row caps.

Cause: proxied commands print the MCP tool's formatted body, and in-process
commands print the CLI's compact list (`cli/src/main.rs:1689–1717`).

Fix: have the daemon return structured results and render them in the CLI with
one formatter. The compact form should be the default in both paths.

#### B5. Line numbers use two conventions (medium)

Measured: `pub struct Engine` is on line 401. `grep` reports 401. `symbols`
reports `L400`, `search` reports `[L398-L401]`, and `usages --complete` prints
0-based lines. The `ask` pack converts to 1-based. `context --line` takes a
0-based argument.

Effect: an agent that passes a `symbols` line to an editor or a read tool is
off by one.

Fix: 1-based inclusive everywhere in user-facing output and arguments. Keep
0-based values internal.

#### B6. Token estimates undercount (medium)

Measured: the estimator is bytes divided by four. Against `cl100k_base` it
undercounts by 5% on Rust, 11% on JSON, and more on C: `context --budget-mode
strict --token-budget 1500` printed 2,373 tokens on Linux and 628 for a
500 budget on this repository. The header reported "Used: 1251" for 1,501.

Fix: calibrate per content class (about 3.3 bytes per token for code is closer)
or use a real BPE tokenizer for final accounting, and treat the budget as a
ceiling on the estimate plus a margin.

#### B7. Piping to `head` panics (medium)

Measured: `codixing ask ... | head`, `symbols ... | head`, and
`usages ... | head` exit with code 101 and a "failed printing to stdout:
Broken pipe" panic message. Agents pipe to `head` routinely; the panic text
lands in their context and the non-zero exit reads as a failure.

Fix: restore the default `SIGPIPE` disposition at the start of `main` on Unix,
or route output through a writer that treats `EPIPE` as a clean exit.

#### B8. Default `search` output (low)

- The snippet is the first three lines of the chunk
  (`cli/src/main.rs:1819–1825`), often a doc comment or a test preamble. Show
  the lines that match the query terms.
- Scores such as `score=24002.000` are unbounded and not comparable between
  queries. Drop them or normalise to a relative value.
- Results continue well past the relevance cliff. A cut-off relative to the top
  score would shorten most natural-language result lists.

#### B9. MCP surface (medium)

| Profile | Tools | Catalog tokens |
|---|---:|---:|
| minimal (default) | 10 | 1,912 |
| reviewer | 51 | 8,451 |
| editor | 68 | 10,499 |
| dangerous | 70 | 10,700 |

- Five of the ten minimal tools are meta-tools. The minimal profile has no
  `grep_code`, `search_usages`, or `change_impact`, which are the tools the
  project's own guidance treats as the core workflow. Reaching them means
  switching to `reviewer` and adding about 6,500 tokens of schema to every
  later turn.
- `code_search` alone is 658 tokens, mostly the strategy description.

Fix: add a middle tier of about 12 read tools (the current five plus grep,
usages, impact, read file, find tests, callers, callees), or let a tool found
through `search_tools` be called without changing profile. Shorten parameter
descriptions and move detail to `get_tool_schema`.

### C. Latency and memory on large repositories

#### C1. Every CLI call reopens the whole index (high at scale)

Measured cold wall time per command:

| Corpus | Typical command | Peak RSS |
|---|---:|---:|
| Codixing | 25–40 ms | 59 MB |
| Django | 130–150 ms | 71–84 MB |
| Linux | 4.3–4.9 s | about 900 MB |

For comparison, `rg -n -w vfs_read` over the Linux tree takes 0.3 s. The
README states 1.57 s cold-start search on the kernel; that figure was not
reproduced here (different hardware, 71K vs 63K files) and should be
re-measured.

Breakdown of `grep vfs_read --count` on Linux (4.8 s), from debug-log
timestamps:

| Stage | Time |
|---|---:|
| Tantivy open | about 0.01 s |
| Symbol table load | 2.55 s |
| Chunk metadata, per-file postings, graph | about 1.4 s |
| Query, then process teardown | about 0.85 s |

Causes:

- `MmapSymbolTable::load` calls `validate_structure()` on every open
  (`symbols/mmap.rs:408`, `1003–1008`). It walks the full 298 MB file: string
  pool, every record, prefix index, file indexes. The file is memory-mapped but
  is read in full each time.
- `open_read_only_with_mode` deserialises all chunk metadata into a map and
  loads the graph regardless of the command (`engine/init.rs:977–1044`).
  `grep` needs only the trigram index. `symbols` needs only the symbol table.
- Stack samples near exit show the main thread freeing memory. A short-lived
  CLI process can skip that.

Fix:

- Validate the symbol table once when a generation is published and record a
  digest in the generation manifest. On open, check the header and digest only.
- Load each component lazily on first use, as the file trigram and concept
  index already are.
- Skip teardown in the CLI with `std::process::exit` after flushing output.

Expected result: cold `grep` and `symbols` on Linux well under 1 s.

#### C2. The daemon cannot start on long repository paths (high)

Measured: with the repository at a 132-character path, `codixing-mcp --daemon`
exits with `failed to bind daemon socket ... path must be shorter than
SUN_LEN`. The CLI then falls back to cold starts with no message.

Cause: the socket lives at `<root>/.codixing/daemon-<profile>.sock`, and Unix
socket paths are limited to about 108 bytes on Linux and 104 on macOS. Deep
monorepo checkouts and worktrees exceed this easily.

Fix: place the socket in a short private runtime directory
(`$XDG_RUNTIME_DIR/codixing/<hash>.sock`, or a per-user 0700 directory under
the system temp directory) and store its path in a pointer file under
`.codixing/`. This also closes the bind-then-chmod window noted in the July
review. `codixing doctor` should report a daemon that failed to bind.

#### C3. Many commands never use the daemon (medium)

Measured on Linux with a warm daemon:

| Command | Time |
|---|---:|
| `search`, `grep`, `symbols`, `usages`, `callers`, `callees` | 9–146 ms |
| `grep --count`, `search --json` | 4.6 s |
| `sync .` with no changes | 5.5 s |
| `graph --map` (proxied; cold is 15–16 s) | 6.9 s |
| `impact` | 7.4–7.7 s |
| `ask` | 19.5 s |
| `context` | 130–168 s |
| `audit` | stopped after 6 minutes |

Fix: add a structured daemon endpoint that returns totals so `--count`,
`--files-with-matches`, and `--json` can be proxied. Proxy `ask`, `impact`,
and `context`. Start the daemon on first CLI use when the index is above a
size threshold, with an idle timeout, so CLI-only users get warm latency.
Cache the repository map per generation.

#### C4. `ask` rebuilds the project-wide test map once per file (high at scale)

Measured: `ask` takes about 15 s beyond index open on Linux.

Cause, by code reading: `find_tests_for_file` builds the complete test map for
all files and their import edges on each call
(`engine/test_mapping.rs:35–41`), and `agent_context_pack` calls it once per
orientation file (`context_assembly.rs:860`). `impact` pays the same cost once.

Fix: compute tests for one file directly from naming rules and the graph's
callers of that file, or build the map once per generation and cache it.

`context` (130–168 s) and `audit` (over 6 minutes) were not profiled. A likely
contributor to `context` is the same call in usage-example mining
(`engine/examples.rs:82`), made once per definition of the symbol; with the
inflated definition counts from A3 that multiplies quickly. Both commands need
a profile on a symbol-bearing build and a time limit that returns partial
results.

#### C5. The post-edit hook is expensive on large repositories (medium)

Measured on Linux: `update --file` after a one-line edit takes 9.6 s and
1.3 GB. `sync .` takes 10.9 s, of which 4.7 s is the persist phase. The
PostToolUse hook starts one such process in the background after every edit,
so a burst of edits queues several 1.3 GB processes behind the writer lock.

Fix: have the hook send the changed path to the daemon, which already batches
watcher events, and only fall back to a process when no daemon runs. Debounce
in the hook. Investigate the 4.7 s persist for a one-file change.

#### C6. Initial indexing memory (low)

Linux `init` peaked at 4.05 GB. That is acceptable on a workstation and tight
on an 8 GB laptop or small CI runner. Reporting peak memory in
`init --dry-run`, alongside the disk estimate, would set expectations.

### D. Measurement and guidance

#### D1. `bench-tokens` cannot detect any of the above (medium)

The harness runs one hard-coded workflow against a baseline that always reads
lines 1–500 of the target file (`cli/src/main.rs:4192–4198`), counts bytes
divided by four (L4353), and never checks that the output contains the answer.
It reported "63.5% saved" on this repository while `ask`, one of its five
steps, was returning the wrong files.

Fix: replace it with a task set that has expected answers. Report tokens spent
until the correct file and line are present, hit rate, and wall time, against
an `rg` plus bounded-read baseline. Run it in CI on two or three pinned real
repositories. The current large-repo gate measures speed on a synthetic corpus
and exact-match validity, so retrieval quality on real code has no gate.

#### D2. Guidance and hooks (low)

- `CLAUDE.md` and the hooks recommend the CLI over MCP. On a large repository
  without a daemon that is the slowest path (C1). The guidance should depend
  on repository size, or the CLI should start the daemon itself.
- `CLAUDE.md` describes `codixing audit` as an index-freshness check. It is a
  stale and orphaned source-file report; `doctor` and `sync` cover index
  freshness.
- The Grep-blocking hook suggests `codixing search` for multi-word patterns.
  Given A6, a failed semantic search on a large repository leaves the agent
  with no fallback it is allowed to use. The hook should suggest
  `codixing grep` first for literal patterns.

### E. Smaller issues

| Issue | Evidence |
|---|---|
| `grep 'a\|b'` returns nothing with no hint | POSIX-style alternation is common in agent-written patterns; detect `\|` and suggest `|` |
| `grep --file README.md` also matched `benchmarks/README.md` | The flag is documented as targeting a single file |
| `impact` lists documentation first and counts it in the blast radius | `sync.rs`: "235 files" of 361, led by a review document and `CHANGELOG.md` |
| `symbols` table rows contain embedded newlines | Multi-line import and typedef names break the column layout |

## Priorities

Ordered by expected effect on agent token spend and wall time per unit of work.

| # | Change | Findings | Size |
|---:|---|---|---|
| 1 | Fix `ask` identifier detection and pinning; require `ask` to match or beat `search` on a fixed query set | A1 | S |
| 2 | Fix definition tiers (`impl` below types), exact-match-first ordering, and C/C++ bodiless structs | A2, A3 | S |
| 3 | Logs to stderr; `SIGPIPE`; default limits on `symbols`, `audit`; strict `context` default | B2, B3, B7 | S |
| 4 | Compact text default for `ask`; drop imports and redundant fields | B1 | S |
| 5 | Over-fetch before the ranking pipeline | A4 | S |
| 6 | Skip per-open symbol validation; lazy component loading; skip CLI teardown | C1 | M |
| 7 | Short socket path; proxy remaining commands; auto-start daemon on large indexes | C2, C3, C5 | M |
| 8 | Trigram-backed `usages` with exact call lines | A5 | M |
| 9 | Per-file test lookup or cached test map; profile `context` and `audit` | C4 | M |
| 10 | Answer-checked token benchmark in CI on real repositories; one line-number convention; one output formatter | D1, B4, B5, B6 | M |

After these, the next step for natural-language quality on very large
repositories is the file-level scoring experiment in A6.

Status of the July roadmap, for continuity: the 10K/100K gate, delta-based
incremental artifacts, shared trigram storage and bounded PageRank have
landed. The delta artifacts and single-generation behaviour were confirmed in
these tests; the others were not exercised directly. Still open from that list and
confirmed here: file-scoped symbol lookup for pack and map building (C4), one
response envelope across CLI, daemon, MCP and HTTP (B4), daemon readiness and
socket placement (C2), and request cancellation for long operations (C3).

## Verification

| Check | Result |
|---|---|
| `cargo build --release --workspace` | Passed |
| `cargo fmt --check` | Passed |
| `cargo clippy --workspace -- -D warnings` | Passed |
| `cargo test --workspace` | 1,715 passed, 0 failed, 10 ignored (42 test binaries) |

No source code was changed in this review. It adds this document and the
measurement harness.

## Reproducing the measurements

```bash
cargo build --release --workspace
export PATH="$PWD/target/release:$PATH"
pip install tiktoken

# Retrieval evaluation (index the corpus first with `codixing init .`)
python3 benchmarks/review-2026-11-10/eval.py benchmarks/review-2026-11-10/tasks_self.json . search ask rg
python3 benchmarks/review-2026-11-10/eval.py benchmarks/review-2026-11-10/tasks_django.json /path/to/django search ask rg
python3 benchmarks/review-2026-11-10/eval.py benchmarks/review-2026-11-10/tasks_linux.json /path/to/linux search rg

# Time, peak RSS, and token count of any command
python3 benchmarks/review-2026-11-10/measure.py -- codixing symbols Engine

# Token cost of each section of an ask pack
python3 benchmarks/review-2026-11-10/pack_cost.py "how does incremental sync decide which files changed"

# MCP catalog size per profile
python3 benchmarks/review-2026-11-10/mcp_probe.py . list
```

Corpora: Linux 6.6 from `cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.6.tar.xz`
and the Django 5.0.6 source distribution from PyPI. Keep the corpus path short
when testing the daemon (finding C2).

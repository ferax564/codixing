# Codixing MCP Launch Preparation

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Prepare Codixing for closed-source MCP distribution — simplify code, run multi-language benchmarks, update docs/website, and package for release.

**Architecture:** Clean up dead code and unnecessary dependencies, run benchmarks on Rust/Python/JS/TS/Go/Java repos, update website with results, prepare cross-platform binary distribution with zero-config install.

**Tech Stack:** Rust, tree-sitter, GitHub Actions, npm (for `npx` distribution), HTML/CSS/JS (website)

---

## Current State

- 33 MCP tools, 368 tests, 5 crates (core, cli, mcp, lsp, server)
- 14 languages supported via tree-sitter
- SWE-bench Lite: R@1=48.0%, R@5=71.0% (Python only, 300 tasks)
- Real-world benchmark: 6 repos (Rust + Python + JS), 26 tasks
- Release CI: linux-x86_64 + macos-aarch64
- Website: `docs/index.html` — needs benchmark updates
- README: comprehensive but has stale GPU section and unneeded detail

---

## Task 1: Code Cleanup — Remove Dead Code and Simplify

Remove unused code, simplify overly complex paths, and clean up the benchmark experiments that didn't work out.

**Files:**
- Modify: `benchmarks/swe_bench_eval.py`
- Modify: `crates/core/src/reranker/mod.rs` (check if used)
- Modify: `crates/core/src/retriever/http_reranker.rs` (check if used)
- Modify: `crates/server/src/routes/graph.rs` (dead code warnings)
- Modify: `README.md` (remove GPU Acceleration section, trim)

**Step 1: Check for dead code in core crate**

```bash
cargo clippy --workspace 2>&1 | grep "dead_code\|unused"
```

Identify all dead code warnings. The known ones are in `crates/server/src/routes/graph.rs`.

**Step 2: Clean up swe_bench_eval.py**

The file still contains `get_reranker()`, `rerank_chunks()` (fastembed cross-encoder), and CLI args `--py-rerank`, `--reranker` that are legacy experiments. Remove them if they're not part of the best pipeline.

Keep only:
- BM25 search (`search_codixing_multi`)
- Outline embed reranking (`embed_rerank_files`, `extract_file_outline`)
- Grep baseline (`search_grep`)
- The CLI args: `--limit`, `--repo`, `--skip-clone`, `--strategy`, `--embed-rerank`

**Step 3: Remove GPU Acceleration section from README**

The "GPU Acceleration Options" section (lines ~388-407) documents experiments that aren't useful to end users. Remove it entirely.

**Step 4: Run tests to verify nothing broke**

```bash
cargo test --workspace
cargo clippy -p codixing-core -p codixing -p codixing-lsp -p codixing-mcp -- -D warnings
```

**Step 5: Commit**

```bash
git add -A
git commit -m "chore: remove dead code, legacy benchmark experiments, and GPU docs"
```

---

## Task 2: Multi-Language Benchmark — Expand repos.toml and tasks.toml

Add Go, Java, TypeScript, and C++ repos to the benchmark suite. The existing `run_benchmark.py` already supports multi-repo evaluation via `repos.toml` + `tasks.toml`.

**Files:**
- Modify: `benchmarks/repos.toml`
- Modify: `benchmarks/tasks.toml`

**Step 1: Add new repos to repos.toml**

Add after the existing repos:

```toml
# ── Go ────────────────────────────────────────────────────────────
[[repo]]
name = "gin"
url = "https://github.com/gin-gonic/gin"
lang = "go"
description = "HTTP web framework (~30K LoC Go)"
shallow = true

# ── Java ──────────────────────────────────────────────────────────
[[repo]]
name = "spring-boot"
url = "https://github.com/spring-projects/spring-boot"
lang = "java"
description = "Java application framework (~500K LoC Java)"
shallow = true

# ── TypeScript ────────────────────────────────────────────────────
[[repo]]
name = "next.js"
url = "https://github.com/vercel/next.js"
lang = "typescript"
description = "React framework (~300K LoC TS/JS)"
shallow = true

# ── C++ ───────────────────────────────────────────────────────────
[[repo]]
name = "leveldb"
url = "https://github.com/google/leveldb"
lang = "cpp"
description = "Key-value store (~20K LoC C++)"
shallow = true
```

**Step 2: Add tasks for each new repo to tasks.toml**

Add ~4 tasks per repo covering: symbol lookup, code understanding, call graph, architecture. Follow the exact same format as the existing tokio/django/react tasks. Each task needs:
- `repo`, `id`, `category`, `description`, `query`
- `baseline_commands` (grep/cat commands)
- `codixing_commands` (MCP tool calls)
- `expected_file` (where applicable)

Example for gin:

```toml
# ═══════════════════════════════════════════════════════════════════════
# GIN (Go)
# ═══════════════════════════════════════════════════════════════════════

[[task]]
repo = "gin"
id = "gin-1"
category = "symbol_lookup"
description = "Find the Engine struct definition"
query = "Engine"
baseline_commands = [
    "grep -rn 'type Engine struct' --include='*.go'",
    "cat gin.go",
]
codixing_commands = [
    "find_symbol Engine",
]
expected_file = "gin.go"

[[task]]
repo = "gin"
id = "gin-2"
category = "code_understanding"
description = "How does middleware chaining work in gin"
query = "middleware chain handler"
baseline_commands = [
    "grep -rn 'func.*Use(' --include='*.go'",
    "grep -rn 'HandlersChain' --include='*.go'",
    "cat routergroup.go",
]
codixing_commands = [
    "search 'middleware chain handler'",
]

[[task]]
repo = "gin"
id = "gin-3"
category = "call_graph"
description = "Find callers of Context.JSON"
query = "Context.JSON callers"
baseline_commands = [
    "grep -rn '\\.JSON(' --include='*.go'",
]
codixing_commands = [
    "symbol_callers JSON --file context.go",
]

[[task]]
repo = "gin"
id = "gin-4"
category = "architecture"
description = "Get gin project structure overview"
query = "project structure"
baseline_commands = [
    "find . -name '*.go' -exec wc -l {} + | sort -rn | head -20",
]
codixing_commands = [
    "get_repo_map",
]
```

Add similar 4-task blocks for: spring-boot, next.js, leveldb.

**Step 3: Run the expanded benchmark**

```bash
python3 benchmarks/run_benchmark.py --skip-index 2>&1 | tee /tmp/bench_multilang.log
```

If repos aren't cloned yet:

```bash
python3 benchmarks/run_benchmark.py
```

This will clone, index, and benchmark all 10 repos (~42 tasks).

**Step 4: Save results and verify**

Results auto-save to `benchmarks/results/`. Check that all new repos index successfully and tasks produce reasonable results.

**Step 5: Commit**

```bash
git add benchmarks/repos.toml benchmarks/tasks.toml benchmarks/results/
git commit -m "feat(bench): add Go, Java, TypeScript, C++ repos to multi-language benchmark"
```

---

## Task 3: Multi-Language SWE-bench-style Evaluation

Create a lightweight file-localization benchmark for non-Python repos. Since SWE-bench is Python-only, we'll create our own mini benchmark using known bug-fix commits from popular repos.

**Files:**
- Create: `benchmarks/multilang_eval.py`
- Create: `benchmarks/multilang_tasks.toml`

**Step 1: Create multilang_tasks.toml**

Each task specifies: repo URL, commit before the fix, problem description (from the commit message or issue), and gold files (from the fix diff).

```toml
# Multi-language file localization benchmark
# Format: repo, base_commit (before fix), problem_statement, gold_files

# ── Rust ──
[[task]]
id = "tokio-timeout-1"
repo = "https://github.com/tokio-rs/tokio"
base_commit = "..."  # commit before the fix
problem_statement = "timeout future doesn't work correctly when polled after expiration"
gold_files = ["tokio/src/time/timeout.rs"]

# ── TypeScript ──
[[task]]
id = "next-router-1"
repo = "https://github.com/vercel/next.js"
base_commit = "..."
problem_statement = "router.push() doesn't preserve query params"
gold_files = ["packages/next/src/client/router.ts"]

# ... (aim for 10-20 tasks per language, 5 languages = 50-100 tasks)
```

Actually, curating 100 tasks with exact commits is too much work for now. Instead:

**Step 1: Create a simpler approach — test indexing + search quality on known repos**

Create `benchmarks/multilang_eval.py` that:
1. Clones/uses cached repos (tokio, gin, spring-boot, next.js, leveldb)
2. Indexes each with Codixing
3. Runs 10 known-answer queries per repo (symbol lookup → check if symbol file is in top-5)
4. Reports per-language accuracy

```python
#!/usr/bin/env python3
"""
multilang_eval.py — Multi-language search quality evaluation

Tests Codixing's ability to locate known symbols and files across
Rust, Python, Go, Java, TypeScript, and C++ codebases.

Usage:
    python3 benchmarks/multilang_eval.py
    python3 benchmarks/multilang_eval.py --repos gin leveldb
"""
```

The key function per task:
1. Index the repo with `codixing init . --no-embeddings`
2. Run `codixing search "QUERY" --json --limit 20`
3. Check if `expected_file` is in the top-1, top-5, top-10
4. Aggregate per-language recall metrics

**Step 2: Define tasks inline (no separate TOML needed)**

Hard-code ~10 tasks per language in the script. Each is: `(repo_name, query, expected_file_substring)`.

Example:
```python
TASKS = {
    "tokio": [
        ("Runtime struct definition", "runtime/runtime.rs"),
        ("TcpListener bind implementation", "net/tcp/listener.rs"),
        ("spawn_blocking function", "runtime/blocking"),
        ("JoinHandle implementation", "task/join.rs"),
        ("io copy utility", "io/util/copy.rs"),
    ],
    "gin": [
        ("Engine struct definition", "gin.go"),
        ("Context JSON response", "context.go"),
        ("RouterGroup middleware", "routergroup.go"),
        ("recovery middleware panic handler", "recovery.go"),
        ("binding validation", "binding"),
    ],
    "leveldb": [
        ("DB Open implementation", "db/db_impl.cc"),
        ("MemTable insert", "db/memtable.cc"),
        ("SSTable block reader", "table/block.cc"),
        ("Write batch implementation", "db/write_batch.cc"),
        ("LRU cache implementation", "util/cache.cc"),
    ],
    "django": [
        ("QuerySet filter implementation", "db/models/query.py"),
        ("URL resolver match", "urls/resolvers.py"),
        ("Model save method", "db/models/base.py"),
        ("Template render", "template/base.py"),
        ("Form validation clean", "forms/forms.py"),
    ],
    "react": [
        ("useState hook", "ReactHooks"),
        ("reconciler begin work", "ReactFiberBeginWork"),
        ("createElement function", "ReactElement"),
        ("useEffect hook", "ReactFiberHooks"),
        ("fiber commit work", "ReactFiberCommitWork"),
    ],
}
```

**Step 3: Run the evaluation**

```bash
python3 benchmarks/multilang_eval.py
```

Expected output: per-language table with Hit@1, Hit@5, Hit@10, and an overall score.

**Step 4: Commit**

```bash
git add benchmarks/multilang_eval.py
git commit -m "feat(bench): add multi-language search quality evaluation"
```

---

## Task 4: Simplify and Optimize Core — Profile and Fix Hot Paths

Profile the MCP server and identify any obvious inefficiencies.

**Files:**
- Modify: `crates/core/src/engine/mod.rs` (if needed)
- Modify: `crates/mcp/src/main.rs` (if needed)

**Step 1: Profile a typical search workflow**

```bash
# Index codixing itself and time operations
time ./target/release/codixing init . --no-embeddings
time ./target/release/codixing search "BM25 scoring" --json --limit 20
time ./target/release/codixing search "how does the search pipeline work" --json --limit 20
```

**Step 2: Check binary sizes**

```bash
ls -lh target/release/codixing target/release/codixing-mcp target/release/codixing-lsp target/release/codixing-server
```

Large binary → check if tree-sitter grammars or embedding model weights are bundled. Consider `strip` on release binaries.

**Step 3: Check startup time of MCP server**

```bash
time echo '{"jsonrpc":"2.0","method":"initialize","id":1,"params":{}}' | \
  ./target/release/codixing-mcp --root .
```

If >200ms, investigate. The daemon mode should help, but cold start matters for first use.

**Step 4: Check for unnecessary dependencies**

Review `Cargo.toml` — are all dependencies still needed? Specifically:
- `qdrant-client` — behind feature flag, fine
- `fastembed` — large dep; is it compiled even when not used?
- `ort` — ONNX runtime; should be `load-dynamic` only

**Step 5: Run `cargo build --release` with timing**

```bash
cargo build --release --workspace --timings
```

Open `target/cargo-timings/cargo-timing.html` to see which crates are slow to build.

**Step 6: Consider stripping binaries for release**

```bash
strip target/release/codixing-mcp
ls -lh target/release/codixing-mcp
```

**Step 7: Commit any optimizations**

```bash
git add -A
git commit -m "perf: strip binaries, remove unused deps, optimize startup"
```

---

## Task 5: Update Website (docs/index.html)

Update the website with multi-language benchmark results, cleaner messaging, and launch-ready copy.

**Files:**
- Modify: `docs/index.html`

**Step 1: Update benchmark numbers**

The SWE-bench numbers (lines 1322-1334) are already correct at 48.0%/71.0%/74.3%. Add the multi-language results from Task 3 as a new card.

After the SWE-bench card (line ~1336), add a multi-language card:

```html
<div class="insight-card">
  <h3>Multi-language search quality</h3>
  <p style="margin-top:8px">Symbol localization across 5 languages:</p>
  <div style="margin-top:16px;display:grid;grid-template-columns:repeat(5,1fr);gap:12px;text-align:center">
    <div>
      <div style="font-size:1.5rem;font-weight:700;color:var(--cyan)">XX%</div>
      <div style="color:var(--text-muted);font-size:0.82rem">Rust</div>
    </div>
    <div>
      <div style="font-size:1.5rem;font-weight:700;color:var(--cyan)">XX%</div>
      <div style="color:var(--text-muted);font-size:0.82rem">Python</div>
    </div>
    <div>
      <div style="font-size:1.5rem;font-weight:700;color:var(--cyan)">XX%</div>
      <div style="color:var(--text-muted);font-size:0.82rem">TypeScript</div>
    </div>
    <div>
      <div style="font-size:1.5rem;font-weight:700;color:var(--cyan)">XX%</div>
      <div style="color:var(--text-muted);font-size:0.82rem">Go</div>
    </div>
    <div>
      <div style="font-size:1.5rem;font-weight:700;color:var(--cyan)">XX%</div>
      <div style="color:var(--text-muted);font-size:0.82rem">C++</div>
    </div>
  </div>
  <p style="margin-top:16px;color:var(--text-muted);font-size:0.85rem">Hit@5 for known symbol localization. 14 languages supported via tree-sitter AST.</p>
</div>
```

Replace XX% with actual numbers from Task 3.

**Step 2: Update language count**

Search for "10 language" references and update to "14 languages" (we now support Rust, Python, TS, JS, Go, Java, C, C++, C#, Ruby, Swift, Kotlin, Scala, Zig, PHP — but 14 with dedicated tree-sitter parsers).

**Step 3: Add "Closed Source" / "Free for open-source" messaging**

Update the CTA section to clarify the distribution model.

**Step 4: Update the Quick Start code examples**

Make sure the install command and MCP registration examples are up to date.

**Step 5: Commit**

```bash
git add docs/index.html
git commit -m "docs: update website with multi-language benchmarks and launch copy"
```

---

## Task 6: Update README.md

Streamline README for launch — focus on value prop, install, benchmark results, MCP integration.

**Files:**
- Modify: `README.md`

**Step 1: Remove GPU Acceleration section**

Lines ~388-407 — remove entirely. This is developer notes, not user-facing.

**Step 2: Update language table**

The Supported Languages table (lines 428-435) already shows all tiers. Verify it's accurate.

**Step 3: Add multi-language benchmark table**

After the SWE-bench table (lines 339-350), add:

```markdown
### Multi-Language Search Quality

| Language | Repos | Tasks | Hit@1 | Hit@5 |
|----------|-------|-------|-------|-------|
| Rust | tokio, ripgrep, axum | XX | XX% | XX% |
| Python | django, fastapi | XX | XX% | XX% |
| TypeScript | react, next.js | XX | XX% | XX% |
| Go | gin | XX | XX% | XX% |
| C++ | leveldb | XX | XX% | XX% |
```

**Step 4: Update Roadmap**

Add Phase 12 entry:

```markdown
| **Phase 12: Launch Prep** | ✅ Complete | Multi-language benchmarks, code cleanup, website update, binary distribution |
```

**Step 5: Trim unnecessary developer detail**

Remove or condense:
- The detailed GPU acceleration section
- Embedding benchmark tables that are too detailed for end users
- Move developer-specific info to a CONTRIBUTING.md if needed

**Step 6: Commit**

```bash
git add README.md
git commit -m "docs: streamline README for launch, add multi-language benchmarks"
```

---

## Task 7: Binary Distribution Setup

Set up `npx codixing-mcp` for easy MCP server installation.

**Files:**
- Create: `npm/package.json`
- Create: `npm/index.js` (binary dispatcher)
- Modify: `.github/workflows/release.yml` (add npm publish step)

**Step 1: Create npm package structure**

```
npm/
├── package.json
├── index.js          # Downloads + runs the right binary
└── README.md         # npm package readme
```

`npm/package.json`:
```json
{
  "name": "codixing-mcp",
  "version": "0.12.0",
  "description": "Code retrieval engine for AI agents — MCP server",
  "bin": {
    "codixing-mcp": "index.js"
  },
  "scripts": {
    "postinstall": "node index.js --install"
  },
  "os": ["darwin", "linux"],
  "cpu": ["x64", "arm64"],
  "license": "UNLICENSED",
  "repository": {
    "type": "git",
    "url": "https://github.com/ferax564/codixing"
  }
}
```

`npm/index.js`:
```javascript
#!/usr/bin/env node
/**
 * codixing-mcp npm wrapper
 *
 * Downloads the correct platform binary on install,
 * then proxies all calls to it.
 */
const { execFileSync, spawn } = require("child_process");
const { existsSync, mkdirSync, chmodSync } = require("fs");
const { join } = require("path");
const https = require("https");
const fs = require("fs");

const VERSION = require("./package.json").version;
const BIN_DIR = join(__dirname, "bin");
const BINARY_NAME = process.platform === "win32" ? "codixing-mcp.exe" : "codixing-mcp";
const BINARY_PATH = join(BIN_DIR, BINARY_NAME);

const PLATFORM_MAP = {
  "darwin-arm64": "codixing-mcp-macos-aarch64",
  "linux-x64": "codixing-mcp-linux-x86_64",
};

function getBinaryUrl() {
  const key = `${process.platform}-${process.arch}`;
  const artifact = PLATFORM_MAP[key];
  if (!artifact) {
    console.error(`Unsupported platform: ${key}`);
    process.exit(1);
  }
  return `https://github.com/ferax564/codixing/releases/download/v${VERSION}/${artifact}`;
}

function download(url, dest) {
  return new Promise((resolve, reject) => {
    const follow = (url) => {
      https.get(url, (res) => {
        if (res.statusCode === 302 || res.statusCode === 301) {
          return follow(res.headers.location);
        }
        if (res.statusCode !== 200) {
          return reject(new Error(`HTTP ${res.statusCode} for ${url}`));
        }
        const file = fs.createWriteStream(dest);
        res.pipe(file);
        file.on("finish", () => { file.close(); resolve(); });
      }).on("error", reject);
    };
    follow(url);
  });
}

async function install() {
  if (existsSync(BINARY_PATH)) return;
  mkdirSync(BIN_DIR, { recursive: true });
  const url = getBinaryUrl();
  console.log(`Downloading codixing-mcp v${VERSION}...`);
  await download(url, BINARY_PATH);
  chmodSync(BINARY_PATH, 0o755);
  console.log("codixing-mcp installed successfully.");
}

async function main() {
  if (process.argv.includes("--install")) {
    await install();
    return;
  }

  if (!existsSync(BINARY_PATH)) {
    await install();
  }

  const child = spawn(BINARY_PATH, process.argv.slice(2), {
    stdio: "inherit",
    env: process.env,
  });
  child.on("exit", (code) => process.exit(code || 0));
}

main().catch((err) => {
  console.error(err.message);
  process.exit(1);
});
```

**Step 2: Test locally**

```bash
cd npm && node index.js --install
```

**Step 3: Add npm publish to release workflow**

In `.github/workflows/release.yml`, after the release job, add:

```yaml
  npm-publish:
    name: Publish to npm
    needs: release
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with:
          node-version: 20
          registry-url: https://registry.npmjs.org
      - run: cd npm && npm publish
        env:
          NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}
```

**Step 4: Commit**

```bash
git add npm/ .github/workflows/release.yml
git commit -m "feat: add npm package for npx codixing-mcp distribution"
```

---

## Task 8: Update .mcp.json for Generic Distribution

The current `.mcp.json` has hardcoded paths. Create a template and update docs to show generic setup.

**Files:**
- Create: `mcp.json.example`
- Modify: `README.md` (MCP setup section)

**Step 1: Create mcp.json.example**

```json
{
  "mcpServers": {
    "codixing": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "codixing-mcp", "--root", "."]
    }
  }
}
```

**Step 2: Update README MCP section**

Replace current hardcoded path instructions with:

```markdown
## MCP Integration (Claude Code, Cursor, Windsurf)

### One-command setup (Claude Code)

```bash
claude mcp add codixing -- npx -y codixing-mcp --root .
```

### Manual setup

Add to your `.mcp.json` or MCP settings:

```json
{
  "mcpServers": {
    "codixing": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "codixing-mcp", "--root", "."]
    }
  }
}
```
```

**Step 3: Commit**

```bash
git add mcp.json.example README.md
git commit -m "docs: add generic MCP setup instructions for distribution"
```

---

## Task 9: License Change — Proprietary with Free Tier

**Files:**
- Modify: `LICENSE` (replace MIT with proprietary)
- Modify: `Cargo.toml` (update license field)
- Modify: `README.md` (update license section)

**Step 1: Create proprietary license**

Replace `LICENSE` content with a proprietary license that allows:
- Free use for open-source projects
- Free use for individual developers
- Paid license for commercial teams (>5 developers)

**Step 2: Update Cargo.toml**

Change `license = "MIT"` to `license = "LicenseRef-Codixing"` in workspace Cargo.toml.

**Step 3: Update README**

Replace the MIT license section at the bottom with:

```markdown
## License

Codixing is source-available with a proprietary license. Free for:
- Open-source projects
- Individual developers
- Teams up to 5 developers

[Contact us](mailto:hello@codixing.com) for commercial licensing.
```

**Step 4: Commit**

```bash
git add LICENSE Cargo.toml README.md
git commit -m "chore: switch from MIT to proprietary license with free tier"
```

---

## Task 10: Final Integration Test and Tag Release

**Step 1: Run all tests**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --check
```

**Step 2: Run all benchmarks**

```bash
# Multi-repo benchmark
python3 benchmarks/run_benchmark.py --skip-clone

# Multi-language eval
python3 benchmarks/multilang_eval.py

# SWE-bench (optional — takes 40 min)
python3 benchmarks/swe_bench_eval.py --limit 30 --embed-rerank "Salesforce/SweRankEmbed-Small"
```

**Step 3: Build release binaries locally**

```bash
cargo build --release --workspace
strip target/release/codixing target/release/codixing-mcp target/release/codixing-lsp target/release/codixing-server
ls -lh target/release/codixing*
```

**Step 4: Test MCP server end-to-end**

```bash
echo '{"jsonrpc":"2.0","method":"initialize","id":1,"params":{}}' | ./target/release/codixing-mcp --root .
```

Verify it responds with capabilities JSON.

**Step 5: Tag release**

```bash
git tag v0.12.0
git push origin main --tags
```

This triggers the release CI which builds linux-x86_64 + macos-aarch64 binaries and creates a GitHub Release.

---

## Risk Assessment

| Risk | Mitigation |
|------|-----------|
| Multi-language benchmarks show poor results for some languages | Tree-sitter support is mature for all 14 languages; BM25 is language-agnostic |
| npm binary download fails on some platforms | Support only linux-x64 and macos-arm64 initially (matches release CI) |
| License change confuses existing users | MIT → proprietary is a one-way change; document clearly in release notes |
| Binary size too large | Strip symbols (~50% reduction typically); consider `lto = true` in Cargo.toml |
| ONNX Runtime dependency for embeddings | BM25-only mode (default) has zero external deps; embeddings are opt-in |

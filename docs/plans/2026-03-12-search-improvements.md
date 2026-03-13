# Search Improvements + Install Script

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Improve search quality across languages (definition boost tuning, query expansion), explore SWE-bench R@1 improvement, harden daemon file watcher, and create install script.

**Architecture:** Incremental improvements to the search pipeline in `crates/core/src/engine/search.rs`, eval script improvements, and a new `install.sh` distribution script.

**Tech Stack:** Rust, Python (benchmarks), Bash (install script)

---

## Task 1: Multi-Language Definition Boost Tuning

The `apply_definition_boost` gives 2× to files that define query symbols. Test whether tuning this per search context improves results.

**Files:**
- Modify: `crates/core/src/engine/search.rs`

**Step 1: Check current definition boost**

Read `apply_definition_boost` in `search.rs`. Understand how it works — it looks up symbols matching query terms in the symbol table and boosts files that define them.

**Step 2: Increase definition boost from 2.0× to 3.0×**

Source files that *define* a symbol should strongly outrank files that merely *use* it. The current 2.0× may not be enough when test files have many keyword matches.

```rust
const DEFINITION_BOOST: f32 = 3.0;  // was 2.0
```

**Step 3: Run multi-language eval to compare**

```bash
cargo build --release --bin codixing
python3 benchmarks/multilang_eval.py
```

Compare Hit@1 before and after. If it improves, keep it. If not, revert.

**Step 4: Run SWE-bench 30-task check**

```bash
python3 benchmarks/swe_bench_eval.py --limit 30 --embed-rerank "Salesforce/SweRankEmbed-Small"
```

Verify SWE-bench R@1 doesn't degrade.

**Step 5: Commit if improvement**

```bash
git add crates/core/src/engine/search.rs
git commit -m "feat(search): increase definition boost from 2× to 3×"
```

---

## Task 2: Smarter Query Expansion (CamelCase + snake_case splitting)

When a user searches `"URLResolver"`, BM25 should also match `url_resolver` and `url`, `resolver` separately. Similarly `"getServerSideProps"` should match `get_server_side_props`.

**Files:**
- Modify: `crates/core/src/engine/search.rs` (or the retriever layer)

**Step 1: Check how queries are currently processed**

Read the BM25 retriever code to understand query tokenization. Tantivy's default tokenizer may already split on underscores but not CamelCase.

**Step 2: Add query expansion in the search dispatch**

Before passing the query to the retriever, expand CamelCase and snake_case terms:
- `URLResolver` → also search `url resolver`
- `getServerSideProps` → also search `get server side props`
- `ReactFiberBeginWork` → also search `react fiber begin work`

This should be a query expansion step, not a tokenizer change (we don't want to modify the index).

Approach: In the `search()` method, if the query contains CamelCase identifiers, create an expanded query that includes the split terms as additional keywords. Run BM25 with the expanded query.

```rust
fn expand_query(query: &str) -> String {
    let mut expanded = query.to_string();
    // Find CamelCase words and add split versions
    for word in query.split_whitespace() {
        if word.len() > 3 {
            let split = split_camel_case(word);
            if split.len() > 1 {
                expanded.push(' ');
                expanded.push_str(&split.join(" "));
            }
        }
    }
    expanded
}

fn split_camel_case(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() && i > 0 && !current.is_empty() {
            parts.push(current.to_lowercase());
            current.clear();
        }
        current.push(c);
    }
    if !current.is_empty() {
        parts.push(current.to_lowercase());
    }
    parts
}
```

**Step 3: Test with multi-language eval**

```bash
cargo build --release --bin codixing
python3 benchmarks/multilang_eval.py
```

**Step 4: SWE-bench check**

```bash
python3 benchmarks/swe_bench_eval.py --limit 30 --embed-rerank "Salesforce/SweRankEmbed-Small"
```

**Step 5: Commit**

```bash
git add crates/core/src/engine/search.rs
git commit -m "feat(search): add CamelCase query expansion for better cross-language matching"
```

---

## Task 3: SWE-bench R@1 Improvement — Code-Specific Reranking Research

The 69 tasks with gold in top-5 but not #1 are the biggest opportunity. ms-marco cross-encoders failed because they're trained on web search. Research code-specific alternatives.

**Files:**
- Modify: `benchmarks/swe_bench_eval.py` (experimental)

**Step 1: Analyze the 69 "reranking opportunity" tasks**

From the SWE-bench results, identify patterns in why the wrong file is #1:
- Is it test files? (addressed by 0.5× demotion)
- Is it a sibling file in the same directory?
- Is it a file that imports/is imported by the gold file?

Run a quick analysis:
```bash
python3 -c "
import json
with open('benchmarks/results/swe_bench_lite_eval.json') as f:
    data = json.load(f)
# Look at tasks where gold is in top-5 but not #1
rerank_ops = [t for t in data.get('tasks', []) if t.get('gold_rank', 99) > 1 and t.get('gold_rank', 99) <= 5]
print(f'Reranking opportunities: {len(rerank_ops)}')
for t in rerank_ops[:10]:
    print(f'  {t[\"instance_id\"]}: gold={t[\"gold_files\"]}, rank={t[\"gold_rank\"]}, top1={t[\"predicted\"][0] if t[\"predicted\"] else \"?\"}')
"
```

**Step 2: Try a simple heuristic reranker**

Instead of a ML cross-encoder, try a heuristic: among the top-5 non-test files, prefer the one whose path most closely matches identifiers in the problem statement.

For example, if the issue mentions `django.db.models.lookups`, and the top-5 has both `django/db/models/lookups.py` and `django/db/models/query.py`, the path-matching heuristic should pick `lookups.py`.

This is similar to the existing dotted path resolution but applied as a reranking step after BM25+embed.

```python
def path_match_rerank(problem: str, files: list[str]) -> list[str]:
    """Rerank by how well file paths match identifiers in the problem."""
    # Extract dotted paths and module references
    dotted = re.findall(r'[a-z]\w*(?:\.[a-z]\w*){2,}', problem)
    path_frags = set()
    for d in dotted:
        parts = d.split('.')
        for i in range(len(parts)):
            path_frags.add('/'.join(parts[i:]))

    if not path_frags:
        return files

    def score(fp):
        return sum(1 for frag in path_frags if frag in fp)

    top5 = files[:5]
    rest = files[5:]
    top5_scored = sorted(top5, key=lambda f: -score(f))

    # Only reorder if the best match is clearly better
    if score(top5_scored[0]) > score(top5[0]):
        return top5_scored + rest
    return files
```

**Step 3: Test on 30 SWE-bench tasks**

```bash
python3 benchmarks/swe_bench_eval.py --limit 30 --embed-rerank "Salesforce/SweRankEmbed-Small"
```

**Step 4: If it helps, run full 300**

**Step 5: Commit or revert**

```bash
git add benchmarks/swe_bench_eval.py
git commit -m "feat(bench): add path-match reranking heuristic for SWE-bench"
```

---

## Task 4: Harden Daemon File Watcher

The daemon file watcher (`crates/core/src/watcher/mod.rs`) handles live index updates. Verify it works robustly for large monorepos and fix any issues.

**Files:**
- Read: `crates/core/src/watcher/mod.rs`
- Possibly modify if issues found

**Step 1: Read and audit the watcher code**

Check for:
- Does it batch file changes? (Many editors save rapidly — debouncing needed)
- Does it handle deleted files?
- Does it respect .gitignore?
- What happens if indexing a file fails? (error handling)
- Memory usage for large repos (thousands of watched files)

**Step 2: Test with a large repo**

```bash
# Start daemon on django (2894 Python files)
./target/release/codixing-mcp --root benchmarks/repos/django &
# Modify a file
echo "# test" >> benchmarks/repos/django/django/db/models/query.py
# Wait 1s, then search to verify index updated
sleep 1
echo '{"jsonrpc":"2.0","method":"tools/call","id":1,"params":{"name":"code_search","arguments":{"query":"test comment","limit":5}}}' | ./target/release/codixing-mcp --root benchmarks/repos/django
# Clean up
git -C benchmarks/repos/django checkout django/db/models/query.py
```

**Step 3: Fix any issues found**

If debouncing is missing, add a 200ms debounce. If error handling is weak, add proper logging and continue-on-error.

**Step 4: Commit if changes made**

```bash
git add crates/core/src/watcher/mod.rs
git commit -m "fix(watcher): improve robustness for large monorepos"
```

---

## Task 5: Create Install Script

Create `docs/install.sh` that downloads the correct binary for the user's platform.

**Files:**
- Create: `docs/install.sh`

**Step 1: Create the install script**

```bash
#!/bin/sh
set -e

VERSION="0.12.0"
REPO="ferax564/codixing"
INSTALL_DIR="/usr/local/bin"

# Detect platform
OS="$(uname -s)"
ARCH="$(uname -m)"

case "${OS}-${ARCH}" in
  Linux-x86_64)   SUFFIX="linux-x86_64" ;;
  Darwin-arm64)   SUFFIX="macos-aarch64" ;;
  Darwin-x86_64)  SUFFIX="macos-x86_64" ;;
  *) echo "Unsupported platform: ${OS}-${ARCH}"; exit 1 ;;
esac

BINARIES="codixing codixing-mcp codixing-lsp codixing-server"
BASE_URL="https://github.com/${REPO}/releases/download/v${VERSION}"

echo "Installing Codixing v${VERSION} for ${OS}/${ARCH}..."

for bin in $BINARIES; do
  URL="${BASE_URL}/${bin}-${SUFFIX}"
  echo "  Downloading ${bin}..."
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "${URL}" -o "/tmp/${bin}"
  elif command -v wget >/dev/null 2>&1; then
    wget -q "${URL}" -O "/tmp/${bin}"
  else
    echo "Error: curl or wget required"; exit 1
  fi
  chmod +x "/tmp/${bin}"

  if [ -w "${INSTALL_DIR}" ]; then
    mv "/tmp/${bin}" "${INSTALL_DIR}/${bin}"
  else
    sudo mv "/tmp/${bin}" "${INSTALL_DIR}/${bin}"
  fi
done

echo ""
echo "Codixing installed to ${INSTALL_DIR}/"
echo ""
echo "Quick start:"
echo "  codixing init .              # Index current directory"
echo "  codixing search 'query'      # Search your code"
echo ""
echo "MCP integration (Claude Code):"
echo "  claude mcp add codixing -- codixing-mcp --root ."
echo ""
echo "Or use npx (no install needed):"
echo "  npx -y codixing-mcp --root ."
```

**Step 2: Test locally (dry run)**

```bash
bash -n docs/install.sh  # syntax check
```

**Step 3: Commit**

```bash
git add docs/install.sh
git commit -m "feat: add install.sh for binary distribution"
```

---

## Risk Assessment

| Risk | Mitigation |
|------|-----------|
| Definition boost too aggressive | Test on both multilang and SWE-bench before committing |
| Query expansion adds noise | Only expand CamelCase words >3 chars; test carefully |
| Path-match reranking hurts some tasks | Compare before/after on full 300 tasks |
| Watcher changes break existing behavior | Only modify if actual issues found |
| Install script download fails | Include both curl and wget fallbacks |

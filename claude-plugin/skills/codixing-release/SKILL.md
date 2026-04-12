---
name: codixing-release
description: "Complete Codixing release pipeline — version bump in 5 locations, tests, docs, CI review, benchmark, blog, X post, tag, publish. Use /codixing-release [version] to ship a new version."
user-invocable: true
disable-model-invocation: false
argument-hint: "[version]"
allowed-tools: Bash, Read, Edit, Write, Glob, Grep, Agent
---

# Codixing Release Pipeline

Ship a new Codixing version from code to announcement. Every step is automated.

<HARD-GATE>
Follow ALL steps. No skipping. No deferring. Every release that skipped steps needed fix PRs.
</HARD-GATE>

## Step 1: Pre-Flight

```bash
git status                    # must be clean
git log --oneline -10         # review what's shipping
```

Run each check SEPARATELY and verify exit code before proceeding:

```bash
cargo test --workspace        # count total tests from output
cargo clippy --workspace -- -D warnings
cargo fmt --check
```

If ANY fails, fix first. Do NOT pipe through awk/tail — that masks failures.

## Step 2: Version Bump (ALL 5 locations)

**Preferred: use the bump script** (prevents duplicate-key corruption):

```bash
python3 scripts/bump_version.py NEW_VERSION
```

Verify:
```bash
grep -rn "NEW_VERSION" Cargo.toml npm/package.json docs/install.sh \
  claude-plugin/.claude-plugin/plugin.json .claude-plugin/marketplace.json
```

(Manual fallback instructions below remain for reference.)

Determine version from git log since last tag. Then update ALL of:

1. `Cargo.toml` — `workspace.package.version`
2. `npm/package.json` — `"version"`
3. `docs/install.sh` — `VERSION=`
4. `claude-plugin/.claude-plugin/plugin.json` — `"version"`
5. `.claude-plugin/marketplace.json` — `metadata.version` AND `plugins[0].version`

Verify: `grep -rn "NEW_VERSION" Cargo.toml npm/package.json docs/install.sh claude-plugin/.claude-plugin/plugin.json .claude-plugin/marketplace.json`

## Step 3: Documentation Update

Update ALL of these. Grep for OLD counts to find strays:

- **README.md** — feature list, test count, tool count (grep `"[0-9]+ MCP tools"` and `"[0-9]+ tests"`)
- **CLAUDE.md** — version, test count, tool count
- **docs/index.html** — test count, tool count (multiple occurrences — check all)

```bash
# Find any remaining old counts:
grep -rn "OLD_TEST_COUNT\|OLD_TOOL_COUNT" README.md CLAUDE.md docs/index.html
```

## Step 4: Benchmark (if OpenClaw available)

Run from the repo root:

```bash
./target/release/codixing init benchmarks/repos/openclaw --no-embeddings --wait
python3 benchmarks/queue_v2_benchmark.py --repo openclaw
```

Report actual R@10 numbers. Do NOT predict. If OpenClaw not available, say "benchmark TBD."

## Step 5: Create PR

```bash
git checkout -b release/vX.Y.Z
git add -A
git commit -m "release: vX.Y.Z — [summary]

- [bullet points]"
git push -u origin release/vX.Y.Z
gh pr create --title "release: vX.Y.Z — [summary]" --body "..."
```

## Step 6: CI + Review Comments

<HARD-GATE>
MANDATORY after creating the PR:

1. Watch CI: `gh pr checks N` — wait until ALL green
2. Read review comments: `gh api repos/ferax564/codixing/pulls/N/comments | python3 -c "import json,sys; [print(f'{c[\"user\"][\"login\"]}: {c[\"body\"][:200]}') for c in json.load(sys.stdin)]"`
3. Fix ALL P1 and P2 issues
4. Push fixes, wait for CI again
5. Only proceed when ALL checks green AND ALL P1/P2 addressed
</HARD-GATE>

## Step 7: Merge + Tag

```bash
gh pr merge N --squash
git checkout main && git pull
# Auto-tag fires but GITHUB_TOKEN won't trigger release.yml. Re-push:
sleep 15 && git fetch --tags
git tag -d vX.Y.Z 2>/dev/null; git push origin :refs/tags/vX.Y.Z 2>/dev/null
git tag vX.Y.Z && git push origin vX.Y.Z
# Clean up branch:
git branch -d release/vX.Y.Z; git push origin --delete release/vX.Y.Z
```

## Step 8: Verify Release Artifacts

```bash
# Wait for release workflow:
gh run list --workflow release.yml --limit 1
# When complete:
gh release view vX.Y.Z
```

Update release notes:
```bash
gh release edit vX.Y.Z --notes "$(cat <<'EOF'
## What's New in vX.Y.Z

### [Feature 1]
[Description]

### [Feature 2]
[Description]

### Install
curl -fsSL https://codixing.com/install.sh | bash
npm install -g codixing

**Full Changelog**: https://github.com/ferax564/codixing/compare/vPREV...vX.Y.Z
EOF
)"
```

## Step 9: Blog Post

<HARD-GATE>
ASK the user: "What angle for the blog post? A) Benchmark B) Feature walkthrough C) Architecture D) Skip"
Do NOT write without asking. Do NOT use changelog framing.
</HARD-GATE>

Write to `docs/blog-*.html`. Commit and push to main.

## Step 10: X Post

```bash
# Check automarketing repo:
ls ~/code/automarketing/social/scripts/post.py
```

Create `~/code/automarketing/social/approved/YYYY-MM-DD-x-codixing-vXYZ.md`:

```markdown
---
platform: x
type: single
status: approved
created: YYYY-MM-DDTHH:MM:SSZ
tags: [codixing, release, vX.Y.Z, ai-agents, developer-tools, mcp]
---

[post content — casual first person, real numbers, not a press release]
```

```bash
cd ~/code/automarketing
python3 social/scripts/post.py --file social/approved/YYYY-MM-DD-x-codixing-vXYZ.md --dry-run
# Ask user approval, then:
python3 social/scripts/post.py --file social/approved/YYYY-MM-DD-x-codixing-vXYZ.md
```

## Final Checklist

- [ ] Version in all 5 locations
- [ ] Tests pass (Ubuntu + macOS + Windows)
- [ ] PR review comments addressed (P1/P2)
- [ ] README, CLAUDE.md, docs/index.html updated
- [ ] GitHub Release with notes
- [ ] Blog post published
- [ ] X post published
- [ ] Release branch cleaned up

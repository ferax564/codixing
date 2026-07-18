---
name: codixing-release
description: "Complete Codixing release pipeline — transactional version bump, tests, docs, CI review, benchmark, blog, X post, tag, publish. Use /codixing-release [version] to ship a new version."
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

## Step 2: Version Bump (ALL 14 fields across 7 files)

**Preferred: use the bump script** (prevents duplicate-key corruption):

```bash
python3 scripts/bump_version.py NEW_VERSION   # e.g. python3 scripts/bump_version.py 0.35.0
```

The script prints confirmation for each file and a verify command with the actual version substituted.

(Manual fallback instructions below remain for reference.)

Determine version from git log since last tag. Then update ALL of:

1. `Cargo.toml` — `workspace.package.version`
2. `Cargo.lock` — the source-less package versions for `codixing`, `codixing-core`, `codixing-lsp`, `codixing-mcp`, and `codixing-server`
3. `npm/package.json` — `"version"`
4. `editors/vscode/package.json` — `"version"`
5. `editors/vscode/package-lock.json` — top-level `version` AND `packages[""].version`
6. `claude-plugin/.claude-plugin/plugin.json` — `"version"`
7. `.claude-plugin/marketplace.json` — `metadata.version`, the Codixing plugin version, AND immutable `source.ref` (`vX.Y.Z`)

Verify (set `NEW_VERSION` to the version you just bumped to):
```bash
NEW_VERSION="0.35.0"   # replace with target
python3 scripts/check_version_consistency.py "$NEW_VERSION"
```

## Step 3: Documentation Update

Update the durable release-facing descriptions. Tool and test totals are
generated and change frequently, so do not copy exact counts into prose:

- **README.md** — feature list, install commands, and profile behavior
- **CLAUDE.md** — release workflow, version-field contract, and profile behavior
- **docs/index.html / docs/docs.html** — install commands and user-visible capabilities

```bash
# Verify the transactional version fields, then look for a stale old version:
python3 scripts/check_version_consistency.py "$NEW_VERSION"
codixing grep "OLD_VERSION" --literal
```

## Step 4: Retrieval Benchmark (if OpenClaw available)

Run from the repo root:

```bash
python3 benchmarks/queue_v2_benchmark.py --repo openclaw
```

The benchmark removes any existing OpenClaw index and creates the required
hybrid `bge-small-en` index before measuring concept accuracy. Do not pre-index
the fixture with a different configuration. Report actual Recall@10 numbers
from the generated Codixing retrieval and embedding report. Do NOT predict. If
OpenClaw is not available, say "benchmark TBD."

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
gh pr merge N --squash --delete-branch
git checkout main && git pull
# auto-tag.yml creates vX.Y.Z once from the release commit.
AUTO_TAG_RUN=$(gh run list --workflow auto-tag.yml --branch main --limit 1 \
  --json databaseId --jq '.[0].databaseId')
gh run watch "$AUTO_TAG_RUN"
git fetch --tags
git rev-parse vX.Y.Z
```

Do not delete or re-push the tag. `auto-tag.yml` runs only after the complete
main CI workflow succeeds, creates the tag at that verified commit, and then
dispatches `release.yml` with the tag.

## Step 8: Verify Release Artifacts

```bash
# Inspect the complete main CI run that produced the release artifacts:
gh run list --workflow ci.yml --branch main --limit 3
gh run watch RUN_ID

# Inspect auto-tag, then the release.yml run it dispatched:
gh run list --workflow auto-tag.yml --limit 3
gh run list --workflow release.yml --event workflow_dispatch --limit 3
gh run watch RELEASE_RUN_ID

# When the dispatched run completes:
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
curl --proto '=https' --proto-redir '=https' -fsSLo /tmp/codixing-install.sh https://codixing.com/install.sh
sh /tmp/codixing-install.sh
npm install -g codixing-mcp

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

- [ ] All 14 version fields across all 7 files are consistent
- [ ] Tests pass (Ubuntu + macOS + Windows)
- [ ] PR review comments addressed (P1/P2)
- [ ] README, CLAUDE.md, docs/index.html updated
- [ ] GitHub Release with notes
- [ ] Blog post published
- [ ] X post published
- [ ] Release branch cleaned up

# Go-to-Market Plan — Post v0.13.0

Created: 2026-03-21

## Week 1: Foundation (unblock everything)

### 1.1 Email forwarding (DAY 1)
- [ ] Set up `hello@codixing.com` → personal gmail forwarding
- [ ] Set up `security@codixing.com` → personal gmail forwarding
- [ ] Use Cloudflare Email Routing (free, if DNS is on Cloudflare) or ImprovMX
- [ ] Test by sending an email to both addresses
- WHY: LICENSE and SECURITY.md reference these. Without them, commercial inquiries and vulnerability reports go nowhere.

### 1.2 VS Code Marketplace publish (DAY 1-2)
- [ ] Create publisher account at marketplace.visualstudio.com
- [ ] `npm install -g @vscode/vsce`
- [ ] Update `editors/vscode/package.json`: add icon, publisher, repository, categories
- [ ] `cd editors/vscode && vsce package && vsce publish`
- [ ] Add install badge to README: `ext install codixing.codixing`
- WHY: VS Code Marketplace has 15M+ monthly active users. One-click install.

### 1.3 Submit to Anthropic plugin marketplace (DAY 2)
- [ ] Go to `platform.claude.com/plugins/submit` (or equivalent)
- [ ] Submit the `claude-plugin/` directory
- [ ] If accepted: users get `claude plugin install codixing` without adding marketplace first
- WHY: Removes friction for the primary audience (Claude Code users).

## Week 2: Launch

### 2.1 Write launch post (DAY 3-4)
Key narrative: "AI coding agents waste 99% of context tokens on grep output. Codixing fixes this."

Structure:
1. The problem (grep returns 225KB for a symbol lookup, agent burns context)
2. The solution (AST-aware search, dependency graph, token budgets)
3. One command to try it (`claude plugin marketplace add ferax564/codixing`)
4. Numbers (99% token reduction, 0.21s init, 48 tools, 20 languages, Windows support)
5. Open source, free for teams ≤5

### 2.2 Post on channels (DAY 5)
- [ ] Hacker News: "Show HN: Codixing — code retrieval for AI agents (99% fewer tokens than grep)"
- [ ] r/programming
- [ ] r/LocalLLaMA (if the local ONNX angle resonates)
- [ ] X/Twitter thread with the benchmark table
- [ ] Claude Code community/Discord (if one exists)
- [ ] Dev.to blog post (longer form)
- Time: weekday 9-10am ET for HN

### 2.3 Homebrew tap (DAY 5)
- [ ] Create `ferax564/homebrew-codixing` repo
- [ ] Add formula pointing to v0.13.0 release binaries
- [ ] Test `brew install ferax564/codixing/codixing`
- [ ] Add to README install section

## Week 3-4: Distribution

### 3.1 Update website for v0.13.0
- [ ] Add "What's New" section to codixing.com (or a changelog page)
- [ ] Document --medium mode, read-only access, Windows support, call graph, progress notifications
- [ ] Update docs.html MCP tools section with new capabilities
- [ ] Add Windows install instructions to docs

### 3.2 Codex marketplace / registry
- [ ] Check if OpenAI has a Codex plugin/skill registry
- [ ] If yes, submit Codixing
- [ ] If no, ensure the README Codex instructions are prominent

### 3.3 Integration guides
- [ ] Blog post: "How to set up Codixing with Claude Code" (step by step with screenshots)
- [ ] Blog post: "Codixing vs grep: benchmarks on real codebases"
- [ ] Blog post: "Using Codixing's dependency graph for code review"

## Month 2: Growth

### 4.1 Community
- [ ] Add GitHub Discussions to the repo
- [ ] Create a Discord server (or use GitHub Discussions exclusively)
- [ ] Respond to issues and PRs within 24 hours

### 4.2 Commercial
- [ ] Create a pricing page on codixing.com
- [ ] Tiers: Free (teams ≤5), Pro ($X/month, teams >5), Enterprise (custom)
- [ ] Add Stripe checkout or "Contact us" form for Pro tier
- [ ] Legal: review BSL license terms with a lawyer

### 4.3 Analytics
- [ ] Track npm download counts weekly
- [ ] Track GitHub stars, clones, and traffic via GitHub Insights
- [ ] Optional: add opt-in anonymous telemetry on `codixing init` (OS, file count, version)
- [ ] Set up a simple dashboard (even a Google Sheet tracking weekly numbers)

### 4.4 Partnerships
- [ ] Reach out to Continue.dev team about featuring Codixing as a recommended MCP server
- [ ] Reach out to Cursor team about built-in integration
- [ ] Write for the Anthropic blog if they have a developer ecosystem section

## Success metrics (end of month 2)

| Metric | Target |
|--------|--------|
| GitHub stars | 500+ |
| npm weekly downloads | 200+ |
| VS Code installs | 100+ |
| Plugin installs (Claude Code) | 50+ |
| Commercial inquiries | 5+ |
| Community members | 20+ |

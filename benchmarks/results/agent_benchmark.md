# Codixing Agent Benchmark Report

**Date:** 2026-03-29 09:11
**Model:** claude-sonnet-4-6
**Runs per task per condition:** 3

## Summary

| Metric | Vanilla (mean) | Codixing (mean) | Reduction |
|--------|----------------|-----------------|-----------|
| Tool calls | 24.4 | 4.6 | **81% fewer** |
| Tokens | 8,153 | 2,964 | **64% fewer** |
| Wall time | 148.2s | 41.9s | **72% faster** |
| Pass rate | 92% | 100% | **+8%** |

## Efficiency Results

| Task | Repo | Category | V Calls (mean+/-std) | C Calls (mean+/-std) | Call Reduction | Significant? |
|------|------|----------|----------------------|----------------------|----------------|--------------|
| grep-impossible-complexity-1 | openclaw | complexity | 23.3 +/- 2.3 | 4.0 +/- 0.0 | 83% | Yes |
| grep-impossible-transitive-1 | openclaw | transitive_impact | 64.7 +/- 38.6 | 6.7 +/- 0.6 | 90% | Yes |
| structural-blast-radius-openclaw-1 | openclaw | blast_radius | 7.0 +/- 2.0 | 5.7 +/- 3.1 | 19% | No |
| structural-callers-openclaw-1 | openclaw | caller_completeness | 2.7 +/- 0.6 | 2.0 +/- 0.0 | 25% | Yes |

## Structural Accuracy (Ground Truth Recall)

These tasks have known correct answers. **Recall** = fraction of ground truth items the agent found. Higher is better.

| Task | Category | Ground Truth | V Recall (mean) | C Recall (mean) | V Found | C Found | Delta |
|------|----------|-------------|-----------------|------------------|---------|---------|-------|
| grep-impossible-complexity-1 | complexity | 3 items | 44% | 100% | 1.3/3 | 3.0/3 | **+56%** |
| grep-impossible-transitive-1 | transitive_impact | 12 items | 72% | 100% | 8.7/12 | 12.0/12 | **+28%** |
| structural-blast-radius-openclaw-1 | blast_radius | 15 items | 67% | 96% | 10.0/15 | 14.3/15 | **+29%** |
| structural-callers-openclaw-1 | caller_completeness | 19 items | 100% | 98% | 19.0/19 | 18.7/19 | **-2%** |

**Average structural recall:** Vanilla 71% → Codixing 98% (**+28%**)

### Items Missed by Vanilla Agent

- **grep-impossible-complexity-1**: `createConfigIO`, `sanitizeChatHistoryMessage`
- **grep-impossible-transitive-1**: `extensions/telegram/src/bot-native-commands.ts`, `src/agents/subagent-control.ts`, `src/auto-reply/reply/agent-runner-payloads.ts`, `src/auto-reply/reply/followup-runner.ts`
- **structural-blast-radius-openclaw-1**: `extensions/discord/src/shared.ts`, `extensions/imessage/src/channel.ts`, `extensions/irc/src/channel.ts`, `extensions/matrix/src/channel.ts`, `extensions/mattermost/src/channel.ts`, `extensions/nostr/src/channel.ts`, `extensions/signal/src/channel.ts`, `extensions/slack/src/channel.ts`, `extensions/whatsapp/src/channel.ts`, `extensions/zalo/src/channel.runtime.ts`, `src/auto-reply/command-auth.ts`, `src/auto-reply/commands-registry.data.ts`, `src/channels/plugins/bundled.ts`, `src/channels/plugins/registry.ts`, `src/channels/plugins/setup-registry.ts`

## Cost

**Total:** $4.42
**Per session:** $0.184
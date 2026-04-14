# Codixing Large-Repo Agent Benchmark

**Date:** 2026-04-13 22:57
**Model:** claude-sonnet-4-6
**Runs per task per mode:** 1

## Summary

| Metric | vanilla | codixing-sticky |
|---|---|---|
| Tool calls (mean) | 13.2 | 4.5 |
| Tokens (mean) | 11,318 | 3,836 |
| Wall time (mean) | 143.8s | 52.2s |
| Recall (mean) | 85% | 90% |

### Deltas vs vanilla

| Mode | Calls | Tokens | Time | Recall |
|---|---|---|---|---|
| codixing-sticky | +66% | +66% | +64% | +5pp |

## Per-Task Results

| Task | Repo | Cat | vanilla calls | vanilla tok | vanilla rec | codixing-sticky calls | codixing-sticky tok | codixing-sticky rec |
|---|---|---|---|---|---|---|---|---|
| march-blast-radius | openclaw | blast_radius | 6.0 | 14,107 | 100% | 4.0 | 6,896 | 100% |
| march-callers | openclaw | caller_completeness | 2.0 | 2,647 | 100% | 3.0 | 4,636 | 100% |
| march-complexity | openclaw | complexity | 24.0 | 19,922 | 100% | 4.0 | 857 | 100% |
| march-transitive | openclaw | transitive_impact | 21.0 | 8,595 | 40% | 7.0 | 2,957 | 60% |

## Tool Breakdown (codixing-sticky)

- **march-blast-radius** → `{'ToolSearch': 1, 'mcp__codixing__find_symbol': 1, 'mcp__codixing__search_usages': 1, 'mcp__codixing__get_references': 1}`
- **march-callers** → `{'ToolSearch': 1, 'mcp__codixing__find_symbol': 1, 'mcp__codixing__search_usages': 1}`
- **march-complexity** → `{'ToolSearch': 1, 'mcp__codixing__get_complexity': 3}`
- **march-transitive** → `{'ToolSearch': 1, 'mcp__codixing__change_impact': 1, 'mcp__codixing__get_references': 5}`

## Missed Ground-Truth Items

- **march-transitive** [vanilla] missed: src/channels/plugins/registry, src/channels/plugins/bundled, src/agents/
- **march-transitive** [codixing-sticky] missed: src/channels/plugins/registry, src/channels/plugins/bundled

**Total cost:** $1.56  (8 sessions)
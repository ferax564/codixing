# Codixing Large-Repo Agent Benchmark

**Date:** 2026-04-13 22:43
**Model:** claude-sonnet-4-6
**Runs per task per mode:** 1

## Summary

| Metric | vanilla | codixing-sticky |
|---|---|---|
| Tool calls (mean) | 12.5 | 14.0 |
| Tokens (mean) | 8,265 | 9,023 |
| Wall time (mean) | 105.0s | 155.3s |
| Recall (mean) | 85% | 85% |

### Deltas vs vanilla

| Mode | Calls | Tokens | Time | Recall |
|---|---|---|---|---|
| codixing-sticky | -12% | -9% | -48% | +0pp |

## Per-Task Results

| Task | Repo | Cat | vanilla calls | vanilla tok | vanilla rec | codixing-sticky calls | codixing-sticky tok | codixing-sticky rec |
|---|---|---|---|---|---|---|---|---|
| march-blast-radius | openclaw | blast_radius | 7.0 | 10,354 | 100% | 21.0 | 18,137 | 80% |
| march-callers | openclaw | caller_completeness | 3.0 | 1,455 | 100% | 3.0 | 2,672 | 100% |
| march-complexity | openclaw | complexity | 24.0 | 16,523 | 100% | 25.0 | 11,345 | 100% |
| march-transitive | openclaw | transitive_impact | 16.0 | 4,729 | 40% | 7.0 | 3,938 | 60% |

## Tool Breakdown (codixing-sticky)

- **march-blast-radius** → `{'ToolSearch': 1, 'mcp__codixing__find_symbol': 1, 'mcp__codixing__get_references': 1, 'mcp__codixing__change_impact': 1, 'Read': 2, 'mcp__codixing__grep_code': 14, 'mcp__codixing__list_files': 1}`
- **march-callers** → `{'ToolSearch': 1, 'mcp__codixing__find_symbol': 1, 'mcp__codixing__code_search': 1}`
- **march-complexity** → `{'ToolSearch': 1, 'mcp__codixing__outline_file': 3, 'mcp__codixing__read_file': 9, 'Read': 12}`
- **march-transitive** → `{'ToolSearch': 1, 'mcp__codixing__change_impact': 1, 'mcp__codixing__get_references': 5}`

## Missed Ground-Truth Items

- **march-blast-radius** [codixing-sticky] missed: src/channels/plugins/types
- **march-transitive** [vanilla] missed: src/channels/plugins/registry, src/channels/plugins/bundled, src/agents/
- **march-transitive** [codixing-sticky] missed: src/channels/plugins/registry, src/channels/plugins/bundled

**Total cost:** $2.53  (8 sessions)
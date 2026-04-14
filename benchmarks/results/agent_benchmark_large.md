# Codixing Large-Repo Agent Benchmark

**Date:** 2026-04-13 21:55
**Model:** claude-sonnet-4-6
**Runs per task per mode:** 1

## Summary

| Metric | vanilla | codixing | codixing-sticky |
|---|---|---|---|
| Tool calls (mean) | 9.5 | 8.4 | 8.2 |
| Tokens (mean) | 2,595 | 2,543 | 2,979 |
| Wall time (mean) | 53.4s | 56.0s | 61.8s |
| Recall (mean) | 100% | 100% | 94% |

### Deltas vs vanilla

| Mode | Calls | Tokens | Time | Recall |
|---|---|---|---|---|
| codixing | +12% | +2% | -5% | +0pp |
| codixing-sticky | +13% | -15% | -16% | -6pp |

## Per-Task Results

| Task | Repo | Cat | vanilla calls | vanilla tok | vanilla rec | codixing calls | codixing tok | codixing rec | codixing-sticky calls | codixing-sticky tok | codixing-sticky rec |
|---|---|---|---|---|---|---|---|---|---|---|---|
| lx-arch-1 | linux | architecture | 7.0 | 2,737 | 100% | 6.0 | 2,579 | 100% | 6.0 | 2,510 | 100% |
| lx-callers-1 | linux | caller_completeness | 1.0 | 544 | 100% | 1.0 | 552 | 100% | 2.0 | 752 | 100% |
| lx-concept-1 | linux | concept_search | 30.0 | 920 | 100% | 18.0 | 671 | 100% | 4.0 | 922 | 100% |
| lx-symbol-1 | linux | symbol_lookup | 9.0 | 1,709 | 100% | 6.0 | 1,345 | 100% | 4.0 | 1,073 | 100% |
| oc-blast-1 | openclaw | blast_radius | 3.0 | 8,622 | 100% | 5.0 | 10,522 | 100% | 16.0 | 9,533 | 80% |
| oc-callers-1 | openclaw | caller_completeness | 3.0 | 1,568 | 100% | 1.0 | 2,611 | 100% | 3.0 | 2,881 | 75% |
| oc-concept-1 | openclaw | concept_search | 13.0 | 2,951 | 100% | 16.0 | 1,221 | 100% | 26.0 | 4,915 | 100% |
| oc-symbol-1 | openclaw | symbol_lookup | 10.0 | 1,709 | 100% | 14.0 | 845 | 100% | 5.0 | 1,248 | 100% |

## Tool Breakdown (codixing-sticky)

- **lx-arch-1** → `{'ToolSearch': 1, 'mcp__codixing__get_repo_map': 1, 'mcp__codixing__list_files': 2, 'Bash': 2}`
- **lx-callers-1** → `{'ToolSearch': 1, 'mcp__codixing__grep_code': 1}`
- **lx-concept-1** → `{'ToolSearch': 2, 'mcp__codixing__code_search': 1, 'mcp__codixing__find_symbol': 1}`
- **lx-symbol-1** → `{'ToolSearch': 1, 'mcp__codixing__find_symbol': 1, 'mcp__codixing__read_file': 1, 'Read': 1}`
- **oc-blast-1** → `{'ToolSearch': 2, 'mcp__codixing__find_symbol': 1, 'mcp__codixing__change_impact': 1, 'mcp__codixing__get_references': 2, 'Read': 2, 'mcp__codixing__grep_code': 8}`
- **oc-callers-1** → `{'ToolSearch': 1, 'mcp__codixing__find_symbol': 1, 'mcp__codixing__code_search': 1}`
- **oc-concept-1** → `{'ToolSearch': 2, 'mcp__codixing__code_search': 5, 'mcp__codixing__read_file': 13, 'mcp__codixing__outline_file': 1, 'mcp__codixing__find_symbol': 5}`
- **oc-symbol-1** → `{'ToolSearch': 2, 'mcp__codixing__find_symbol': 1, 'mcp__codixing__outline_file': 1, 'Read': 1}`

## Missed Ground-Truth Items

- **oc-blast-1** [codixing-sticky] missed: src/auto-reply/commands-registry.data.ts
- **oc-callers-1** [codixing-sticky] missed: src/channels/plugins/index.ts

**Total cost:** $2.67  (24 sessions)
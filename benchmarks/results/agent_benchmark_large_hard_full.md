# Codixing Large-Repo Agent Benchmark

**Date:** 2026-04-13 23:18
**Model:** claude-sonnet-4-6
**Runs per task per mode:** 1

## Summary

| Metric | vanilla | codixing-sticky |
|---|---|---|
| Tool calls (mean) | 14.1 | 6.6 |
| Tokens (mean) | 2,806 | 3,132 |
| Wall time (mean) | 81.4s | 56.0s |
| Recall (mean) | 61% | 70% |

### Deltas vs vanilla

| Mode | Calls | Tokens | Time | Recall |
|---|---|---|---|---|
| codixing-sticky | +54% | -12% | +31% | +9pp |

## Per-Task Results

| Task | Repo | Cat | vanilla calls | vanilla tok | vanilla rec | codixing-sticky calls | codixing-sticky tok | codixing-sticky rec |
|---|---|---|---|---|---|---|---|---|
| hard-lx-mm-blast | linux | blast_radius | 20.0 | 1,375 | 14% | 6.0 | 2,478 | 43% |
| hard-lx-page-fault-archs | linux | arch_sweep | 18.0 | 3,241 | 100% | 5.0 | 2,446 | 90% |
| hard-lx-rcu-gp | linux | concept_search | 10.0 | 993 | 100% | 8.0 | 1,934 | 100% |
| hard-lx-syscall-openat | linux | macro_symbol | 1.0 | 487 | 67% | 2.0 | 747 | 67% |
| hard-lx-write-iter-fs | linux | interface_implementers | 1.0 | 1,182 | 100% | 2.0 | 3,010 | 100% |
| hard-oc-2hop-transitive | openclaw | transitive_impact | 13.0 | 1,301 | 33% | 7.0 | 3,183 | 50% |
| hard-oc-complexity | openclaw | complexity | 20.0 | 11,792 | 100% | 4.0 | 1,015 | 100% |
| hard-oc-exec-approval-flow | openclaw | concept_cross_file | 30.0 | 1,251 | 33% | 22.0 | 4,714 | 33% |
| hard-oc-types-blast | openclaw | blast_radius | 14.0 | 3,635 | 0% | 3.0 | 8,659 | 44% |

## Tool Breakdown (codixing-sticky)

- **hard-lx-mm-blast** → `{'ToolSearch': 1, 'mcp__codixing__change_impact': 1, 'mcp__codixing__search_usages': 1, 'mcp__codixing__find_symbol': 1, 'Bash': 1, 'mcp__codixing__symbol_callers': 1}`
- **hard-lx-page-fault-archs** → `{'ToolSearch': 1, 'mcp__codixing__grep_code': 3, 'mcp__codixing__find_symbol': 1}`
- **hard-lx-rcu-gp** → `{'ToolSearch': 2, 'mcp__codixing__code_search': 2, 'mcp__codixing__find_symbol': 1, 'mcp__codixing__read_symbol': 1, 'Read': 2}`
- **hard-lx-syscall-openat** → `{'ToolSearch': 1, 'mcp__codixing__grep_code': 1}`
- **hard-lx-write-iter-fs** → `{'ToolSearch': 1, 'mcp__codixing__grep_code': 1}`
- **hard-oc-2hop-transitive** → `{'ToolSearch': 1, 'mcp__codixing__change_impact': 1, 'mcp__codixing__get_references': 5}`
- **hard-oc-complexity** → `{'ToolSearch': 1, 'mcp__codixing__get_complexity': 3}`
- **hard-oc-exec-approval-flow** → `{'ToolSearch': 1, 'mcp__codixing__code_search': 3, 'mcp__codixing__find_symbol': 4, 'mcp__codixing__read_file': 2, 'mcp__codixing__outline_file': 3, 'Read': 7, 'mcp__codixing__read_symbol': 2}`
- **hard-oc-types-blast** → `{'ToolSearch': 1, 'mcp__codixing__get_references': 1, 'mcp__codixing__search_usages': 1}`

## Missed Ground-Truth Items

- **hard-lx-mm-blast** [vanilla] missed: mm/memory.c, mm/vmscan.c, kernel/fork.c, fs/proc/task_mmu.c, 788, 700
- **hard-lx-mm-blast** [codixing-sticky] missed: mm/memory.c, mm/vmscan.c, kernel/fork.c, fs/proc/task_mmu.c
- **hard-lx-page-fault-archs** [codixing-sticky] missed: arch/s390/
- **hard-lx-syscall-openat** [vanilla] missed: do_sys_openat2
- **hard-lx-syscall-openat** [codixing-sticky] missed: do_sys_openat2
- **hard-oc-2hop-transitive** [vanilla] missed: src/channels/plugins/registry.ts, src/channels/plugins/bundled.ts, src/agents/, src/commands/
- **hard-oc-2hop-transitive** [codixing-sticky] missed: src/channels/plugins/registry.ts, src/channels/plugins/bundled.ts, src/commands/
- **hard-oc-exec-approval-flow** [vanilla] missed: src/infra/exec-approval, src/infra/exec
- **hard-oc-exec-approval-flow** [codixing-sticky] missed: src/infra/exec-approval, src/infra/exec
- **hard-oc-types-blast** [vanilla] missed: src/channels/registry.ts, src/channels/plugins/registry.ts, src/channels/plugins/bundled.ts, src/channels/plugins/catalog.ts, src/channels/plugins/setup-wizard.ts, src/channels/plugins/load.ts, src/channels/plugins/helpers.ts, src/channels/plugins/binding-types.ts, src/channels/plugins/outbound/load.ts, src/channels/plugins/message-action-dispatch.ts, src/channels/plugins/config-writes.ts, src/channels/plugins/pairing.ts, src/channels/plugins/whatsapp-shared.ts, src/channels/plugins/bluebubbles-actions.ts, src/channels/plugins/status-issues/shared.ts, 160
- **hard-oc-types-blast** [codixing-sticky] missed: src/channels/plugins/catalog.ts, src/channels/plugins/setup-wizard.ts, src/channels/plugins/outbound/load.ts, src/channels/plugins/message-action-dispatch.ts, src/channels/plugins/config-writes.ts, src/channels/plugins/pairing.ts, src/channels/plugins/whatsapp-shared.ts, src/channels/plugins/bluebubbles-actions.ts, src/channels/plugins/status-issues/shared.ts

**Total cost:** $2.33  (18 sessions)
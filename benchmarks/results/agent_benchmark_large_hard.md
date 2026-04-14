# Codixing Large-Repo Agent Benchmark

**Date:** 2026-04-13 22:19
**Model:** claude-sonnet-4-6
**Runs per task per mode:** 1

## Summary

| Metric | vanilla | codixing-sticky |
|---|---|---|
| Tool calls (mean) | 14.9 | 7.1 |
| Tokens (mean) | 2,250 | 3,656 |
| Wall time (mean) | 63.2s | 57.7s |
| Recall (mean) | 60% | 74% |

### Deltas vs vanilla

| Mode | Calls | Tokens | Time | Recall |
|---|---|---|---|---|
| codixing-sticky | +52% | -62% | +9% | +15pp |

## Per-Task Results

| Task | Repo | Cat | vanilla calls | vanilla tok | vanilla rec | codixing-sticky calls | codixing-sticky tok | codixing-sticky rec |
|---|---|---|---|---|---|---|---|---|
| hard-lx-mm-blast | linux | blast_radius | 18.0 | 1,208 | 43% | 6.0 | 2,371 | 57% |
| hard-lx-page-fault-archs | linux | arch_sweep | 11.0 | 3,296 | 100% | 9.0 | 2,332 | 70% |
| hard-lx-rcu-gp | linux | concept_search | 21.0 | 1,124 | 100% | 10.0 | 2,651 | 100% |
| hard-lx-syscall-openat | linux | macro_symbol | 1.0 | 414 | 67% | 4.0 | 1,111 | 67% |
| hard-lx-write-iter-fs | linux | interface_implementers | 1.0 | 1,389 | 100% | 2.0 | 2,819 | 100% |
| hard-oc-2hop-transitive | openclaw | transitive_impact | 14.0 | 4,337 | 33% | 7.0 | 3,352 | 50% |
| hard-oc-exec-approval-flow | openclaw | concept_cross_file | 34.0 | 1,385 | 33% | 17.0 | 8,792 | 100% |
| hard-oc-types-blast | openclaw | blast_radius | 19.0 | 4,851 | 0% | 2.0 | 5,822 | 50% |

## Tool Breakdown (codixing-sticky)

- **hard-lx-mm-blast** → `{'ToolSearch': 1, 'mcp__codixing__change_impact': 1, 'mcp__codixing__get_references': 1, 'mcp__codixing__code_search': 1, 'mcp__codixing__predict_impact': 1, 'Bash': 1}`
- **hard-lx-page-fault-archs** → `{'ToolSearch': 2, 'mcp__codixing__code_search': 1, 'mcp__codixing__grep_code': 6}`
- **hard-lx-rcu-gp** → `{'ToolSearch': 2, 'mcp__codixing__code_search': 3, 'mcp__codixing__find_symbol': 3, 'Read': 2}`
- **hard-lx-syscall-openat** → `{'ToolSearch': 2, 'mcp__codixing__code_search': 1, 'mcp__codixing__grep_code': 1}`
- **hard-lx-write-iter-fs** → `{'ToolSearch': 1, 'mcp__codixing__grep_code': 1}`
- **hard-oc-2hop-transitive** → `{'ToolSearch': 1, 'mcp__codixing__change_impact': 1, 'mcp__codixing__get_references': 5}`
- **hard-oc-exec-approval-flow** → `{'ToolSearch': 1, 'mcp__codixing__get_repo_map': 1, 'mcp__codixing__code_search': 2, 'mcp__codixing__read_file': 2, 'Read': 6, 'mcp__codixing__find_symbol': 5}`
- **hard-oc-types-blast** → `{'ToolSearch': 1, 'mcp__codixing__get_references': 1}`

## Missed Ground-Truth Items

- **hard-lx-mm-blast** [vanilla] missed: kernel/fork.c, fs/proc/task_mmu.c, 788, 700
- **hard-lx-mm-blast** [codixing-sticky] missed: mm/vmscan.c, kernel/fork.c, fs/proc/task_mmu.c
- **hard-lx-page-fault-archs** [codixing-sticky] missed: arch/x86/, arch/riscv/, arch/s390/
- **hard-lx-syscall-openat** [vanilla] missed: do_sys_openat2
- **hard-lx-syscall-openat** [codixing-sticky] missed: do_sys_openat2
- **hard-oc-2hop-transitive** [vanilla] missed: src/channels/plugins/registry.ts, src/channels/plugins/bundled.ts, src/agents/, src/commands/
- **hard-oc-2hop-transitive** [codixing-sticky] missed: src/channels/plugins/registry.ts, src/channels/plugins/bundled.ts, src/commands/
- **hard-oc-exec-approval-flow** [vanilla] missed: src/infra/exec-approval, src/infra/exec
- **hard-oc-types-blast** [vanilla] missed: src/channels/registry.ts, src/channels/plugins/registry.ts, src/channels/plugins/bundled.ts, src/channels/plugins/catalog.ts, src/channels/plugins/setup-wizard.ts, src/channels/plugins/load.ts, src/channels/plugins/helpers.ts, src/channels/plugins/binding-types.ts, src/channels/plugins/outbound/load.ts, src/channels/plugins/message-action-dispatch.ts, src/channels/plugins/config-writes.ts, src/channels/plugins/pairing.ts, src/channels/plugins/whatsapp-shared.ts, src/channels/plugins/bluebubbles-actions.ts, src/channels/plugins/status-issues/shared.ts, 160
- **hard-oc-types-blast** [codixing-sticky] missed: src/channels/plugins/helpers.ts, src/channels/plugins/outbound/load.ts, src/channels/plugins/message-action-dispatch.ts, src/channels/plugins/config-writes.ts, src/channels/plugins/pairing.ts, src/channels/plugins/whatsapp-shared.ts, src/channels/plugins/bluebubbles-actions.ts, src/channels/plugins/status-issues/shared.ts

**Total cost:** $2.01  (16 sessions)
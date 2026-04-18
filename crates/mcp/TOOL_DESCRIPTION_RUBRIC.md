# MCP Tool Description Rubric

Every tool `description` field in `crates/mcp/tool_defs/*.toml` must meet
this rubric. It exists because tool-description quality has measurable
impact on agent behaviour — the survey in arXiv 2602.14878 recorded
**+5.85 pp task success** and **+15.12 % evaluator score** from
disciplined descriptions, and found 56 % of surveyed MCP tools had
"Unclear Purpose". Codixing ships 67 tools, too many for agents to
distinguish without disciplined text.

The rubric test lives at
[`crates/mcp/tests/tool_description_rubric.rs`](tests/tool_description_rubric.rs)
and runs as part of `cargo test`. Tools that fail it block the build.

## Four checks

Each description must satisfy **all four** of the following.

### 1. Purpose — action-first

First sentence is one action verb + object. No preamble, no "This tool…".

- ✅ `Find all code locations where a symbol is referenced or called.`
- ❌ `This tool is used to search for code locations.`

### 2. Activation criteria — "Use when …"

Include at least one trigger phrase telling the agent **when** to pick
this tool over a neighbour. Accepted phrasings (case-insensitive):

- `use when`, `use this`, `useful when`, `useful for`
- `essential for`, `ideal for`
- `unlike`, `instead of`, `prefer this`
- `tip:`
- `when you`, `when the`
- `needed for`

The assertion test is satisfied if the description contains any of
those phrases. In practice, prefer `Use when …` followed by a concrete
scenario. If the tool overlaps with another, add a contrast clause
(`— prefer X when you need Y`).

- ✅ `Use when you need the full deterministic set of callers; prefer
   `symbol_callers` for the ranked top-K view.`
- ❌ `Returns all callers of the symbol.` (no activation signal)

### 3. Parameter semantics — behavioural

Every param's own `description` should explain what the value **means
for the tool's behaviour**, not just its type.

- ✅ `limit`: `Maximum number of usage locations to return (default:
   20). Ignored when 'complete=true'.`
- ❌ `limit`: `An integer.`

Call out default values and any cross-parameter interactions
(e.g. "ignored when X").

### 4. Limitations — called out inline

If the tool has known limitations (stale ranking, graph required,
requires a federation, slower on large repos, deprecated), say so in
the description. Agents must not discover limitations the hard way.

- ✅ `Requires graph intelligence to be enabled.`
- ✅ `Deprecated: use assemble_context instead.`

## Writing a new tool

When you add a tool to `tool_defs/*.toml`:

1. Write the description against this rubric.
2. Run `cargo test -p codixing-mcp --test tool_description_rubric` —
   should pass immediately.
3. If it fails, read the failure message. It names the tool and the
   missing check.

## Batch-audit workflow

When running a rubric pass across the whole surface:

```bash
# List current descriptions
grep -E '^description = ' crates/mcp/tool_defs/*.toml

# Run the assertion
cargo test -p codixing-mcp --test tool_description_rubric -- --nocapture
```

Failures print the offending tool name + current description so you
can edit the right TOML quickly.

import json, subprocess, sys, tiktoken
enc = tiktoken.get_encoding("cl100k_base")
t = lambda s: len(enc.encode(s, disallowed_special=()))
task = sys.argv[1]
raw = subprocess.run(["codixing", "ask", task] + sys.argv[2:], capture_output=True, text=True).stdout
p = json.loads(raw)
compact = json.dumps(p, separators=(",", ":"))
# per-section pretty cost
sec = {k: t(json.dumps(v, indent=2)) for k, v in p.items()}
# lean rendering: only fields that carry information
lines = [f"# {p['mode']} pack (budget {p['token_budget']})"]
for e in p["must_read"]:
    sig = (e.get("signature") or "").split("\n")[0]
    lines.append(f"{e['id']} {e['path']}:{e['start_line']}-{e['end_line']} [{e['kind']}] {sig}")
syms = [s for s in p["related_symbols"] if s["kind"] != "import"]
if syms:
    lines.append("symbols:")
    for s in syms:
        lines.append(f"  {s['path']}:{s['range']} {s.get('signature') or s['name']}")
if p["likely_tests"]:
    lines.append("tests:")
    for x in p["likely_tests"]:
        lines.append(f"  {x['path']} ({x['confidence']})")
for d in p["docs_and_conventions"]:
    lines.append(f"doc {d['path']}:{d['range']} {d.get('title') or ''}")
lean = "\n".join(lines)
imports_rel = sum(1 for s in p["related_symbols"] if s["kind"] == "import")
orient_syms = [s for o in p["repo_orientation"] for s in o["symbols"]]
imports_or = sum(1 for s in orient_syms if s.startswith(("import ", "module ")))
print(f"task: {task}")
print(f"  pretty={t(raw)} tok  compact={t(compact)} tok  lean-text={t(lean)} tok  self-reported={p['total_estimated_tokens']}  bytes/4={len(raw)//4}")
print(f"  sections (pretty tok): " + ", ".join(f"{k}={v}" for k, v in sec.items() if v > 15))
print(f"  related_symbols: {imports_rel}/{len(p['related_symbols'])} are imports; orientation symbols: {imports_or}/{len(orient_syms)} are import/module lines")
print("  must_read:", [f"{e['path'].split('/')[-1]}:{e['start_line']}" for e in p["must_read"]])

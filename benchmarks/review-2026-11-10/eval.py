#!/usr/bin/env python3
"""Ground-truth retrieval eval: rank of expected file + token cost per surface.

Requires the codixing binary on PATH, an existing index at ROOT, ripgrep, and
the tiktoken package. Token counts use cl100k_base as a proxy tokenizer.
"""
import json, subprocess, sys, time
import tiktoken
enc = tiktoken.get_encoding("cl100k_base")
tok = lambda s: len(enc.encode(s, disallowed_special=()))

def run(cmd, cwd):
    t0 = time.time()
    p = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    return p.stdout, p.stderr, (time.time() - t0) * 1000, p.returncode

def rank(paths, expect):
    for i, p in enumerate(paths, 1):
        if p in expect:
            return i
    return 0

def main():
    # usage: eval.py TASKS.json ROOT [search] [ask] [rg]
    spec = json.load(open(sys.argv[1]))
    root = sys.argv[2]
    surfaces = sys.argv[3:] or ["search", "ask"]
    rows = []
    for t in spec["tasks"]:
        row = {"q": t["q"], "kind": t["kind"]}
        if "search" in surfaces:
            out, err, ms, rc = run(["codixing", "search", t["q"], "--limit", "10", "--json"], root)
            try:
                res = json.loads(out) if out.strip() else []
            except Exception:
                res = []
            paths = [r["file"] for r in res]
            # token cost of the default human/agent rendering, not the JSON one
            plain, _, _, _ = run(["codixing", "search", t["q"], "--limit", "10"], root)
            row["search"] = {"rank": rank(paths, t["expect"]), "tok": tok(plain), "ms": ms}
        if "ask" in surfaces:
            # Ranks come from the stable JSON schema; token cost from the
            # default (text) rendering an agent actually receives. Binaries
            # older than the text default print JSON for both.
            out, err, ms, rc = run(["codixing", "ask", t["q"], "--json"], root)
            if rc != 0:
                out, err, ms, rc = run(["codixing", "ask", t["q"]], root)
                plain = out
            else:
                plain, _, _, _ = run(["codixing", "ask", t["q"]], root)
            try:
                pack = json.loads(out)
                paths = [e["path"] for e in pack["must_read"]]
            except Exception:
                paths = []
            row["ask"] = {"rank": rank(paths, t["expect"]), "tok": tok(plain), "ms": ms, "n": len(paths)}
        if "rg" in surfaces and t["kind"] == "id":
            out, _, ms, _ = run(["rg", "-n", "-w", "--no-heading", t["q"], "."], root)
            row["rg"] = {"tok": tok(out), "lines": out.count("\n"), "ms": ms}
        rows.append(row)
    for kind in ("id", "nl"):
        sel = [r for r in rows if r["kind"] == kind]
        print(f"\n## {kind} queries ({len(sel)})")
        for s in [x for x in surfaces if x != "rg"]:
            ranks = [r[s]["rank"] for r in sel]
            n = len(ranks)
            h = lambda k: sum(1 for x in ranks if 0 < x <= k)
            mrr = sum(1 / x for x in ranks if x) / n
            toks = sorted(r[s]["tok"] for r in sel)
            mss = sorted(r[s]["ms"] for r in sel)
            print(f"  {s:7s} hit@1={h(1)}/{n} hit@3={h(3)}/{n} hit@10={h(10)}/{n} MRR={mrr:.2f} "
                  f"tok median={toks[n//2]} max={toks[-1]}  ms median={mss[n//2]:.0f} max={mss[-1]:.0f}")
        if "rg" in surfaces and kind == "id":
            toks = sorted(r["rg"]["tok"] for r in sel)
            mss = sorted(r["rg"]["ms"] for r in sel)
            print(f"  rg -nw  tok median={toks[len(toks)//2]} max={toks[-1]} ms median={mss[len(mss)//2]:.0f}")
        for r in sel:
            cells = " ".join(f"{s}={r[s]['rank'] or '-'}" for s in surfaces if s != "rg")
            extra = f" rg={r['rg']['tok']}tok/{r['rg']['lines']}L" if "rg" in r else ""
            print(f"    {cells:20s}{extra}  {r['q']}")

if __name__ == "__main__":
    main()

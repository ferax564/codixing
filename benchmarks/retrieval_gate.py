#!/usr/bin/env python3
"""Answer-checked retrieval gate for real repositories.

Runs a task set (queries with the files that answer them) against an indexed
repository and fails when retrieval quality drops below the thresholds stored
with the task set. For every task it records, per surface (`codixing search`
and `codixing ask`):

- the rank of the first expected file (0 = not returned);
- tokens to answer: the tokens an agent reads, top down, until the expected
  file has been shown. A miss costs the whole output;
- wall time of the command.

Identifier tasks also record what `rg -n -w` prints for the same name, as the
baseline an index has to beat.

Usage:
    retrieval_gate.py TASKS.json ROOT [--output report.json] [--binary PATH]
    retrieval_gate.py --self-test

ROOT must already be indexed (`codixing init ROOT`). Only the standard
library is required; with `tiktoken` installed token counts are exact
(cl100k_base), otherwise they are estimated from bytes and labelled so.
"""
import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import time

try:
    import tiktoken

    _ENC = tiktoken.get_encoding("cl100k_base")
    TOKENIZER = "cl100k_base"

    def count_tokens(text):
        return len(_ENC.encode(text, disallowed_special=()))

except Exception:  # pragma: no cover - depends on the environment
    TOKENIZER = "estimate (bytes / 3.2)"

    def count_tokens(text):
        return int(len(text.encode("utf-8", "replace")) / 3.2 + 0.5)


# `1. path/to/file.rs:10-42  signature` (search) and
# `E1 path/to/file.rs:10-42 [test]` (ask evidence).
SEARCH_HIT = re.compile(r"^(\d+)\. (.+?):\d+(?:-\d+)?(?:\s|$)")
ASK_HIT = re.compile(r"^E(\d+) (.+?):\d+(?:-\d+)?(?:\s|$)")


def parse_hits(text, pattern):
    """Return [(path, end_offset)]: each hit's file and where its block ends."""
    hits = []
    offset = 0
    for line in text.splitlines(keepends=True):
        match = pattern.match(line)
        if match:
            hits.append([match.group(2), offset + len(line)])
        elif hits and line.startswith(" "):
            # Snippet lines belong to the hit above them.
            hits[-1][1] = offset + len(line)
        offset += len(line)
    return [(path, end) for path, end in hits]


def score(text, pattern, expect):
    """(rank, tokens_to_answer, tokens_total, distinct files shown)."""
    hits = parse_hits(text, pattern)
    files = []
    for path, _ in hits:
        if path not in files:
            files.append(path)
    total = count_tokens(text)
    for path, end in hits:
        if path in expect:
            return files.index(path) + 1, count_tokens(text[:end]), total, len(files)
    return 0, total, total, len(files)


def run(cmd, cwd, env):
    started = time.time()
    proc = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True)
    return proc.stdout, proc.returncode, (time.time() - started) * 1000


def median(values):
    values = sorted(values)
    return values[len(values) // 2] if values else 0


def summarise(rows, surface):
    ranks = [row[surface]["rank"] for row in rows]
    n = len(ranks)
    hit = lambda k: sum(1 for r in ranks if 0 < r <= k)
    return {
        "tasks": n,
        "hit_at_1": hit(1),
        "hit_at_3": hit(3),
        "hit_at_10": hit(10),
        "mrr": round(sum(1 / r for r in ranks if r) / n, 4) if n else 0.0,
        "median_tokens_to_answer": median([row[surface]["tokens_to_answer"] for row in rows]),
        "median_tokens_total": median([row[surface]["tokens_total"] for row in rows]),
        "median_ms": round(median([row[surface]["ms"] for row in rows])),
    }


def evaluate(spec, root, binary):
    # Measure the in-process path: a daemon (possibly of another build)
    # would make results depend on what ran before.
    env = dict(os.environ, CODIXING_NO_DAEMON="1")
    have_rg = shutil.which("rg") is not None
    rows = []
    for task in spec["tasks"]:
        expect = set(task["expect"])
        row = {"q": task["q"], "kind": task["kind"]}
        for surface, pattern in (("search", SEARCH_HIT), ("ask", ASK_HIT)):
            args = [binary, surface, task["q"]]
            if surface == "search":
                args += ["--limit", "10"]
            out, rc, ms = run(args, root, env)
            rank, to_answer, total, files = score(out, pattern, expect)
            row[surface] = {
                "rank": rank,
                "tokens_to_answer": to_answer,
                "tokens_total": total,
                "files": files,
                "ms": round(ms, 1),
                "rc": rc,
            }
        if have_rg and task["kind"] == "id":
            out, _, ms = run(["rg", "-n", "-w", "--no-heading", task["q"], "."], root, env)
            row["rg"] = {
                "tokens_total": count_tokens(out),
                "lines": out.count("\n"),
                "answer_present": any(f"./{path}:" in out or f"{path}:" in out for path in expect),
                "ms": round(ms, 1),
            }
        rows.append(row)
    return rows


def check(spec, summary):
    """Compare a summary against the task set's thresholds; return failures."""
    failures = []
    gate = spec.get("gate", {})
    for kind, limits in gate.items():
        for key, minimum in limits.items():
            if key == "ask_within_of_search":
                gap = summary[kind]["search"]["mrr"] - summary[kind]["ask"]["mrr"]
                if gap > minimum + 1e-9:
                    failures.append(
                        f"{kind}: ask MRR is {gap:.2f} below search MRR (allowed {minimum:.2f})"
                    )
                continue
            surface, metric = key.split("_", 1)
            actual = summary[kind][surface][metric]
            if actual + 1e-9 < minimum:
                failures.append(f"{kind}: {surface} {metric} {actual:.2f} < {minimum:.2f}")
    return failures


def self_test():
    search = (
        "1. src/a.rs:10-20  fn alpha()\n"
        "   10| fn alpha() {\n"
        "2. docs/guide.md:1-9 [doc]  § Guide\n"
        "3. src/b.rs:5-8  fn beta()\n"
        "   5| fn beta() {}\n"
    )
    hits = parse_hits(search, SEARCH_HIT)
    assert [path for path, _ in hits] == ["src/a.rs", "docs/guide.md", "src/b.rs"], hits
    rank, to_answer, total, files = score(search, SEARCH_HIT, {"src/b.rs"})
    assert (rank, files) == (3, 3) and to_answer == total, (rank, to_answer, total)
    rank, to_answer, total, _ = score(search, SEARCH_HIT, {"src/a.rs"})
    assert rank == 1 and to_answer < total, (rank, to_answer, total)
    assert score(search, SEARCH_HIT, {"src/missing.rs"})[0] == 0

    ask = (
        "# locate pack: 2 evidence\n## Read first\n"
        "E1 src/a.rs:10-20  fn alpha()\n   | fn alpha() {\n"
        "E2 src/a.rs:40-50\nE3 src/b.rs:5-8 [test]\n"
        "## Defined in the evidence\nsrc/b.rs:5 fn beta()\n"
    )
    # Two chunks of one file are one file: src/b.rs is the second file shown.
    assert score(ask, ASK_HIT, {"src/b.rs"})[0] == 2

    spec = {"gate": {"id": {"search_mrr": 0.9, "ask_within_of_search": 0.1}}}
    good = {"id": {"search": {"mrr": 0.95}, "ask": {"mrr": 0.9}}}
    bad = {"id": {"search": {"mrr": 0.85}, "ask": {"mrr": 0.6}}}
    assert check(spec, good) == []
    assert len(check(spec, bad)) == 2, check(spec, bad)
    print("retrieval_gate self-test passed")


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("tasks", nargs="?")
    parser.add_argument("root", nargs="?")
    parser.add_argument("--output", help="write the JSON report here")
    parser.add_argument("--binary", default="codixing", help="codixing binary to measure")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return 0
    if not args.tasks or not args.root:
        parser.error("TASKS.json and ROOT are required")

    with open(args.tasks, encoding="utf-8") as handle:
        spec = json.load(handle)
    binary = os.path.abspath(args.binary) if os.sep in args.binary else args.binary
    rows = evaluate(spec, args.root, binary)

    summary = {}
    for kind in ("id", "nl"):
        selected = [row for row in rows if row["kind"] == kind]
        if not selected:
            continue
        summary[kind] = {s: summarise(selected, s) for s in ("search", "ask")}
        baseline = [row["rg"] for row in selected if "rg" in row]
        if baseline:
            summary[kind]["rg"] = {
                "tasks": len(baseline),
                "median_tokens_total": median([b["tokens_total"] for b in baseline]),
                "max_tokens_total": max(b["tokens_total"] for b in baseline),
                "answer_present": sum(1 for b in baseline if b["answer_present"]),
                "median_ms": round(median([b["ms"] for b in baseline])),
            }
    failures = check(spec, summary)

    print(f"# Retrieval gate: {spec.get('corpus', args.root)}")
    print(f"Tokens: {TOKENIZER}")
    for kind, label in (("id", "identifier"), ("nl", "natural language")):
        if kind not in summary:
            continue
        print(f"\n## {label} ({summary[kind]['search']['tasks']} tasks)")
        for surface in ("search", "ask"):
            s = summary[kind][surface]
            print(
                f"  {surface:6s} hit@1 {s['hit_at_1']}/{s['tasks']}  hit@3 {s['hit_at_3']}/{s['tasks']}"
                f"  hit@10 {s['hit_at_10']}/{s['tasks']}  MRR {s['mrr']:.2f}"
                f"  tokens to answer (median) {s['median_tokens_to_answer']}"
                f"  {s['median_ms']} ms"
            )
        if "rg" in summary[kind]:
            b = summary[kind]["rg"]
            print(
                f"  rg -nw output (median) {b['median_tokens_total']} tokens, max {b['max_tokens_total']}"
                f"  {b['median_ms']} ms"
            )
        for row in (r for r in rows if r["kind"] == kind):
            print(
                f"    search={row['search']['rank'] or '-':<3} ask={row['ask']['rank'] or '-':<3} {row['q']}"
            )

    report = {
        "schema_version": 1,
        "corpus": spec.get("corpus"),
        "root": os.path.abspath(args.root),
        "tokenizer": TOKENIZER,
        "summary": summary,
        "gate": spec.get("gate", {}),
        "failures": failures,
        "tasks": rows,
    }
    if args.output:
        with open(args.output, "w", encoding="utf-8") as handle:
            json.dump(report, handle, indent=1)
        print(f"\nWrote {args.output}")

    if failures:
        print("\nGATE FAILED:")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    print("\nGate passed." if spec.get("gate") else "\nNo thresholds in the task set; nothing gated.")
    return 0


if __name__ == "__main__":
    sys.exit(main())

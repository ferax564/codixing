#!/usr/bin/env python3
"""Measure a shell command: wall ms, peak RSS, stdout/stderr bytes, cl100k tokens.

usage: measure.py [-n HEAD_LINES] [-q] [-o FILE] -- <shell command>
"""
import resource
import subprocess
import sys
import time

import tiktoken

ENC = tiktoken.get_encoding("cl100k_base")


def main():
    args = sys.argv[1:]
    head = 0
    out_file = None
    while args and args[0] != "--":
        if args[0] == "-n":
            head = int(args[1])
            args = args[2:]
        elif args[0] == "-o":
            out_file = args[1]
            args = args[2:]
        else:
            break
    if args and args[0] == "--":
        args = args[1:]
    cmd = " ".join(args)
    t0 = time.time()
    p = subprocess.run(cmd, shell=True, capture_output=True)
    ms = (time.time() - t0) * 1000
    rss_mb = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss / 1024
    out = p.stdout.decode("utf-8", "replace")
    err = p.stderr.decode("utf-8", "replace")
    toks = len(ENC.encode(out, disallowed_special=()))
    etoks = len(ENC.encode(err, disallowed_special=()))
    if out_file:
        with open(out_file, "w") as f:
            f.write(out)
    if head:
        lines = out.splitlines()
        print("\n".join(lines[:head]))
        if len(lines) > head:
            print(f"... [{len(lines) - head} more lines]")
        if err.strip():
            el = err.strip().splitlines()
            print("--- stderr ---")
            print("\n".join(el[:8]))
    print(
        f"## rc={p.returncode} ms={ms:.0f} rss={rss_mb:.0f}MB "
        f"stdout={len(p.stdout)}B/{len(out.splitlines())}L/{toks}tok "
        f"stderr={len(p.stderr)}B/{etoks}tok :: {cmd[:110]}"
    )


if __name__ == "__main__":
    main()

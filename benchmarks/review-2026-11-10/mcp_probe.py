#!/usr/bin/env python3
"""Minimal MCP stdio client: tools/list size per profile + tool call output sizes."""
import json, subprocess, sys, time
import tiktoken
enc = tiktoken.get_encoding("cl100k_base")
tok = lambda s: len(enc.encode(s, disallowed_special=()))

class Mcp:
    def __init__(self, root, profile):
        self.p = subprocess.Popen(
            ["codixing-mcp", "--root", root, "--no-daemon-fork", "--profile", profile],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        self.id = 0
        self.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                 "clientInfo": {"name": "probe", "version": "0"}})
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
        self.p.stdin.flush()
    def call(self, method, params=None):
        self.id += 1
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params or {}}) + "\n")
        self.p.stdin.flush()
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError("server closed")
            msg = json.loads(line)
            if msg.get("id") == self.id:
                return msg
    def tool(self, name, args):
        t0 = time.time()
        r = self.call("tools/call", {"name": name, "arguments": args})
        ms = (time.time() - t0) * 1000
        text = "".join(c.get("text", "") for c in r.get("result", {}).get("content", [])) if "result" in r else json.dumps(r.get("error"))
        return text, ms
    def close(self):
        self.p.stdin.close(); self.p.wait(timeout=10)

if __name__ == "__main__":
    root = sys.argv[1]
    mode = sys.argv[2]
    if mode == "list":
        for prof in ["minimal", "reviewer", "editor", "dangerous"]:
            m = Mcp(root, prof)
            r = m.call("tools/list")
            tools = r["result"]["tools"]
            s = json.dumps(tools, separators=(",", ":"))
            desc = sum(tok(t.get("description", "")) for t in tools)
            print(f"{prof:9s} tools={len(tools):3d} bytes={len(s):6d} tokens={tok(s):6d} (descriptions alone {desc})")
            if prof == "minimal":
                for t in sorted(tools, key=lambda t: -tok(json.dumps(t))):
                    print(f"    {tok(json.dumps(t, separators=(',', ':'))):5d} tok  {t['name']}")
            m.close()
    else:
        prof = sys.argv[3]
        m = Mcp(root, prof)
        for spec in json.load(open(mode)):
            text, ms = m.tool(spec["name"], spec["args"])
            print(f"{tok(text):6d} tok {len(text):7d} B {ms:7.0f} ms  {spec['name']} {json.dumps(spec['args'])[:90]}")
            if spec.get("show"):
                print("      | " + "\n      | ".join(text.splitlines()[:spec["show"]]))
        m.close()

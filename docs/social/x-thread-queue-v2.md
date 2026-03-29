# Queue Embedding v2 — X Thread

## Tweet 1
shipped queue-based embedding for codixing. 17x faster search, 67% better symbol recall than grep on a 368K LoC TypeScript monorepo. here's what changed and what broke along the way.

## Tweet 2
the problem: embedding 881K chunks on linux kernel takes 60+ min with a single ONNX session. if it crashes at chunk 500K, you restart from zero. no crash recovery, no parallelism, no way to serve search while embedding.

## Tweet 3
v2 solution: RustQueue gives crash recovery + parallel ONNX workers. each worker gets its own model session (~200MB RAM). file-grouped jobs instead of per-chunk — reduces queue I/O from 45K writes to ~2K on OpenClaw.

## Tweet 4
speed results on OpenClaw (9,387 files):

symbol_lookup: 55ms (grep: 937ms) — 17x
usages: 53ms (grep: 937ms) — 18x
fast (BM25+vector): 240ms — 4x
explore: 77ms — 12x

grep can't touch the symbol table.

## Tweet 5
accuracy: symbol R@10 = 1.000 (grep: 0.600). codixing finds every definition. grep buries it under usage files — search "ChannelPlugin" and get 10 importers but not the file that defines it. MRR 0.496 vs 0.271.

## Tweet 6
honest failure list: v1 was slower than no queue. eager trigram load added 55s to startup. parallel drain OOM'd with 16 workers (3.2GB just for ONNX sessions). one-job-per-chunk serialized 45K times to redb. iterated 6 times to get it right.

## Tweet 7
architecture that worked: rayon::join parallelizes graph + trigram build. embed_pending() dispatches to parallel workers or sync path based on a 1000-chunk threshold. block_on_async() handles the sync/async boundary without nested runtime panics.

## Tweet 8
try it: `claude plugin marketplace add ferax564/codixing` — would love feedback on search accuracy for your repos, especially monorepos where cross-package search is still weak (grep beats us there, R@10 0.367 vs 0.067).

@AnthropicAI @cursor_ai #BuildInPublic #DevTools #Rust

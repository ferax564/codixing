#!/usr/bin/env python3
"""Fine-tune a cross-encoder on SWE-bench data for file localization reranking.

Creates (query, file_outline, label) triples from SWE-bench train split,
then fine-tunes a ModernBERT-base cross-encoder using sentence-transformers.

Usage:
    python3 benchmarks/train_reranker.py --build-data   # Step 1: build training data
    python3 benchmarks/train_reranker.py --train         # Step 2: fine-tune
    python3 benchmarks/train_reranker.py --eval          # Step 3: evaluate on Lite
"""
import argparse
import ast
import json
import os
import re
import subprocess
import sys
import time
from collections import defaultdict
from pathlib import Path

# ── Paths ──
REPOS_DIR = Path("benchmarks/repos")
DATA_DIR = Path("benchmarks/reranker_data")
MODEL_DIR = Path("benchmarks/models/swe-reranker")
CODIXING = Path("/home/andrea/code/codixing/target/release/codixing")
ENV = {
    **os.environ,
    "ORT_DYLIB_PATH": os.path.expanduser("~/.local/lib/libonnxruntime.so"),
    "LD_LIBRARY_PATH": os.path.expanduser("~/.local/lib"),
}


def extract_file_outline(file_path: Path, rel_path: str) -> str:
    """Extract compact outline: path + class/function signatures."""
    try:
        source = file_path.read_text(errors="replace")
    except (OSError, UnicodeDecodeError):
        return rel_path
    try:
        tree = ast.parse(source)
    except SyntaxError:
        return rel_path

    lines = source.splitlines()
    parts = [rel_path]
    for node in ast.iter_child_nodes(tree):
        if isinstance(node, ast.FunctionDef | ast.AsyncFunctionDef):
            sig = lines[node.lineno - 1].strip() if node.lineno <= len(lines) else ""
            parts.append(sig[:120])
        elif isinstance(node, ast.ClassDef):
            sig = lines[node.lineno - 1].strip() if node.lineno <= len(lines) else ""
            parts.append(sig[:120])
            methods = []
            for child in ast.iter_child_nodes(node):
                if isinstance(child, ast.FunctionDef | ast.AsyncFunctionDef):
                    methods.append(child.name)
            if methods:
                parts.append(f"  methods: {', '.join(methods[:15])}")
    return "\n".join(parts)[:800]


def checkout_commit(repo_path: Path, commit: str) -> bool:
    result = subprocess.run(
        ["git", "checkout", "-f", commit],
        capture_output=True, timeout=30, cwd=str(repo_path),
    )
    return result.returncode == 0


def clone_repo(repo: str):
    repo_name = repo.split("/")[-1]
    repo_path = REPOS_DIR / repo_name
    if repo_path.exists():
        return
    print(f"  Cloning {repo}...")
    subprocess.run(
        ["git", "clone", "--quiet", f"https://github.com/{repo}.git", str(repo_path)],
        timeout=300,
    )


def get_python_files(repo_path: Path) -> list[str]:
    """List all .py files in repo (excluding tests, hidden dirs, .codixing)."""
    result = subprocess.run(
        ["find", ".", "-name", "*.py",
         "-not", "-path", "*/.codixing/*",
         "-not", "-path", "*/.git/*",
         "-not", "-path", "*/node_modules/*"],
        capture_output=True, timeout=10, cwd=str(repo_path),
    )
    files = []
    for line in result.stdout.decode(errors="replace").strip().split("\n"):
        f = line.strip().lstrip("./")
        if f and f.endswith(".py"):
            files.append(f)
    return files


def is_test_file(path: str) -> bool:
    return ("/test" in path or path.startswith("tests/") or
            path.startswith("test/") or path.split("/")[-1].startswith("test_"))


def build_training_data():
    """Build cross-encoder training data from SWE-bench train + non-Lite test."""
    from datasets import load_dataset

    DATA_DIR.mkdir(parents=True, exist_ok=True)
    REPOS_DIR.mkdir(parents=True, exist_ok=True)

    # Load datasets
    print("Loading SWE-bench datasets...")
    ds_train = load_dataset("princeton-nlp/SWE-bench", split="train")
    ds_test = load_dataset("princeton-nlp/SWE-bench", split="test")
    ds_dev = load_dataset("princeton-nlp/SWE-bench", split="dev")
    ds_lite = load_dataset("princeton-nlp/SWE-bench_Lite", split="test")

    # Exclude Lite tasks from training
    lite_ids = set(t["instance_id"] for t in ds_lite)
    tasks = [t for t in list(ds_train) + list(ds_test) + list(ds_dev)
             if t["instance_id"] not in lite_ids]
    print(f"Training tasks: {len(tasks)} (excluded {len(lite_ids)} Lite tasks)")

    # Only use repos we have cloned (same 12 repos as Lite)
    lite_repos = set(t["repo"] for t in ds_lite)
    tasks = [t for t in tasks if t["repo"] in lite_repos]
    print(f"Tasks from Lite repos: {len(tasks)}")

    # Clone repos
    repos = sorted(set(t["repo"] for t in tasks))
    for repo in repos:
        clone_repo(repo)

    # Build training examples
    examples = []
    skipped = 0
    repo_cache = {}  # (repo, commit) -> last indexed

    for i, task in enumerate(tasks):
        if i % 100 == 0:
            print(f"  [{i+1}/{len(tasks)}] Processing...", flush=True)

        repo = task["repo"]
        repo_name = repo.split("/")[-1]
        repo_path = REPOS_DIR / repo_name
        commit = task["base_commit"]

        # Extract gold files from patch
        gold_files = set(re.findall(r"--- a/(.*?)\n", task["patch"]))
        if not gold_files:
            skipped += 1
            continue

        # Checkout the right version
        if not checkout_commit(repo_path, commit):
            skipped += 1
            continue

        # Get all Python files
        all_files = get_python_files(repo_path)
        if not all_files:
            skipped += 1
            continue

        # Build outlines for gold files (positives)
        query = task["problem_statement"][:2000]  # Truncate long queries
        gold_outlines = []
        for gf in gold_files:
            full_path = repo_path / gf
            if full_path.exists() and gf.endswith(".py"):
                outline = extract_file_outline(full_path, gf)
                gold_outlines.append((gf, outline))

        if not gold_outlines:
            skipped += 1
            continue

        # Sample hard negatives: files in same directory as gold, plus random others
        neg_candidates = set()
        for gf in gold_files:
            gold_dir = "/".join(gf.split("/")[:-1])
            # Same-directory siblings (hardest negatives)
            for f in all_files:
                f_dir = "/".join(f.split("/")[:-1])
                if f_dir == gold_dir and f not in gold_files and not is_test_file(f):
                    neg_candidates.add(f)

        # Add some random non-test files from the same package
        for gf in gold_files:
            pkg = gf.split("/")[0] if "/" in gf else ""
            for f in all_files:
                if (f.startswith(pkg + "/") and f not in gold_files
                        and not is_test_file(f) and f not in neg_candidates):
                    neg_candidates.add(f)
                    if len(neg_candidates) >= 30:
                        break

        # Build outlines for negatives (limit to 15)
        neg_outlines = []
        for nf in sorted(neg_candidates)[:15]:
            full_path = repo_path / nf
            if full_path.exists():
                outline = extract_file_outline(full_path, nf)
                neg_outlines.append((nf, outline))

        if not neg_outlines:
            skipped += 1
            continue

        # Create training examples: (query, outline, label)
        for gf, outline in gold_outlines:
            examples.append({
                "query": query,
                "document": outline,
                "label": 1,
                "instance_id": task["instance_id"],
                "file": gf,
            })
        for nf, outline in neg_outlines:
            examples.append({
                "query": query,
                "document": outline,
                "label": 0,
                "instance_id": task["instance_id"],
                "file": nf,
            })

    print(f"\nBuilt {len(examples)} examples ({skipped} tasks skipped)")
    pos = sum(1 for e in examples if e["label"] == 1)
    neg = sum(1 for e in examples if e["label"] == 0)
    print(f"  Positives: {pos}, Negatives: {neg}, Ratio: 1:{neg/max(pos,1):.1f}")

    # Save
    out_path = DATA_DIR / "train.jsonl"
    with open(out_path, "w") as f:
        for ex in examples:
            f.write(json.dumps(ex) + "\n")
    print(f"Saved to {out_path}")


def train_model():
    """Fine-tune ModernBERT-base cross-encoder on the training data."""
    from sentence_transformers import CrossEncoder
    from sentence_transformers.cross_encoder.trainer import CrossEncoderTrainer
    from sentence_transformers.cross_encoder.training_args import CrossEncoderTrainingArguments
    from datasets import Dataset

    data_path = DATA_DIR / "train.jsonl"
    if not data_path.exists():
        print(f"ERROR: {data_path} not found. Run --build-data first.")
        sys.exit(1)

    # Load training data
    print("Loading training data...")
    examples = []
    with open(data_path) as f:
        for line in f:
            examples.append(json.loads(line))

    print(f"Loaded {len(examples)} examples")

    # Truncate queries to 500 chars and docs to 500 chars for CPU speed
    for ex in examples:
        ex["query"] = ex["query"][:500]
        ex["document"] = ex["document"][:500]

    # Subsample for CPU training: keep all positives + equal negatives
    import random
    random.seed(42)
    positives = [ex for ex in examples if ex["label"] == 1]
    negatives = [ex for ex in examples if ex["label"] == 0]
    # 1:3 positive:negative ratio (from 1:8), max 8K total
    neg_sample = random.sample(negatives, min(len(positives) * 3, len(negatives)))
    examples = positives + neg_sample
    random.shuffle(examples)
    print(f"Subsampled: {len(examples)} ({len(positives)} pos, {len(neg_sample)} neg)")

    # Create HF dataset
    dataset = Dataset.from_list([
        {"sentence1": ex["query"], "sentence2": ex["document"], "label": float(ex["label"])}
        for ex in examples
    ])

    # Split into train/eval (95/5)
    split = dataset.train_test_split(test_size=0.05, seed=42)
    train_ds = split["train"]
    eval_ds = split["test"]
    print(f"Train: {len(train_ds)}, Eval: {len(eval_ds)}")

    # Initialize cross-encoder from ModernBERT-base
    base_model = "answerdotai/ModernBERT-base"
    print(f"Loading base model: {base_model}")
    model = CrossEncoder(base_model, num_labels=1, trust_remote_code=True,
                         max_length=512)  # Limit token length for CPU training

    # Training args — optimized for CPU (AMD Ryzen 7 6800H, 32GB RAM)
    output_dir = str(MODEL_DIR)
    args = CrossEncoderTrainingArguments(
        output_dir=output_dir,
        num_train_epochs=1,
        per_device_train_batch_size=16,
        per_device_eval_batch_size=32,
        gradient_accumulation_steps=2,  # effective batch size = 32
        learning_rate=2e-5,
        warmup_ratio=0.1,
        weight_decay=0.01,
        fp16=False,
        bf16=False,
        logging_steps=25,
        eval_strategy="epoch",
        save_strategy="epoch",
        save_total_limit=1,
        load_best_model_at_end=False,
        dataloader_num_workers=4,
        max_grad_norm=1.0,
    )

    # Train
    print("Starting training...")
    trainer = CrossEncoderTrainer(
        model=model,
        args=args,
        train_dataset=train_ds,
        eval_dataset=eval_ds,
    )
    trainer.train()

    # Save final model
    model.save_pretrained(output_dir)
    print(f"Model saved to {output_dir}")


def evaluate_model():
    """Evaluate fine-tuned cross-encoder on SWE-bench Lite."""
    print("Run the SWE-bench eval with:")
    print(f"  python3 benchmarks/swe_bench_eval.py --skip-clone --embed-rerank 'Salesforce/SweRankEmbed-Small' --ce-rerank '{MODEL_DIR}'")


def main():
    parser = argparse.ArgumentParser(description="Train SWE-bench file reranker")
    parser.add_argument("--build-data", action="store_true", help="Build training data")
    parser.add_argument("--train", action="store_true", help="Fine-tune model")
    parser.add_argument("--eval", action="store_true", help="Show eval command")
    args = parser.parse_args()

    if args.build_data:
        build_training_data()
    elif args.train:
        train_model()
    elif args.eval:
        evaluate_model()
    else:
        parser.print_help()


if __name__ == "__main__":
    main()

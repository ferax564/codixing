#!/usr/bin/env bash
# Downloads all-MiniLM-L6-v2 ONNX model + tokenizer from HuggingFace
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MODEL_URL="https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/onnx/model.onnx"
TOKENIZER_URL="https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/tokenizer.json"

echo "Downloading all-MiniLM-L6-v2 ONNX model..."
curl -L -o "$SCRIPT_DIR/model.onnx" "$MODEL_URL"
echo "Downloading tokenizer..."
curl -L -o "$SCRIPT_DIR/tokenizer.json" "$TOKENIZER_URL"
echo "Done. Model files saved to $SCRIPT_DIR/"
ls -lh "$SCRIPT_DIR/model.onnx" "$SCRIPT_DIR/tokenizer.json"

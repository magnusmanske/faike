#!/usr/bin/env bash
# Fetch the optional AI-image classifier used by `detect.classifier`.
# Usage: scripts/fetch-classifier.sh [models-dir]
# The model is an ONNX export of SMOGY/SMOGY-Ai-images-detector (MIT).
set -euo pipefail

directory="${1:-models}"
file="$directory/ai-image-detector.onnx"
base="https://huggingface.co/onnx-community/SMOGY-Ai-images-detector-ONNX/resolve/main/onnx"

mkdir -p "$directory"
if [[ -s "$file" ]]; then
    echo "already present: $file"
    exit 0
fi
echo "fetching the quantized classifier (~93 MB)"
curl -fL --retry 3 --retry-delay 2 "$base/model_quantized.onnx" -o "$file"
echo "classifier model is in $file"

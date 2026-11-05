#!/usr/bin/env bash
# Fetch the optional AI-image classifiers used by `detect.classifier`.
# Usage: scripts/fetch-classifier.sh [models-dir]
#
# primary:   SMOGY/SMOGY-Ai-images-detector ONNX (Swin, 224), CC-BY-NC-4.0.
# secondary: fev/Community Forensics ViT-S/16 ONNX (384, single logit), MIT.
# Neither model is bundled or redistributed by this repository.
set -euo pipefail

directory="${1:-models}"
mkdir -p "$directory"

fetch() {
    local file="$1" url="$2" label="$3"
    if [[ -s "$directory/$file" ]]; then
        echo "already present: $directory/$file"
        return
    fi
    echo "fetching $label"
    curl -fL --retry 3 --retry-delay 2 "$url" -o "$directory/$file"
}

fetch ai-image-detector.onnx \
    "https://huggingface.co/onnx-community/SMOGY-Ai-images-detector-ONNX/resolve/main/onnx/model_quantized.onnx" \
    "SMOGY classifier (~93 MB, CC-BY-NC-4.0)"
fetch ai-image-detector-commfor.onnx \
    "https://huggingface.co/angelhd25/ull-ai-image-detector/resolve/main/commfor384_web_v13_fp32.onnx" \
    "Community Forensics classifier (~88 MB, MIT)"

echo "classifier models are in $directory"

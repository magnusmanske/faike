#!/usr/bin/env bash
# Fetch the Adobe TrustMark ONNX models used by the watermark detector.
# Usage: scripts/fetch-models.sh [models-dir] [variant]
# The models are MIT-licensed, but are not bundled in the repository.
set -euo pipefail

directory="${1:-models}"
variant="${2:-Q}"
base="https://cai-watermark.adobe.net/watermarking/trustmark-models"

mkdir -p "$directory"
for file in "encoder_${variant}.onnx" "decoder_${variant}.onnx"; do
    if [[ -s "$directory/$file" ]]; then
        echo "already present: $directory/$file"
        continue
    fi
    echo "fetching $file"
    curl -fL --retry 3 --retry-delay 2 "$base/$file" -o "$directory/$file"
done
echo "TrustMark models are in $directory"

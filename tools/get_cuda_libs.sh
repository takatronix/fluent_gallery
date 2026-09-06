#!/bin/bash
# ONNX(ORT)を GPU で走らせるための共有ライブラリを engine/cuda/lib に置く。
#
# ORT 2.0.0-rc.13 の CUDA プロバイダは **CUDA 13** 向けにビルドされている。
# CUDA 12 しか入っていない機械では libcublasLt.so.13 が見つからず、
# 黙って CPU に落ちる(ログに理由は出る)。ここで CUDA 13 のランタイムだけ取ってくる。
#
# 置くだけでよい。アプリは起動時に engine/cuda/lib を見つけると、
# そこを通して自分を起動し直す(src/ep.rs の adopt_gpu_libs)。
#
# 効果(RTX 4090 実測): SAM2 のマスク 0.95秒 → 0.057秒/枚(16.7倍)
# 注意: GroundingDINO は GPU にしても速くならないので CPU 固定のまま(docs/model-placement-design.md)
set -e
cd "$(dirname "$0")/.."
DEST="engine/cuda/lib"
mkdir -p "$DEST"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
PY=$(command -v python3.12 || command -v python3)
echo "CUDA 13 のランタイムと cuDNN を取得します(約1.4GB)"
"$PY" -m venv "$TMP/venv" >/dev/null
"$TMP/venv/bin/pip" -q download --no-deps -d "$TMP/whl" nvidia-cublas nvidia-cuda-runtime nvidia-cudnn-cu13
for w in "$TMP"/whl/*.whl; do
  "$TMP/venv/bin/python" -c "import zipfile,sys; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])" "$w" "$TMP/x"
done
find "$TMP/x" \( -name 'libcublas*.so.13*' -o -name 'libcudart.so.13*' -o -name 'libcudnn*.so.9*' \) -exec cp {} "$DEST/" \;
echo "置きました: $DEST ($(du -sh "$DEST" | cut -f1))"
echo "アプリを再起動すると ONNX が GPU で走ります(ログに「CUDA で実行」と出ます)"

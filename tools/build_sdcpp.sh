#!/bin/bash
# stable-diffusion.cpp(生成エンジン、MIT)を CUDA でビルドして engine/bin/ に置く(Linux/NVIDIA 用。Mac は build_mac.sh が公式 zip を同梱)。
# 冪等: engine/bin/sd-cli があれば何もしない。SD_REF で版を固定(既定は Mac 同梱と同じ master-841-6b3edaa 相当のコミット)。
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=engine/bin
[ -x "$BIN/sd-cli" ] && [ -x "$BIN/sd-server" ] && { echo "sd.cpp: already in $BIN"; exit 0; }
SRC="${SD_SRC:-$HOME/stable-diffusion.cpp}"
SD_REF="${SD_REF:-6b3edaa}"
command -v nvcc >/dev/null || { echo "nvcc が無い(CUDA toolkit)。Vulkan で代替するなら SD_BACKEND=VULKAN"; }
if [ ! -d "$SRC" ]; then git clone --recursive https://github.com/leejet/stable-diffusion.cpp "$SRC"; fi
git -C "$SRC" fetch -q origin && git -C "$SRC" checkout -q "$SD_REF" && git -C "$SRC" submodule update --init --recursive -q
BACKEND="${SD_BACKEND:-CUDA}"
cmake -S "$SRC" -B "$SRC/build" -DSD_${BACKEND}=ON -DCMAKE_BUILD_TYPE=Release -DSD_BUILD_EXAMPLES=ON >/dev/null
cmake --build "$SRC/build" --config Release -j"$(nproc)"
mkdir -p "$BIN"
cp "$SRC/build/bin/sd-cli" "$SRC/build/bin/sd-server" "$BIN/"
cp "$SRC"/build/bin/*.so "$BIN/" 2>/dev/null || true
echo "sd.cpp($BACKEND, $SD_REF) → $BIN"; "$BIN/sd-cli" --help 2>&1 | head -1

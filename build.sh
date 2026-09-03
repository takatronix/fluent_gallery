#!/bin/bash
# fluent_gallery リリースビルド(BINDGEN/RUSTFLAGSはこのマシンの必須設定)
export PATH="$HOME/.cargo/bin:$PATH"
export BINDGEN_EXTRA_CLANG_ARGS=-I/usr/lib/gcc/x86_64-linux-gnu/15/include
export RUSTFLAGS="-L /usr/lib/x86_64-linux-gnu"
cd "$(dirname "$0")"
exec cargo build --release

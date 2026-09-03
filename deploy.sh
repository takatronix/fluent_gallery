#!/bin/bash
# ビルド→再起動→UI回帰テストまでが1セット。テストが落ちたら終了コード1=デプロイ失敗扱い。
# (2026-09-03「バグ修正して新しいバグ入れてるよね」への再発防止 — 目視確認だけのデプロイ禁止)
set -e
cd "$(dirname "$0")"
bash build.sh
OLD=$(pgrep -x fluent_gallery || true)
if [ -n "$OLD" ]; then kill $OLD; sleep 3; fi
setsid nohup ./target/release/fluent_gallery > /tmp/fluent_gallery.log 2>&1 &
sleep 4
curl -sf -o /dev/null localhost:8790/api/facets || { echo "起動失敗"; exit 1; }
node tests/ui_regression.js

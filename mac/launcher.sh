#!/bin/bash
# FluentGallery.app の実行体。サーバを起動してブラウザで開く(Tauri化までの素のバイナリ運用)。
# データは ~/Library/Application Support/FluentGallery/ (store/ engine/ ログ)。UIはバンドル内のweb/を参照。
HERE="$(cd "$(dirname "$0")" && pwd)"          # Contents/MacOS
RES="$HERE/../Resources"
DATA="${FG_DATA:-$HOME/Library/Application Support/FluentGallery}"
PORT="${FG_PORT:-8790}"
URL="http://127.0.0.1:$PORT"
mkdir -p "$DATA/store" "$DATA/engine/models"
ln -sfn "$RES/web" "$DATA/web"                  # index.htmlはroot/web/から毎回読まれる(no-store)
cd "$DATA" || exit 1
alive() { curl -sf -m 1 "$URL/api/caps" >/dev/null 2>&1; }
if alive; then [ -z "$FG_NO_OPEN" ] && open "$URL/"; exit 0; fi   # 二重起動: ブラウザだけ開く
PORT="$PORT" "$HERE/fluent_gallery" >> "$DATA/fluent_gallery.log" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null' EXIT INT TERM      # Dockから終了→サーバも止める
for _ in $(seq 1 100); do alive && break; sleep 0.2; done
alive || { echo "起動失敗: $DATA/fluent_gallery.log を確認" >&2; exit 1; }
[ -z "$FG_NO_OPEN" ] && open "$URL/"
wait $PID

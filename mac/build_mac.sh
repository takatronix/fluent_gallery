#!/bin/bash
# fluent_gallery Mac販売ビルド: 依存チェック → Metalビルド(顔IDなし) → UI回帰テスト → .app → .dmg → (署名/notarize)
#
#   bash mac/build_mac.sh                      # dist/FluentGallery.app と dist/FluentGallery.dmg を作る
#   bash mac/build_mac.sh --store              # ストア提出版(顔IDなし・YouTube/X拒否・COCOはCC BY系のみ)。既定はフル機能
#   bash mac/build_mac.sh --no-test            # 回帰テストを飛ばす
#   bash mac/build_mac.sh --plain              # Tauri殻なし(素のバイナリ+ブラウザ起動の仮.app)
#   SIGN="Developer ID Application: Your Name (TEAMID)" bash mac/build_mac.sh   # 署名(Tauri/plain共通)
#   notarize: Tauri殻は APPLE_ID/APPLE_PASSWORD/APPLE_TEAM_ID(または APPLE_API_KEY系)を環境変数で渡すと自動。
#             --plain は NOTARY_PROFILE=fg(notarytool store-credentials で事前登録)
#   BUNDLE_ID=com.example.fluentgallery        # 既定 com.takatronix.fluentgallery
set -euo pipefail
cd "$(dirname "$0")/.."; ROOT=$PWD
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
FEATURES="metal,faceid"; RUN_TEST=1; PLAIN=0
for a in "$@"; do case "$a" in
  --store) FEATURES="metal,store";;   # ストア提出版: 顔ID(非商用モデル)なし・YouTube/X取り込み拒否・COCOはCC BY系のみ
  --no-test) RUN_TEST=0;;
  --plain) PLAIN=1;;   # Tauri殻を使わず、素のバイナリ+ブラウザ起動の .app(mac/launcher.sh)を作る
  *) echo "unknown option: $a" >&2; exit 2;;
esac; done
BUNDLE_ID="${BUNDLE_ID:-com.takatronix.fluentgallery}"
VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')
APP=dist/FluentGallery.app
DMG=dist/FluentGallery-$VERSION.dmg
step() { printf '\n\033[1;36m== %s\033[0m\n' "$*"; }

step "依存チェック"
[ "$(uname -m)" = arm64 ] || { echo "Apple Silicon 専用です"; exit 1; }
for c in cargo cmake; do command -v $c >/dev/null || case $c in
  cargo) echo "cargo が無い: curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal"; exit 1;;
  cmake) echo "cmake が無い: brew install cmake"; exit 1;;
esac; done
echo "cargo $(cargo --version | cut -d' ' -f2) / cmake $(cmake --version | head -1 | cut -d' ' -f3) / features=$FEATURES / v$VERSION"

step "ビルド (--no-default-features --features $FEATURES)"
cargo build --release --no-default-features --features "$FEATURES"
BIN=target/release/fluent_gallery
file "$BIN" | grep -q arm64 || { echo "arm64 バイナリになっていない"; exit 1; }

if [ "$RUN_TEST" = 1 ]; then
  step "UI回帰テスト(一時ストア・:8798)"
  CHROME="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
  [ -x "$CHROME" ] || { echo "Chrome が見つからない($CHROME)。CHROME=... で指定するか --no-test"; exit 1; }
  command -v node >/dev/null || { echo "node が無い(回帰テスト用)。brew install node か --no-test"; exit 1; }
  [ -d tests/node_modules ] || (cd tests && npm install --silent)
  TMP=$(mktemp -d); mkdir -p "$TMP/store"; ln -s "$ROOT/web" "$TMP/web"
  (cd "$TMP" && exec env PORT=8798 "$ROOT/$BIN" > "$TMP/server.log" 2>&1) & SRV=$!; disown
  for _ in $(seq 1 50); do curl -sf -m 1 localhost:8798/api/caps >/dev/null && break; sleep 0.2; done
  curl -sf localhost:8798/api/caps || { echo "テスト用サーバが起動しない"; cat "$TMP/server.log"; exit 1; }; echo
  set +e; FG_URL=http://localhost:8798 CHROME="$CHROME" node tests/ui_regression.js; RC=$?; set -e
  kill $SRV 2>/dev/null || true; sleep 1; rm -rf "$TMP"
  [ $RC = 0 ] || { echo "回帰テスト失敗 → ビルド中止"; exit 1; }
fi

if [ "$PLAIN" = 0 ]; then
  step "Tauri アプリ殻 (mac/tauri)"
  command -v node >/dev/null || { echo "node が無い(Tauri CLI用)。brew install node か --plain"; exit 1; }
  [ -d mac/tauri/node_modules ] || (cd mac/tauri && npm install --silent)
  cp "$BIN" mac/tauri/src-tauri/binaries/fluent_gallery-aarch64-apple-darwin
  if [ -n "${SIGN:-}" ]; then export APPLE_SIGNING_IDENTITY="$SIGN"; else unset APPLE_SIGNING_IDENTITY; fi  # 未指定=未署名
  (cd mac/tauri && npx tauri build --ci 2>&1 | grep -vE '^\s+(Compiling|Finished)')
  BUNDLE=mac/tauri/src-tauri/target/release/bundle
  rm -rf "$APP" "$DMG"; mkdir -p dist
  cp -R "$BUNDLE/macos/Fluent Gallery.app" "$APP"
  cp "$BUNDLE"/dmg/*.dmg "$DMG"
  step "完了"
  ls -lh "$APP/Contents/MacOS/"* "$DMG" | awk '{print $5, $9}'
  echo "起動: open \"$APP\"   (データ: ~/Library/Application Support/FluentGallery/)"
  exit 0
fi

step "アプリバンドル $APP"
rm -rf "$APP"; mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources/web"
cp "$BIN" "$APP/Contents/MacOS/fluent_gallery"
cp mac/launcher.sh "$APP/Contents/MacOS/FluentGallery"; chmod +x "$APP/Contents/MacOS/"*
cp web/index.html "$APP/Contents/Resources/web/index.html"
[ -f mac/AppIcon.icns ] && cp mac/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
sed -e "s/__BUNDLE_ID__/$BUNDLE_ID/" -e "s/__VERSION__/$VERSION/" mac/Info.plist > "$APP/Contents/Info.plist"
echo "同梱: $(du -sh "$APP" | cut -f1)"

if [ -n "${SIGN:-}" ]; then
  step "署名 ($SIGN)"
  codesign --force --options runtime --timestamp --sign "$SIGN" "$APP/Contents/MacOS/fluent_gallery"
  codesign --force --options runtime --timestamp --sign "$SIGN" "$APP"
  codesign --verify --deep --strict "$APP" && echo "署名OK"
else
  echo "(SIGN 未指定: 未署名。配布するには Developer ID で署名+notarize が必要)"
fi

step "DMG $DMG"
rm -f "$DMG"; STAGE=$(mktemp -d); cp -R "$APP" "$STAGE/"; ln -s /Applications "$STAGE/Applications"
hdiutil create -quiet -volname "Fluent Gallery" -srcfolder "$STAGE" -ov -format UDZO "$DMG"; rm -rf "$STAGE"

if [ -n "${SIGN:-}" ] && [ -n "${NOTARY_PROFILE:-}" ]; then
  step "notarize ($NOTARY_PROFILE)"
  codesign --force --timestamp --sign "$SIGN" "$DMG"
  xcrun notarytool submit "$DMG" --keychain-profile "$NOTARY_PROFILE" --wait
  xcrun stapler staple "$DMG"; xcrun stapler staple "$APP"
fi

step "完了"
ls -lh "$APP/Contents/MacOS/fluent_gallery" "$DMG" | awk '{print $5, $9}'
echo "起動テスト: open $APP   (データ: ~/Library/Application Support/FluentGallery/)"

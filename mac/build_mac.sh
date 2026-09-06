#!/bin/bash
# fluent_gallery Mac販売ビルド: 依存チェック → Metalビルド(顔IDなし) → UI回帰テスト → .app → .dmg → (署名/notarize)
#
#   bash mac/build_mac.sh                      # dist/FluentGallery.app と dist/FluentGallery.dmg を作る
#   bash mac/build_mac.sh --store              # ストア提出版(顔IDなし・YouTube/X拒否・COCOはCC BY系のみ)。既定はフル機能
#   bash mac/build_mac.sh --no-test            # 回帰テストを飛ばす
#   bash mac/build_mac.sh --plain              # Tauri殻なし(素のバイナリ+ブラウザ起動の仮.app)
#   SIGN="Developer ID Application: Your Name (TEAMID)" NOTARY_PROFILE=fluent bash mac/build_mac.sh
#       署名(束の中の llama/sd の dylib と実行ファイルも全部)→ dmg → 公証 → staple。Tauri/plain 共通。
#       事前: developer.apple.com で Developer ID Application 証明書を作ってキーチェーンへ、
#             xcrun notarytool store-credentials fluent --apple-id <Apple ID> --team-id <TEAMID> --password <App用パスワード>
#   SIGN=-  は構造確認(ad-hoc、公証なし)
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
LLAMA_BUILD="${LLAMA_BUILD:-b10797}"   # 同梱する llama.cpp のビルド(macOS arm64 tar.gz がある番号)
SD_BUILD="${SD_BUILD:-master-841-6b3edaa}"  # 同梱する stable-diffusion.cpp のリリース(生成エンジン、MIT、macOS arm64 zip)
SD_ZIP="${SD_ZIP:-sd-master-${SD_BUILD##*-}-bin-Darwin-macOS-26.5.2-arm64.zip}"  # 資産名(sd-master-<hash>-bin-…)の macOS 版数はリリースごとに変わる
APP=dist/FluentGallery.app
DMG=dist/FluentGallery-$VERSION.dmg
step() { printf '\n\033[1;36m== %s\033[0m\n' "$*"; }

# 束の中の実行ファイル/dylib を全部同じ Developer ID で署名してから .app を封印する(順番が逆だと封印が壊れる)。
# Resources/llama, Resources/sd の Mach-O は Tauri の bundler が触らないので自前で回す。
# SIGN="-" は構造確認用(ad-hoc: タイムスタンプ無し。配布は不可)
sign_bundle() {
  local app="$1" id="$2" ts=(--timestamp) rt=(--options runtime)
  [ "$id" = "-" ] && ts=()
  local ent="$ROOT/mac/entitlements.plist"
  while IFS= read -r f; do
    file -b "$f" | grep -q 'Mach-O' || continue
    codesign --force "${rt[@]}" "${ts[@]}" --sign "$id" "$f" || return 1
  done < <(find "$app/Contents/Resources" "$app/Contents/Frameworks" -type f 2>/dev/null \( -name '*.dylib' -o -perm -u+x \) | sort)
  codesign --force "${rt[@]}" "${ts[@]}" --sign "$id" "$app/Contents/MacOS/fluent_gallery" || return 1
  codesign --force "${rt[@]}" "${ts[@]}" --entitlements "$ent" --sign "$id" "$app" || return 1
  codesign --verify --deep --strict "$app" && echo "署名OK: $(codesign -dv "$app" 2>&1 | grep -o 'Authority=[^,]*' | head -1)"
}
# dmg は自前(Tauri の bundle_dmg.sh は Finder を AppleScript で操作するので無人実行だと失敗し、rw.dmg をマウントしたまま残す)
make_dmg() {
  local app="$1" dmg="$2" stage; stage=$(mktemp -d)
  cp -R "$app" "$stage/"; ln -s /Applications "$stage/Applications"
  [ -f "$ROOT/mac/README-install.txt" ] && cp "$ROOT/mac/README-install.txt" "$stage/はじめに読んでください.txt"
  rm -f "$dmg"; hdiutil create -quiet -volname "Fluent Gallery" -srcfolder "$stage" -ov -format UDZO "$dmg"; rm -rf "$stage"
}
# 公証: まず .app を zip で提出して staple(dmg の中の app 自体にチケットが付く=オフラインの初回起動でも通る)、
# そのあと dmg を作り直して署名・提出・staple。dmg を先に作ると中の app が staple 前の物になる
notarize_app_and_dmg() {
  local app="$1" dmg="$2" id="$3" profile="$4" zip
  zip=$(mktemp -d)/FluentGallery.zip
  ditto -c -k --keepParent "$app" "$zip"
  xcrun notarytool submit "$zip" --keychain-profile "$profile" --wait || { echo "app の公証に失敗: xcrun notarytool log <id> --keychain-profile $profile"; return 1; }
  xcrun stapler staple "$app" || return 1
  rm -f "$zip"
  make_dmg "$app" "$dmg"
  codesign --force --timestamp --sign "$id" "$dmg"
  xcrun notarytool submit "$dmg" --keychain-profile "$profile" --wait || { echo "dmg の公証に失敗"; return 1; }
  xcrun stapler staple "$dmg" && spctl -a -vv -t exec "$app" 2>&1 | tail -n 2
}

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
  step "内蔵VLM用 llama-server(公式リリース $LLAMA_BUILD, macOS arm64, MIT)を同梱"
  LL=mac/tauri/src-tauri/llama
  if [ ! -x "$LL/llama-server" ]; then
    rm -rf "$LL"; mkdir -p "$LL"; TMPL=$(mktemp -d)
    curl -sL -o "$TMPL/llama.tar.gz" "https://github.com/ggml-org/llama.cpp/releases/download/$LLAMA_BUILD/llama-$LLAMA_BUILD-bin-macos-arm64.tar.gz"
    tar xzf "$TMPL/llama.tar.gz" -C "$TMPL"
    cp "$TMPL"/llama-*/llama-server "$TMPL"/llama-*/*.dylib "$LL/"; rm -rf "$TMPL"
  fi
  ls "$LL" | wc -l | xargs echo "  llama/ files:"
  step "生成エンジン sd-server(stable-diffusion.cpp $SD_BUILD, macOS arm64 Metal, MIT)を同梱"
  SD=mac/tauri/src-tauri/sd
  if [ ! -x "$SD/sd-server" ]; then
    rm -rf "$SD"; mkdir -p "$SD"; TMPS=$(mktemp -d)
    curl -sL -o "$TMPS/sd.zip" "https://github.com/leejet/stable-diffusion.cpp/releases/download/$SD_BUILD/$SD_ZIP"
    unzip -q -o "$TMPS/sd.zip" -d "$TMPS/sd"
    find "$TMPS/sd" -type f \( -name 'sd-server' -o -name 'sd-cli' -o -name '*.dylib' \) -exec cp {} "$SD/" \;
    rm -rf "$TMPS"; chmod +x "$SD"/sd-*
  fi
  ls "$SD" | wc -l | xargs echo "  sd/ files:"
  # 署名は Tauri に任せず後で束ごと自前で行う(Resources の llama/sd の Mach-O を Tauri は署名しないため)。
  # dmg も Tauri の物は使わない(--bundles app だけ作らせる)
  unset APPLE_SIGNING_IDENTITY
  (cd mac/tauri && npx tauri build --ci --bundles app 2>&1 | grep -vE '^\s+(Compiling|Finished)')
  BUNDLE=mac/tauri/src-tauri/target/release/bundle
  rm -rf "$APP" "$DMG"; mkdir -p dist
  cp -R "$BUNDLE/macos/Fluent Gallery.app" "$APP"
  if [ -n "${SIGN:-}" ]; then
    step "署名 ($SIGN)"
    sign_bundle "$APP" "$SIGN" || { echo "署名失敗"; exit 1; }
  else
    # 未署名のままだと本体だけ linker の ad-hoc 署名で Resources を封印せず「壊れているため開けません」になる → 束ごと ad-hoc で整合
    codesign --force --deep --sign - "$APP" && codesign --verify --deep --strict "$APP" && echo "ad-hoc 署名OK(整合のみ。配布には SIGN= で Developer ID 署名)"
  fi
  step "DMG $DMG"; make_dmg "$APP" "$DMG"
  if [ -n "${SIGN:-}" ] && [ "$SIGN" != "-" ] && [ -n "${NOTARY_PROFILE:-}" ]; then
    step "notarize ($NOTARY_PROFILE)"; notarize_app_and_dmg "$APP" "$DMG" "$SIGN" "$NOTARY_PROFILE" || { echo "公証失敗(xcrun notarytool log <id> --keychain-profile $NOTARY_PROFILE で理由を見る)"; exit 1; }
  elif [ -n "${SIGN:-}" ]; then
    echo "(NOTARY_PROFILE 未指定: 公証なし。xcrun notarytool store-credentials fluent --apple-id … --team-id … --password <app用パスワード> で登録して NOTARY_PROFILE=fluent)"
  fi
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
if [ -x mac/tauri/src-tauri/llama/llama-server ]; then mkdir -p "$APP/Contents/Resources/llama"; cp mac/tauri/src-tauri/llama/* "$APP/Contents/Resources/llama/"; fi
if [ -x mac/tauri/src-tauri/sd/sd-server ]; then mkdir -p "$APP/Contents/Resources/sd"; cp mac/tauri/src-tauri/sd/* "$APP/Contents/Resources/sd/"; fi
[ -f mac/AppIcon.icns ] && cp mac/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
sed -e "s/__BUNDLE_ID__/$BUNDLE_ID/" -e "s/__VERSION__/$VERSION/" mac/Info.plist > "$APP/Contents/Info.plist"
echo "同梱: $(du -sh "$APP" | cut -f1)"

if [ -n "${SIGN:-}" ]; then
  step "署名 ($SIGN)"
  sign_bundle "$APP" "$SIGN" || { echo "署名失敗"; exit 1; }
else
  # 未署名のままだと本体だけ linker の ad-hoc 署名で Resources を封印していない=署名が「壊れている」扱いになり、
  # ダウンロードした人に「壊れているため開けません。ゴミ箱に入れる必要があります」が出る(2026-09-06 実害)。
  # 束ごと ad-hoc 署名して整合を取る。Gatekeeper は通らないが「開発元を確認できない」止まりになり、
  # システム設定 → プライバシーとセキュリティ → 「このまま開く」か xattr -dr com.apple.quarantine で開ける
  codesign --force --deep --sign - "$APP" && codesign --verify --deep --strict "$APP" && echo "ad-hoc 署名OK(整合のみ)"
  echo "(SIGN 未指定: Developer ID 署名なし。配布するには Developer ID Application で署名+notarize が必要)"
fi

step "DMG $DMG"; make_dmg "$APP" "$DMG"

if [ -n "${SIGN:-}" ] && [ "$SIGN" != "-" ] && [ -n "${NOTARY_PROFILE:-}" ]; then
  step "notarize ($NOTARY_PROFILE)"; notarize_app_and_dmg "$APP" "$DMG" "$SIGN" "$NOTARY_PROFILE" || { echo "公証失敗"; exit 1; }
fi

step "完了"
ls -lh "$APP/Contents/MacOS/fluent_gallery" "$DMG" | awk '{print $5, $9}'
echo "起動テスト: open $APP   (データ: ~/Library/Application Support/FluentGallery/)"

#!/bin/bash
# fluent_gallery Mac 版リリース(一発): 最新取り込み → 版番号 → 署名・公証ビルド → ダウンロード状態で検証 → GitHub Release
#
#   bash mac/release.sh                      # Cargo.toml の版番号のまま(タグが既にあれば止まる)
#   bash mac/release.sh --version 0.2.1      # 版番号を上げてコミット(Cargo.toml / tauri.conf.json / Cargo.lock)してからリリース
#   bash mac/release.sh --version 0.3.0 --prerelease --notes notes.md
#   bash mac/release.sh --dry-run            # 公開の直前まで(ビルド・公証・検証は行う)
#   bash mac/release.sh --dry-run --skip-build   # 手順の確認だけ(dist にある物を使う)
#
# 前提(この Mac には全部ある): Developer ID Application 証明書(キーチェーン)、notarytool のキーチェーンプロファイル、gh でログイン済み。
# 環境変数で差し替え可: SIGN(署名ID) NOTARY_PROFILE(既定 fluent) REPO(既定 takatronix/fluent_gallery) BRANCH(既定 main)
set -euo pipefail
cd "$(dirname "$0")/.."; ROOT=$PWD
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
SIGN="${SIGN:-Developer ID Application: Takashi Otsuka (382TE93684)}"
NOTARY_PROFILE="${NOTARY_PROFILE:-fluent}"
REPO="${REPO:-takatronix/fluent_gallery}"
BRANCH="${BRANCH:-main}"
NEWVER=""; PRE=0; NOTES=""; DRY=0; SKIP_BUILD=0
while [ $# -gt 0 ]; do case "$1" in
  --version) NEWVER="$2"; shift 2;;
  --prerelease) PRE=1; shift;;
  --notes) NOTES="$2"; shift 2;;
  --dry-run) DRY=1; shift;;
  --skip-build) SKIP_BUILD=1; shift;;
  *) echo "unknown option: $1" >&2; exit 2;;
esac; done
step() { printf '\n\033[1;36m== %s\033[0m\n' "$*"; }
die() { echo "❌ $*" >&2; exit 1; }

step "前提チェック"
[ "$(uname -m)" = arm64 ] || die "Apple Silicon 専用"
security find-identity -v -p codesigning | grep -q "$SIGN" || die "署名 ID がキーチェーンに無い: $SIGN"
xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null 2>&1 || die "notarytool のプロファイル '$NOTARY_PROFILE' が無い(xcrun notarytool store-credentials $NOTARY_PROFILE --apple-id … --team-id … --password …)"
gh auth status >/dev/null 2>&1 || die "gh が未ログイン(gh auth login)"
command -v cargo >/dev/null || die "cargo が無い"
[ "$(git branch --show-current)" = "$BRANCH" ] || die "ブランチが $BRANCH でない: $(git branch --show-current)"
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then git status --short --untracked-files=no; die "未コミットの変更がある(先にコミットするか捨てる)"; fi
# 前回の失敗で残った dmg のマウントを外す(bundle_dmg.sh が残す rw.dmg など)
for d in $(hdiutil info | awk '/image-path.*Fluent Gallery/{f=1} f && /^\/dev\/disk[0-9]+[[:space:]]/{print $1; f=0}'); do hdiutil detach "$d" -force >/dev/null 2>&1 || true; done
echo "署名: $SIGN / 公証: $NOTARY_PROFILE / repo: $REPO / branch: $BRANCH"

step "最新を取り込む(origin/$BRANCH)"
git fetch -q origin
BEHIND=$(git rev-list --count "HEAD..origin/$BRANCH"); AHEAD=$(git rev-list --count "origin/$BRANCH..HEAD")
echo "behind=$BEHIND ahead=$AHEAD"
if [ "$BEHIND" != 0 ]; then
  git merge --no-edit "origin/$BRANCH" || die "マージが衝突した。解決してから再実行"
fi

# 版番号
CUR=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')
VER="${NEWVER:-$CUR}"
echo "$VER" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.]+)?$' || die "版番号の形が変: $VER"
if git ls-remote --tags origin | grep -q "refs/tags/v$VER$"; then die "タグ v$VER は既に origin にある(--version で新しい番号を)"; fi
if [ -n "$NEWVER" ] && [ "$NEWVER" != "$CUR" ]; then
  step "版番号 $CUR → $NEWVER"
  sed -i '' "0,/^version = \"$CUR\"/s//version = \"$NEWVER\"/" Cargo.toml
  sed -i '' "0,/^version = \"$CUR\"/s//version = \"$NEWVER\"/" mac/tauri/src-tauri/Cargo.toml
  python3 - "$NEWVER" <<'EOF'
import json,sys,re
p='mac/tauri/src-tauri/tauri.conf.json'; t=open(p,encoding='utf-8').read()
t=re.sub(r'"version":\s*"[^"]+"', '"version": "%s"' % sys.argv[1], t, count=1); open(p,'w',encoding='utf-8').write(t)
EOF
  grep -m1 '^version' Cargo.toml; grep -m1 '"version"' mac/tauri/src-tauri/tauri.conf.json
fi
APP=dist/FluentGallery.app; DMG="dist/FluentGallery-$VER.dmg"

if [ "$SKIP_BUILD" = 0 ]; then
  step "ビルド + 署名 + 公証 (mac/build_mac.sh)"
  SIGN="$SIGN" NOTARY_PROFILE="$NOTARY_PROFILE" bash mac/build_mac.sh || die "ビルド/公証に失敗"
else
  echo "(--skip-build: dist の既存物を使う)"
fi
[ -d "$APP" ] && [ -f "$DMG" ] || die "成果物が無い: $APP / $DMG"

step "版番号のコミット(あれば)"
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
  git add Cargo.toml Cargo.lock mac/tauri/src-tauri/Cargo.toml mac/tauri/src-tauri/tauri.conf.json 2>/dev/null || true
  git commit -q -m "v$VER" && echo "commit: $(git log --oneline -1)"
fi

step "配布物の検証(ダウンロード直後の状態を模す)"
xcrun stapler validate "$APP" >/dev/null || die "app にチケットが無い"
xcrun stapler validate "$DMG" >/dev/null || die "dmg にチケットが無い"
T=$(mktemp -d); cp "$DMG" "$T/dl.dmg"; xattr -w com.apple.quarantine "0083;$(printf '%x' "$(date +%s)");Chrome;" "$T/dl.dmg"
spctl -a -t open --context context:primary-signature "$T/dl.dmg" 2>&1 | grep -q accepted || die "quarantine 付き dmg が Gatekeeper に弾かれる"
mkdir -p "$T/mnt"; hdiutil attach -nobrowse -readonly -mountpoint "$T/mnt" "$T/dl.dmg" -quiet
INNER="$T/mnt/Fluent Gallery.app"
spctl -a -vv -t exec "$INNER" 2>&1 | grep -q 'Notarized Developer ID' || { hdiutil detach "$T/mnt" -quiet; die "dmg の中の app が公証済みと判定されない"; }
xcrun stapler validate "$INNER" >/dev/null || { hdiutil detach "$T/mnt" -quiet; die "dmg の中の app にチケットが無い"; }
hdiutil detach "$T/mnt" -quiet; rm -rf "$T"
SHA256=$(shasum -a 256 "$DMG" | cut -d' ' -f1)
echo "OK: dmg も中の app も Notarized Developer ID。sha256=$SHA256 size=$(du -h "$DMG" | cut -f1)"

step "リリースノート"
NOTEFILE=$(mktemp)
LASTTAG=$(git describe --tags --abbrev=0 --match 'v*' 2>/dev/null || true)
if [ -n "$NOTES" ]; then
  cat "$NOTES" > "$NOTEFILE"
else
  {
    echo "Fluent Gallery $VER — Mac(Apple Silicon)版。Developer ID で署名し、Apple の公証(notarization)を通しています。ダウンロードしてそのまま開けます。"
    echo
    echo "## 入れ方"
    echo "1. \`FluentGallery-$VER.dmg\` を開き、\`Fluent Gallery.app\` を Applications へドラッグ"
    echo "2. 起動すると \`~/Library/Application Support/FluentGallery/\` にデータを置き、必要なモデルは初回に取得します(設定 → AI から個別に)"
    echo
    if [ -n "$LASTTAG" ]; then
      echo "## $LASTTAG からの変更"
      git log "$LASTTAG..HEAD" --no-merges --format='- %s' | grep -v '^- v[0-9]' | head -60
      echo
    fi
    echo "## 動作環境"
    echo "- macOS 14 以降 / Apple Silicon。生成には統合メモリ 32GB 以上を推奨(Qwen-Image-Edit は 64GB 以上)"
    echo
    echo "sha256: \`$SHA256\`"
  } > "$NOTEFILE"
fi
sed -n '1,40p' "$NOTEFILE"

if [ "$DRY" = 1 ]; then
  step "dry-run: ここまで。公開するなら --dry-run を外して再実行(成果物はそのまま使えるので --skip-build 可)"
  echo "予定: git push origin $BRANCH && gh release create v$VER $DMG -R $REPO --target $(git rev-parse HEAD) --title 'Fluent Gallery $VER (macOS, Apple Silicon)' $([ $PRE = 1 ] && echo --prerelease)"
  exit 0
fi

step "push と GitHub Release v$VER"
git push origin "$BRANCH"
PREFLAG=(); [ "$PRE" = 1 ] && PREFLAG=(--prerelease)
gh release create "v$VER" "$DMG" -R "$REPO" --target "$(git rev-parse HEAD)" \
  --title "Fluent Gallery $VER (macOS, Apple Silicon)" --notes-file "$NOTEFILE" "${PREFLAG[@]}"
gh release view "v$VER" -R "$REPO" --json url,isPrerelease,assets -q '"公開: \(.url)  prerelease=\(.isPrerelease)  assets=\([.assets[].name] | join(","))"'
rm -f "$NOTEFILE"
step "完了 v$VER"

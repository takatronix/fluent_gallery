# ブラウザ内蔵クローラー(browse エンジン)仕様 v1 — 2026-09-07

fluent_gallery から呼ばれる **人間の目線でサイトを見て回り、関連しそうな画像を集めて gallery に渡す** クローラー。
同じ仕様で **codex 版と fable 版** を別々に実装し、同じベンチで比べて性能の高い方を採用する。
本書は両実装の共通契約。**判断(採用/不採用)は後段の gallery 側 AI(目利き VLM)がやる**ので、
クローラーの仕事は「人が見て関係ありそうな画像を、出典つきで、行儀よく、決められた範囲内で集める」こと。

研究目的。robots.txt と利用規約を守り、ログイン壁・CAPTCHA・レート乱用は一切しない。

---

## 1. 形

- **独立した Node.js プロジェクト**(依存は `playwright` のみ。Chromium 同梱)。gallery 本体(Rust)は触らない。
- **HTTP サービス**(127.0.0.1 のみ待受)+ **CLI** の 2 口。gallery は HTTP で呼ぶ。
- 1 ジョブ = 1 ブラウザコンテキスト = **人が 1 枚のタブで見て回る**のと同じ(ページ並列は最大 1、画像 DL の並列は 4 まで)。
- 実装の識別子(`impl`): codex 版 = `codex`、fable 版 = `fable`。ポート: codex 版 **8797**、fable 版 **8796**(同時起動して比較する)。
- 起動: `node server.js --port 8797`  /  CLI: `node crawl.js --url <URL> --goal "<目標>" --album <名前> [--no-deliver] [--max-pages 20 ...]`
- `npm install` → `npx playwright install chromium` で動くこと(既に `~/Library/Caches/ms-playwright` にキャッシュあり)。

## 2. HTTP API(両実装で完全一致させる)

```
GET  /health                → {ok:true, impl:"codex"|"fable", version, browser:"chromium <ver>"}
POST /jobs                  → {ok:true, id}            (下の JobIn)
GET  /jobs                  → {jobs:[JobStatus...]}     (新しい順、最大 50)
GET  /jobs/{id}             → JobStatus
POST /jobs/{id}/stop        → {ok:true}                 (今のページを終えたら止まる。取った分は渡し済み)
GET  /jobs/{id}/log         → text/plain  jsonl(ページごとの判断ログ、§6)
```

### JobIn
```jsonc
{
  "url": "https://example.org/gallery",     // 開始 URL(必須)
  "goal": "柴犬の写真。イラスト不可",         // 何を集めたいか(自然言語・空なら「本文の画像すべて」)
  "album": "shiba",                         // gallery のフォルダ名(必須)
  "gallery": "http://127.0.0.1:8793",       // 渡し先(既定 http://127.0.0.1:8793)
  "deliver": true,                          // false = 渡さず jobs/<id>/images/ に保存(gallery 無しで開発・比較するため)
  "judge": true,                            // gallery 側で目利き VLM に判定させる(false=保留のまま収蔵)
  "headless": true,
  "limits": {                               // 全部に既定あり。永遠に探索しないための安全弁
    "max_pages": 30,                        // 訪問ページ数
    "max_images": 100,                      // 渡した(または保存した)画像数
    "max_minutes": 10,                      // 壁時計
    "max_depth": 3,                         // 開始 URL からのリンク段数
    "max_bytes_mb": 300,                    // ダウンロード総量
    "same_site": true,                      // 同一サイト(eTLD+1)内だけ。false でも別サイトは 1 段だけ
    "min_side": 300,                        // 短辺がこれ未満の画像は取らない(アイコン/ボタン/追跡ピクセル)
    "bored_pages": 6,                       // 連続でこの数のページから何も取れなかったら終了(飽きる)
    "delay_ms": [800, 2500]                 // ページ遷移の間隔(乱数、人のペース。礼儀。下限 500 未満は不可)
  }
}
```

### JobStatus
```jsonc
{
  "id": "j_20260907_ab12", "impl": "fable", "state": "queued|running|done|stopped|error",
  "album": "shiba", "goal": "...", "start_url": "...",
  "started": 1757000000.0, "elapsed_s": 42.1,
  "pages_visited": 7, "frontier": 23,          // 訪問数 / これから見る候補数
  "images_seen": 210,                          // ページ上で見た画像(全部)
  "images_picked": 41,                         // 人目線で「関係ありそう」と選んだ数(= DL を試みた数)
  "delivered": 30, "accepted": 22, "rejected": 6, "dup": 2, "failed": 0,  // 渡した / 目利き通過 / 目利き却下 / 既存 / 失敗
  "bytes": 48213344,
  "current": {"url": "...", "title": "..."},
  "recent": [{"url": "...", "title": "...", "picked": 5, "delivered": 4, "why": "本文の図版 5 枚。次: /photos/page/2 (ページ送り)"}], // 直近 10 ページ
  "stop_reason": "max_images" | "max_pages" | "max_minutes" | "max_depth_exhausted" | "bored" | "frontier_empty" | "stopped" | "error:<msg>" | null,
  "errors": 1
}
```

## 3. gallery への引き渡し(受け口は gallery 側に新設。両実装共通)

```
POST {gallery}/api/deliver      multipart/form-data
  meta = JSON 文字列(下)      ← 先に送る
  file = 画像バイト列(jpg/png/webp/gif)
→ 200 {ok:true, sha1, verdict:"accepted"|"pending"}
   200 {ok:false, reason:"dup"|"bad"|"rejected"|"too_small", why:"目標と不一致" など}
   4xx/5xx = 失敗(リトライは 1 回まで、失敗はカウントして続行)
```
meta:
```jsonc
{
  "album": "shiba",
  "judge": true,
  "rights": "unknown",
  "crawl": {
    "engine": "browser:fable",              // browser:<impl>
    "url": "https://.../full.jpg",          // 取った画像の URL(最終的に DL した最高解像度の方)
    "landing": "https://.../post/123",      // 載っていたページ
    "title": "ページタイトル",
    "query": "柴犬の写真。イラスト不可",       // = goal
    "album": "shiba",
    "tags": ["shiba", "example.org"],       // 固有名詞っぽい語 + ホスト。最大 6
    "alt": "img の alt", "caption": "figcaption / 直近の見出し", "context": "周辺テキスト ≤300 文字",
    "score": 0.83, "depth": 1, "page_index": 3
  }
}
```
gallery はこれを `source = "crawl:<album>"` で収蔵し(既存の AI フォルダと同じバケツ)、`judge:true` かつフォルダに goal が
あれば内蔵 VLM で目利きして `accepted`/`rejected` を返す。**crawler 側は verdict を数えるだけ**で、判断には関与しない。
`deliver:false` のときは `jobs/<id>/images/<sha1>.<ext>` と `jobs/<id>/images/<sha1>.json`(上の meta)を書く。

## 4. 「人と同じ目線」= 必ずやること

1. **見る**: 実寸のビューポート(1440×900)で開き、読み込み完了を待ち、**上から下へ少しずつスクロール**(遅延読み込み・無限スクロールを人のペースで踏む。1 ページのスクロールは上限あり、例: 12 画面ぶん)。Cookie 同意/ポップアップは一般的なボタン(同意/閉じる/OK/Accept)を試して閉じる。
2. **読む**: タイトル・見出し・本文の要旨・**画像の目録**を取る。画像 1 枚ごとに: 画面上の表示サイズ、naturalWidth/Height、alt、figcaption/近くの見出し/周辺テキスト、`<nav>/<header>/<footer>/<aside>`・広告 iframe の中かどうか、`srcset`/`<picture>` の最大候補、リンク先が原寸画像か(a[href] が画像・lightbox の data-* )、CSS 背景画像。
3. **選ぶ(画像)**: 人が「これはこのページの内容の画像だ」と思う物だけ。除外: アイコン/ロゴ/アバター/ボタン/スプライト/追跡ピクセル/広告/**複数ページで繰り返し出る飾り**(3 ページ以上で同じ URL = サイトの部品)。goal があれば alt/caption/context との関連で順位付け。取るときは**最高解像度の版**(srcset 最大・リンク先原寸)を選ぶ。短辺 `min_side` 未満は取らない。
4. **選ぶ(次のページ)**: ページ内リンクを **goal との関連(アンカー文・周辺文・URL の語)** と **構造の手がかり**(ページ送り「次へ/›/page=2」、gallery/photos/album/portfolio/category、本文中の記事リンク)で点数化し、**良い順に**巡る(best-first のフロンティア)。同一サイト優先、深さ制限、再訪なし。login/logout/cart/signup/search?/mailto/tel/PDF 等の非 HTML は行かない。
5. **礼儀**: robots.txt(`User-agent: *` の Disallow)を守る。ページ間隔 `delay_ms`。UA は Chromium 標準に ` fluent_crawler/0.1 (+research)` を付ける。同一ホスト同時 1 ページ。動画/SNS 媒体(youtube/x/instagram/facebook/tiktok/pinterest)は対象外(別パイプライン)。
6. **やめる**: §2 の limits のどれかに達したら止まる(`stop_reason` に理由)。「飽きる」(bored_pages)も必須。ページ 1 枚のタイムアウト 30 秒、ジョブ全体は max_minutes の番犬。1 ページの失敗はジョブを止めない。
7. **重複**: バイト sha1 と近似ハッシュ(dHash 64bit、ハミング ≤ 6)でジョブ内重複を落とす。gallery 側の `dup` も数える。

## 5. 任意(加点)

- **LLM 補助**: `FG_LLM_BASE`(OpenAI 互換 chat)が環境にあれば、リンクと画像目録の順位付けに使ってよい。**無くても動くこと**(ヒューリスティック単独で成立)。gallery の内蔵 LLM(Qwen3-4B)は `{gallery}/api/nlq` 等では使えないので、比較ベンチは LLM 無しで行う。
- 画像の意味的な近さ(CLIP 等)はクローラーの仕事ではない(gallery 側でやる)。
- スクリーンショットで「ページの見た目」を VLM に読ませる方式も可(ただし LLM 無しモードは必須)。

## 6. ログ(比較のため必須)

`jobs/<id>/log.jsonl` に 1 ページ 1 行:
```jsonc
{"t": 1757000012.3, "url": "...", "title": "...", "depth": 1, "score": 0.71,
 "why_visited": "アンカー『Photos』/ 本文リンク / goal 語 2 語一致",
 "images_seen": 34, "picked": [{"url": "...", "w": 1600, "h": 1067, "score": 0.9, "why": "本文の図版・alt に shiba"}],
 "skipped": {"icon": 20, "nav": 5, "repeat": 3, "small": 2, "ad": 1},
 "next": [{"url": "...", "score": 0.8, "why": "ページ送り"}],
 "delivered": 5, "accepted": 4, "rejected": 1, "ms": 4210}
```

## 7. ベンチ(採用判定はこれで)

`bench/run.js --impl fable|codex --site <name>` で同じサイト・同じ goal・同じ limits で走らせ、以下を並べる:
- 渡した枚数 / 目利き通過率(accepted ÷ delivered = **クローラーの目の良さ**)/ 通過 1 枚あたり秒・バイト
- ページ数、bored/limit のどれで止まったか、エラー数、部品画像(アイコン等)の混入(ログの手動目視)
- 対象(公開・robots 許可・研究向け): Wikimedia Commons のカテゴリ、Met Museum Open Access の検索結果、NASA 画像ギャラリー、一般的なブログ 1 つ。goal は例:「柴犬の写真」「印象派の油彩」「土星の写真」。

## 8. ディレクトリ

```
<impl root>/
  package.json  server.js  crawl.js(CLI)  lib/...   README.md
  jobs/<id>/{status.json,log.jsonl,images/}   (git 管理外)
  bench/run.js  bench/sites.json
```
fable 版は fluent_gallery リポジトリ内 `crawler/`、codex 版は `~/projects/fluent_crawler-codex/`。

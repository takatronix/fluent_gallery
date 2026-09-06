# fluent_crawler(fable 版)— ブラウザ内蔵インテリジェント画像クローラー

fluent_gallery から呼ばれ、**人と同じ目線で指定サイトを見て回り、内容の画像を出典つきで gallery に渡す**クローラー。
Node.js + Playwright(Chromium 同梱)。依存は `playwright` だけ。仕様(codex 版と共通の契約)は
[docs/browser-crawler-spec.md](../docs/browser-crawler-spec.md)。

- クローラーは**選ぶだけ**(部品を落とし、見込み順に最高解像度で取る)。**採否は gallery 側の目利き(内蔵 VLM)**が決める(`POST /api/deliver`)。
- 研究目的。robots.txt 厳守、ページ間隔(既定 0.8〜2.5 秒の乱数)、ログイン/CAPTCHA/動画 SNS 媒体は扱わない。UA に `fluent_crawler/0.1 (+research)` を付ける。

## 動かす

```bash
cd crawler && npm install && npx playwright install chromium
node server.js --port 8796            # HTTP サービス(gallery の /api/browse が未起動なら自動でこれを起こす)
node crawl.js --url https://commons.wikimedia.org/wiki/Category:Shiba_Inu --goal "柴犬の写真。イラスト不可" --album bench_shiba \
     --gallery http://127.0.0.1:8798 --max-pages 6 --max-images 8      # CLI(--no-deliver で jobs/<id>/images/ に保存)
node test/unit.js                     # ブラウザ無しの芯(語切り出し/robots/部品判定/順位付け)
node bench/run.js --impl fable --deliver --gallery http://127.0.0.1:8798   # 比較ベンチ(bench/sites.json)
```

gallery 側: `POST /api/browse {album,url,goal?,limits?}` → `GET /api/browse/status` / `POST /api/browse/stop`。
MCP(`mcp/gallery_mcp.py`)の `browse_start / browse_status / browse_stop`。UI は「取り込み」パネルの「見て回って集める」。
設定 `crawler.base`(別プロセス/別マシンのサービス)、`crawler.dir`(server.js の場所)、`crawler.port`。環境変数 `FG_CRAWLER_BASE/DIR/PORT`、`FG_NODE`。

## API(spec §2 と同じ)

`GET /health` / `POST /jobs` / `GET /jobs` / `GET /jobs/{id}` / `POST /jobs/{id}/stop` / `GET /jobs/{id}/log`(jsonl)。
JobIn の `limits`: max_pages 30 / max_images 100 / max_minutes 10 / max_depth 3 / max_bytes_mb 300 / same_site true / min_side 300 / bored_pages 6 / delay_ms [800,2500](下限 500)。

## 設計の要点(人の目線をどう機械にしたか)

| 人がやること | 実装 |
|---|---|
| 1 枚のタブで順に見る | 1 ジョブ = 1 コンテキスト、ページ並列なし。画像 DL はコンテキストの Cookie/UA/Referer を共有 |
| 同意バナーを閉じる | 「同意/OK/閉じる/Accept…」の見えているボタンを 1 回だけ押す(`lib/page.js dismissConsent`) |
| 上から下へ読む | ホイール事象で 0.6〜0.9 画面ずつ、最大 12 画面。底で 0.9 秒待って無限スクロールも踏む(`humanScroll`) |
| 何が載っているか把握する | DOM 目録: 画像ごとに表示寸法/natural 寸法/alt/figcaption/近い見出し/周辺 300 字/`nav,header,footer,aside,form,dialog` 内か/`main,article,figure` 内か/srcset・`<picture>`・data-* の大きい版/リンク先原寸。CSS 背景と og:image も(`inventoryScript`) |
| 部品を無視する | アイコン/ロゴ/アバター/スプライト/ボタン/追跡ピクセル(class・URL・alt の語)、svg/ico、nav/footer 内、広告ホスト、**3 ページ以上に出る同じ画像(サイトの部品)**、極端な縦横比、短辺 min_side 未満で大きい版も無い物(`lib/score.js junkImage`) |
| 内容の画像を選ぶ | 点 = 大きさ(natural/srcset/リンク先原寸の対数)+ 画面上の見せ方 + 位置(本文/図版/caption)+ goal との語一致(alt/caption/見出し/周辺文/ファイル名)。0.30 未満は取らない、1 ページ最大 40 枚 |
| 最高解像度で保存する | 候補順: リンク先が画像 → srcset 最大 → data-* → CDN の原寸書き換え(Wikimedia thumb→原寸、WordPress -WxH 除去、w=/width= 除去)→ currentSrc。ブラウザが読み込んだ画像レスポンスは捕まえて再 DL しない |
| 同じ物を二度取らない | sha1 + dHash(64bit、ハミング ≤ 6)。復号も dHash も **ブラウザ内の OffscreenCanvas** でやる(画像ライブラリ不要) |
| 次にどこを見るか | best-first フロンティア。点 = goal との語一致(アンカー/周辺文/URL 語)+ ページ送り(rel=next、「次へ」、page=2)+ gallery/photos/category/File: + **サムネを包むリンク**(人はサムネをクリックする)+ 本文内。nav/footer、定型リンク(about/login…)、別サイト/別ホスト、深さで減点。写真でない File:(svg/pdf/ogg)は減点 |
| 行かない所 | robots.txt Disallow、login/cart/Special:/edit/history、非 HTML(pdf/zip/動画)、内部ネットワーク、youtube/x/instagram/facebook/tiktok/pinterest |
| 飽きる・やめる | 全リミット + 連続 bored_pages ページ収穫ゼロで終了。ページ 30 秒/ジョブ番犬(max_minutes+90 秒)。1 ページの失敗はジョブを止めない |

任意の LLM 補助(`FG_LLM_BASE`=OpenAI 互換 chat、例: gallery の llama-server `http://127.0.0.1:8081/v1`): goal の**主語の翻訳だけ**を頼み(「柴犬」→ shiba inu)、語一致に使う。
小型モデルに連想させると嘘が混ざる(柴犬→chihuahua)ので翻訳以外は頼まない。無ければヒューリスティックのみ。

## ログ

`jobs/<id>/log.jsonl`(1 ページ 1 行: why_visited / images_seen / picked[{url,w,h,score,why,result}] / skipped{icon,nav,repeat,small,low…} / next[{url,score,why}] / delivered / accepted / rejected / ms)、`status.json`。
`deliver:false` のときは `jobs/<id>/images/<sha1>.<ext>` と `<sha1>.json`(meta)。

## 既知の弱点

- 語一致は文字列ベース。日本語 goal と英語サイト(またはその逆)は LLM 翻訳が無いと関連点が 0 になり、大きさ/位置だけで選ぶ(それでも目利きが後で落とす)。
- iframe の中は見ない(広告は結果的に無視できるが、iframe 埋め込みギャラリーも見えない)。
- 無限スクロールは 12 画面で打ち切り。ログイン壁/年齢確認/JS チャレンジ(Cloudflare 等)は突破しない(そのページは失敗として次へ)。
- 同一サイト判定は eTLD+1 の簡易版(co.jp 等は考慮)。

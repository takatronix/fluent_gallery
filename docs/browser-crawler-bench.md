# ブラウザ内蔵クローラー 比較ベンチ(fable 版 vs codex 版)— 2026-09-07

仕様は [browser-crawler-spec.md](browser-crawler-spec.md)。両実装を **同じハーネス**(`crawler/bench/run.js`)・同じサイト/goal/limits・
同じ受け口(開発用 gallery :8798、内蔵 VLM Qwen3-VL-4B が目利き、各回ともフォルダを空にしてから)で走らせた。LLM 補助なし。
「通過率」= 目利き通過 ÷ (渡した − 重複) = **クローラーの目の良さ**。「取れた」= 渡した − 重複 − 失敗。

## 結果

fable 版(`crawler/`、リポジトリ内。結果 `crawler/bench/results-fable-2026-09-06T20-25-28.json`)

| site | 状態 | 秒 | ページ | 見た | 選んだ | 取れた | 渡した | 通過 | 却下 | 重複 | 失敗 | 通過率 | MB |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| commons_shiba | max_pages | 80 | 12 | 427 | 22 | 11 | 11 | **10** | 1 | 0 | 0 | **91%** | 28 |
| nasa_saturn | max_images | 49 | 3 | 148 | 37 | 30 | 30 | **18** | 12 | 0 | 0 | 60% | 6 |
| met_impressionism | max_images | 42 | 1 | 42 | 40 | 30 | 30 | 1 | 29 | 0 | 0 | 3% | 2 |
| blog_wpthemes | max_images | 44 | 2 | 29 | 24 | 20 | 20 | **16** | 4 | 0 | 0 | 80% | 22 |

codex 版(`~/projects/fluent_crawler-codex/`。結果 `crawler/bench/results-codex-2026-09-06T20-29-37.json`)

| site | 状態 | 秒 | ページ | 見た | 選んだ | 取れた | 渡した | 通過 | 却下 | 重複 | 失敗 | 通過率 | MB |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| commons_shiba | bored | 30 | 8 | 317 | 8 | − | 1 | 1 | 0 | 0 | **7** | (100%) | 24 |
| nasa_saturn | max_pages | 140 | 10 | 232 | 38 | 20 | 29 | 17 | 12 | 3 | 6 | 65% | **158** |
| met_impressionism | frontier_empty | 0.3 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | − | 0 |
| blog_wpthemes | bored | 25 | 6 | 51 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | − | 20 |

## 読み方

- **Commons 柴犬**: 両方とも「カテゴリ → File: ページ → 原寸」と人と同じ順路を辿った(Commons は `/w/` が robots Disallow なのでページ送りは踏めず、
  1 ページ目の 200 枚ぶんで止まるのは正しい)。fable は 12 ページで 10 枚通過(却下 1 = ダルメシアンと柴犬の写真)。codex は原寸の取得が 8 回中 7 回失敗
  (ログ w=0,h=0、failed=1)し、1 枚渡した後「飽き」で終了。
- **NASA 土星**: 通過枚数は同程度(18 vs 17)だが、fable は 3 ページ・49 秒・6MB、codex は 10 ページ・140 秒・158MB。両方とも却下 12 は
  土星ページに埋まっている水星/月/探査機など「土星ではない内容画像」で、テキストだけでは判別できない(目利きの仕事どおり)。
- **WordPress showcase**: fable は 2 ページで 16 枚通過。codex は 51 枚全部を "irrelevant"(goal 語不一致)で捨て 0 枚。
  日本語 goal と英語サイトでは語一致が常に 0 になるので、**語一致を門にすると何も集まらない**。fable は「本文の大きな内容画像」は語一致が無くても渡し、
  採否を後段の目利きに任せる(ユーザー方針「使うかどうかは後の AI が判断」に沿う)。
- **Met**: サイトが Vercel の bot チェックポイントを出す(robots.txt すら HTML が返る)。fable が見たのは検索結果ではなくエジプト美術のグリッド
  (目利きが 29 枚正しく却下)、codex は 0 ページで終了。**このサイトは比較に使えない**(次回は Met の Open Access API か別の美術館に差し替える)。

## 判定

**fable 版を採用**(3 サイトで優位、1 サイトは比較不能)。理由: (1) 原寸取得と引き渡しが安定(失敗 0)、(2) 同じ収穫で 3 倍速く 26 分の 1 の転送量、
(3) 語一致に頼らず「内容画像」を選ぶので多言語 goal でも収穫がある。codex 版は設計(単一タブ・robots・best-first・sha1+dHash・145 件の単体テスト)は
仕様どおりだが、agent-hub のサンドボックス(workspace-write)で **Chromium 起動・localhost 待受・外部通信・git commit が全部拒否**され、
実サイトで一度も走らせずに納品されたため、取得の失敗と関連度の門の厳しさが実測で直せていない。

## 手順(再現)

```bash
# 受け口(開発用 gallery を scratch の store で :8798 に)… 本番では FluentGallery.app の /api/browse をそのまま使う
cd crawler && node server.js --port 8796 &                      # fable
(cd ~/projects/fluent_crawler-codex && node server.js --port 8797 &)   # codex
node bench/run.js --impl fable --deliver --gallery http://127.0.0.1:8798
node bench/run.js --impl codex --base http://127.0.0.1:8797 --deliver --gallery http://127.0.0.1:8798
```
各回の前に `POST /api/source/trash {"source":"crawl:<album>"}` でフォルダを空にする(重複が「取れた」を減らすため)。

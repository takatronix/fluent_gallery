# fluent_gallery

**完全ローカルAIの画像ライブラリ。** 大量の画像を超高速で「見る → 調整する → フィルタする → あの手この手で増やす → データセットにする」まで一つの流れでこなす。Rust単一バイナリ(axum)+内蔵AI(llama.cpp / ONNX Runtime / ollama)で動き、画像は外に出ない。

現在の稼働規模: **約13万枚 / 20GB**(:8790)

---

## できること

| | |
|---|---|
| **収蔵** | フォルダ/カメラ/動画(1fps間引き+HDRトーンマップ)/URL。SHA-1のcontent-addressed保存+ハードリンク収蔵で容量ゼロ |
| **AIフォルダ** | 「何を集めたいか」を日本語で書くと、AIがクエリを作り7つの検索元から集め、目利きAIが1枚ずつ合否判定して収蔵。♻自動補充で置いとくと増える |
| **顔ID** | 顔検出(SCRFD)+顔埋め込み(ArcFace)で本人を無料・確定的に判定。登録した人物以外は収集時に門前払い、既存画像にも遡及タグ付け、人物名で検索 |
| **VLM属性** | 内蔵qwen2.5vlが caption/tags/scene/subject/style/gender/age/framing/watermark 等を自動付与 |
| **マスク** | GroundingDINO+SAM2(ml-hub経由)で自動セグメント。点/箱クリックの手動修正、切り抜きPNG書き出し |
| **似た画像** | CLIP埋め込みによる類似検索。似ている順の動的フォルダを作れる |
| **非破壊編集** | 露出/コントラスト/彩度/色温度/回転/反転/クロップ/フィルタ。原本は不変、履歴スタックはサイドカーに |
| **整理** | フォルダ/グループ/データセット/棚の名称変更(ダブルクリック or ✎)、D&Dで移動(同じ木の中だけ)、フォルダ同士の合流(確認ポップアップ付き・画像は消えない) |
| **払い出し** | 選択やフィルタ結果を `store/datasets/<name>/` へsymlink+manifestで出荷。ml-hubの学習にそのまま使える |
| **AI 1st** | 全操作がAPI。MCPサーバ(`mcp/`)経由でAIが自律運用できる |

---

## 動かす

```bash
# ビルド(このマシンはBINDGEN/RUSTFLAGSが必須 — build.shが面倒を見る)
bash build.sh

# ビルド→再起動→UI回帰テストまで(通常はこれ一発)
bash deploy.sh
```

サーバは `./target/release/fluent_gallery`(既定 :8790、`PORT`環境変数で変更可)。

### ビルド機能(Cargo feature)

| feature | 既定 | 内容 |
|---|---|---|
| `faceid` | ON | 顔ID(SCRFD+ArcFace)。insightface の buffalo_l モデルは**非商用限定**なので、ストア版では外す。モデルは初回に `/api/faces/pull` で自動取得(288MB) |
| `store` | OFF | ストア提出版の制限: YouTube/X/Instagram 等の取り込み拒否、COCO は CC BY 系のみ。既定OFF=フル機能 |
| `cuda` | ON | 内蔵LLM(llama.cpp)を CUDA で動かす(Linux/NVIDIA) |
| `metal` | OFF | 同 Metal(Apple Silicon)。`cuda` とどちらか一つ |

```bash
cargo build --release                                              # Linux/CUDA・顔IDあり(=deploy.sh)
cargo build --release --no-default-features --features metal,faceid # Mac・フル機能(自分用)
cargo build --release --no-default-features --features metal,store  # Mac・ストア提出版
```

### Mac 販売ビルド(.app / .dmg)

```bash
bash mac/build_mac.sh            # Metal・フル機能(顔IDあり) → 回帰テスト → dist/FluentGallery.app + .dmg
bash mac/build_mac.sh --store    # ストア提出版(顔IDなし・YouTube/X拒否・COCOはCC BY系のみ)
SIGN="Developer ID Application: ..." NOTARY_PROFILE=fg bash mac/build_mac.sh   # 署名+notarize
```

.app は Tauri 2 の殻(`mac/tauri/`)。同梱したサーバをサイドカーとして起動し、WKWebView の窓で `127.0.0.1:8790` を表示、⌘Q でサーバごと終了。`--plain` にすると殻なし(`mac/launcher.sh` でサーバ起動→ブラウザ)の仮 .app になる。

データは `~/Library/Application Support/FluentGallery/`(`store/` 原本・サイドカー・索引、`engine/models/` 自動DLモデル、`fluent_gallery.log`)。`FG_DATA` / `FG_PORT` 環境変数で変更可。UI は バンドル内 `Resources/web/` をそこへリンクして読む。

顔ID無効ビルドでは `/api/faces*` が存在せず(404)、収集の顔ゲートは素通し、UIは `/api/caps` を見て顔IDの操作を隠す。回帰テストも顔ID項目を自動でskipする。

### 依存

- **必須**: Rust, Chrome(回帰テスト用), Node.js(回帰テスト用)
- **任意**: ollama(内蔵VLM `qwen2.5vl:7b`)、ml-hub(マスク生成)、yt-dlp + ffmpeg(動画/SNS取り込み。Mac は `brew install yt-dlp ffmpeg`、`~/.local/bin` と `/opt/homebrew/bin` を探す)、OpenRouter/Anthropic/xAI/Pexels/Pixabayの各APIキー
- キーは `~/ml-hub/config/settings.json` に置く(`openrouter_api_key`, `anthropic_api_key`, `gallery_judge_model` など)

> **内蔵VLMはVRAMに8GBの空きが必要。** 足りないとollamaがCPUへ部分オフロードし、CPUを食い尽くしてUIまで重くなる。サイドバーのVRAMメーターで空きを確認できる。

---

## 設計の芯

```
fluent_gallery (Rust, axum, 単一バイナリ, :8790)
├── web/index.html   UI(単一ファイル・ビルド不要・no-store配信=リロードで反映)
├── store/           データの正本
│   ├── images/ab/<sha1>.<ext>   原本(content-addressed)
│   ├── meta/ab/<sha1>.json      サイドカー=正本(来歴/VLM属性/edits/face_ids)
│   ├── thumbs/                  360サムネ + .p.jpg(1080) + .m.jpg(120 micro)
│   ├── datasets/<name>/         払い出し
│   └── index.sqlite             検索索引(サイドカーからいつでも再構築可)
└── engine/models/   内蔵AIモデル(Qwen3-4B GGUF / CLIP ONNX / YOLO-seg ONNX)
```

**原則**

1. **サイドカーJSONが正本、SQLiteは使い捨ての索引**(`POST /api/rebuild` でいつでも作り直せる)
2. **WHERE句に使う値だけDBへ。** 太いBLOB(埋め込み等)は絶対にimagesテーブルに入れない(入れて一覧が30倍遅くなった実績あり → `embs`/`img_faces` に分離)
3. **重い処理は全部ジョブ+進捗。** 人を待たせない。閲覧中は裏方AIが道を譲る
4. **原本は不変。** 編集は履歴スタック、削除は30日ゴミ箱、キャッシュは全て再生成可能
5. **表示の書き手は1人。** ライトボックスの画像src/サイズを触れるのは `lbView` だけ(複数箇所が触って壊し合った反省)

詳細は `docs/design.md`(正本)、`docs/ui-v3.md`(UI憲法)、`docs/face-id-design.md`、`docs/engine-pr.md`。

---

## 操作(キーボード)

| キー | グリッド | ライトボックス |
|---|---|---|
| `矢印` | カーソル移動 | 前/次の画像 |
| `Shift+矢印` | 範囲選択 | — |
| `Space` | 開く(クイックルック) | 戻る |
| `X` / `Enter` | 選択トグル | 選択に加える |
| `F` | 全画面 | — |
| `Delete` | 選択を削除(30日戻せる) | この画像を削除 |
| `E` / `S` / `O` | — | 編集 / お気に入り / 取得元を開く |
| `⌘Z` | 削除を戻す | 編集を1手戻す |

---

## 開発ルール

**デプロイは `bash deploy.sh` 一択。** ビルド → 再起動 → `tests/ui_regression.js` の11項目が全部通って初めて完了する。UIだけの変更でも回帰テストを流してから「直った」と言うこと。

```bash
node tests/ui_regression.js   # 単体実行(サーバ稼働中に)
```

検査項目: フォルダ切替の追い越し / ライトボックス表示 / 送りのサイズ一貫性 / クロップ / ⭐トグル / 顔IDパネル / 押しっぱなし送り / キーボード操作 / 連続削除 / JSエラー。テスト画像は `tests/fixtures/` を `_uitest` ソースへ収蔵し、最後に自動で掃除する(実データには触らない)。

新しいUI操作を足したら、回帰テストに検査を1本足してからコミットする。

---

## API(抜粋)

```
GET  /api/images?limit&offset&source&q&tag&origin&...   一覧/検索(qはキャプションFTS+タグ)
GET  /api/facets                                        絞り込み候補と件数
POST /api/ingest {path,source,move}                     収蔵(ジョブ)
GET  /api/samples / POST /api/samples/{id}?n=1000        権利クリアなサンプル取得(CC0/PD/CC BY: Commons, Met, CMA, ARTIC, NASA, Wellcome, SMK, COCO(CC BY系のみ), Open Images 顔/実写)
                                                          n は最大2万。取った物は源ごとの台帳に残り、次回は続きから(押すほど溜まる)
POST /api/ingest/stop                                     取り込み/まとめ取りを途中で止める(取った分は残る)
POST /api/ingest/url {url,source?,max?}                  指定ページ(または画像URL)の画像を取り込む。YouTube/X等の動画・SNSは yt-dlp でコマ取り込み(要 yt-dlp+ffmpeg、store版は拒否)、内部ネットワーク拒否
POST /api/crawl  {album,n,minutes}                      AIフォルダの収集を開始
POST /api/enrich {backend,n}                            VLM属性付け
POST /api/faces/enroll {album,person,shas,point}        顔IDの人物登録(pointで顔を指定)
POST /api/faces/detect {sha1}                           顔位置+台帳との照合結果
POST /api/faces/scan {album}                            既存画像へ遡及タグ付け
PUT  /api/edits/{sha1} {action,edit}                    非破壊編集(push/pop/clear)
POST /api/seg {album|shas,prompt}                       自動マスク
POST /api/datasets {name,shas,folder}                   データセット払い出し
POST /api/trash {shas} / /api/trash/restore             削除(30日)/復元
GET  /dl/{sha1}/{fname}                                 原本をファイルとしてダウンロード
POST /api/export {shas,name?} → GET /api/export/{id} → GET /export/{id}/{name}.zip
                                                        zip書き出し(画像+meta/サイドカー+manifest、無圧縮・zip64・ストリーム配信)
GET  /api/images/shas?<同じ条件>                         条件に合う全 sha1(上限なし・一括書き出し用)
POST /api/upload (file=*.zip)                           zip取り込み。meta/<sha1>.json があれば出典/ライセンス/編集履歴ごと復元
GET  /api/activity                                      AI稼働状況+CPU/GPU/VRAM/RAM
```

---

## 罠(踏んだもの)

- **ollamaの部分オフロード**: VRAM不足時にVLMがCPUへ落ちて16コアを食う。診断は `curl 127.0.0.1:11434/api/ps` の `size_vram`/`size`、治療は `keep_alive:0` でアンロード→空きがある時に再ロード
- **スキーマ変更後の黙殺**: 古い `index.sqlite` へのINSERTが黙って失敗する → `rm index.sqlite*` → `POST /api/rebuild`
- **削除とAI処理の競合**: 削除直後に裏方が書き戻して「原本の無い幽霊」が復活した → `save_meta` に原本存在ガード
- **ビルド**: `BINDGEN_EXTRA_CLANG_ARGS` と `RUSTFLAGS` が必須(`build.sh` に封じ込め済み)
- **プロセス停止**: `pkill -f` は自分のシェルごと殺す。`pgrep -x fluent_gallery` でPID指定

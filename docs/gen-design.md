# fluent_gallery — AI生成フォルダ / LoRA棚 / 精製 設計書 v1(2026-09-04)

2026-09-04 夜のユーザー指示を集約したゼロベース設計。旧 atelier(FastAPI 版、参照 zip
`fluent_atelier-reference-2026-09-04.zip`)の「実装のやり方」だけ引き取り、器は gallery の
AIフォルダ(収集)と同型に揃える。[design.md](design.md) の M3「genvar を engine キューに内製化」
と M6「LoRA 学習」をこの文書で置き換える。

## 0. 北極星(ユーザーの言葉の集約)

> gallery は NN ベースの AI を作るときの **前処理の器**。
> 棚(データセット)を AI に満たさせ(収集+**生成**)、**最後に棚全体へ「精製」をかけて**学習可能データにする。
> モデル学習(LoRA を焼く等)は gallery ではやらない。繋がる別アプリに渡す。

```
入口(収集 / 取り込み / ★生成)  →  フォルダ  →  棚(データセット)  →  ★精製  →  学習(別アプリ)
                                                      ↑                                    │
                                                      └──────── LoRA棚 ←── 焼けたLoRA ──────┘
```

- **まず作るのは「棚の素材の自動生成」**(atelier の genraw/genvar に当たる部分)。
- 生成は収集と同じ「AIフォルダ」の器で動く: 条件を書いて ▶、♻で目標枚数まで勝手に増える、目利きがゴミを落とす、コストが見える。
- 内蔵 GPU(Mac=Metal / Linux=CUDA)を第一候補、外部 GPU(rtx4090/farm1)とクラウド API は「同じ契約の別プロバイダ」。
- LoRA 棚は gallery に持つ(生成が使う資産)。Mac/CUDA どちらでも同じ LoRA ファイルが効く構成にする。
- 指示は柔軟に: 参照画像・参照フォルダ・マテリアル・プリセット・色合いを 1 本の文の中に混ぜられる。

## 0.1 言葉と器の見直し — 「データセットが主語」(2026-09-04 追加指示、要ユーザー判断)

> やりたいのは **モデル生成のデータセットを作ること**。速く快適に。ゴミデータをいかに速く消して、
> クオリティチェックして、使う。「フォルダ」という表記もこの辺も考え直したい。

いまの IA は「フォルダ(AI/スマート)」と「出荷(データセット)」の 2 本の木で、作りたい物(データセット)が
最後にしか出てこない。作業の主語をデータセットに寄せ、フォルダという言葉をやめる案:

```
データセット「柴犬」                       ← 作りたい物が最初から主語。ネスト/棚分けは今のグループ機構のまま
 ├ 集める     収集(Web) / 生成(AI) / 取り込み(ファイル・URL・動画)   ← 今の「AIフォルダ」= データセットの中の「集め方」1 件
 ├ 仕分け     新着の受け入れ待ち(ゴミを最速で消す場所)              ← 新規: 人が触るのはここだけ
 ├ 本採用     生き残った物(=学習に使う母集団)
 ├ 精製       掃除→正規化→注釈→均し(§7)を掛けて版を作る
 └ 出荷       v1, v2, …(不変スナップショット、学習ツールが読む)
```

- **集め方は全部同じ入口**(ユーザー列挙: クロール / 生成 / ネットから落としたコンテンツ / 自分のカメラライブラリ)。
  どの入口も「同じサイドカー(source/origin/rights/来歴)を書いて、同じゲートを通り、同じ仕分けに落ちる」だけで、
  データセットとしてまとまる。入口ごとに違うのは rights の既定と来歴の中身だけ:

  | 集め方 | 実体(既存/新規) | rights 既定 | 来歴 |
  |---|---|---|---|
  | 収集(Web) | crawl.rs(既存) | unknown / clean(権利クリーン ON) | クエリ・URL・ライセンス |
  | 生成(AI) | gen.rs(本設計) | generated:<モデルのライセンス> | prompt・seed・参照 sha・LoRA |
  | ネットから落とした物 | urlimport.rs(既存: URL/yt-dlp/動画フレーム)+ D&D の zip/フォルダ | unknown(ユーザーが後で付ける) | URL・取得日時 |
  | 自分のカメラライブラリ | **新規**: Mac の写真.app(originals を読む=フルディスクアクセス、または書き出しフォルダの監視)/ iPhone は写真.app 経由 | own(商用可) | 撮影日時・機種・元の albumId |
  | サンプル(CC0/PD) | samples.rs(既存) | cc0 / pd | 出典 |
- 語彙: **フォルダ → データセット**、**AIフォルダ → 集め方(収集/生成)**、出荷 → 版(v1, v2)。サイドバーの 2 本の木は
  「データセット」1 本に畳み、各データセット行の中に 集め方/仕分け/本採用/版 が出る。
- **仕分け(トリアージ)が速さの本体**: 機械が先に並べ替える(怪しい順: 目利き低スコア / 近重複クラスタ / CLIP 外れ値 / 小さい・ぼけ)、
  人は 1 キーで 採用(→) / ゴミ(←) / 保留(↓)、自動で次へ。理由チップ(「近重複 5 枚」「顔なし」「透かし」)を画像の上に。
  近重複はクラスタ単位で「ベスト 1 枚残す」1 キー。全部 undo 可(v3 憲法 5 条)。目標は **1 枚 1 秒**。
- **機械ゲートは仕分け前に働く**(収集/生成のゲート=既存)。人が見るのは「機械が迷った物」だけになるよう、確信度の高い合否は自動で通す/捨てる
  (捨ては .trash 経由、即断で消さない)。
- 「使う」= 版を出荷して学習アプリへ。版には精製レシピとゲート閾値が記録され、再現できる。
- v3 憲法の「フォルダが主語」は「**データセットが主語**」に読み替える(空で生まれる/検索は別レイヤ/dialog 禁止/SVG は不変)。
- 実装は名前と木の畳み込みが中心で、API(albums/datasets)は当面そのまま。UI の文言・木の構造・仕分けモードを G1.5 として入れる。

## 1. atelier から引き取るもの / 捨てるもの

参照 zip で見たもの(生成に関わる部分): `pipeline.py` の `genraw`(テーマ→Claude が写実プロンプト 24 本→t2i 量産、
LoRA 装着は FLUX=直接 / Qwen=素体→Edit 教師で着せ替え 2 段)と `genvar`(参考画像×日本語指示→Claude が英語の
1 編集指示に翻訳→Qwen-Image-Edit で per_ref 枚の変種→fluent_library へ自動収蔵)、OOM で落ちても揃うまで再開する
stall ループ、`server.py` の薄い受付+nohup 別プロセス+ログ tail+ファイル式 GPU ロック、`remote.py` の艦隊
(nvidia-smi/ssh で空きワーカー自動選択+出張実行)、`lora.html` の LoRA 棚(Civitai/HF URL 取り込み、作例カード、
トリガー語、親モデル検問、2 枚重ね)。`gen_raw.py` / `gen_variants.py` / `gen_pairs.py` 本体は zip に無い
(qwen-anime 側、torch/diffusers+bitsandbytes、INT4 常駐)。rtx4090 では :8772 の atelier がまだ生きていて、
LoRA 棚に flux/qwen 系が数本、genvar ジョブが OOM 再開ループを回した痕跡がある。

| 引き取る(発想) | gallery での形 |
|---|---|
| テーマ→LLM が多様なプロンプト N 本→量産 | 生成フォルダの「計画」段(内蔵 LLM が既定、Claude はブースト) |
| 参考画像×言語指示→編集モデルで変種 | レシピの `@画像` / `@フォルダ` 部品(編集モデルの参照入力) |
| 揃うまで再開・進捗ゼロなら省メモリで再試行 | 生成ループの動作リミット+連続失敗で自動⏸(収集と同じ) |
| 薄い受付+別プロセス+ログ | sd-server 子プロセス(vlm.rs の llama-server と同型)。gallery 本体は GPU を触らない |
| 空きワーカー自動選択 | プロバイダ優先リスト(内蔵→リモート sd-server→ComfyUI→API)。ssh/nvidia-smi 方式は捨てる |
| LoRA 棚(URL 取り込み・作例・トリガー・親モデル検問) | `store/lora/` + サイドカー json + 棚 UI(v3 憲法で移植) |
| 完成後ライブラリへ自動収蔵 | 生成は最初から `source="gen:<folder>"` で収蔵(外部連携ではなく本体機能) |

捨てる: FastAPI/Python 常駐、styles.yaml(様式レジストリ)、teacher/train/export/deploy(蒸留=別アプリ)、
Claude 必須のプロンプト設計(コスト階段に)、nunchaku/bitsandbytes INT4 依存、ComfyUI/ollama の VRAM 覗き見。

## 2. エンジン選定(2026-09-04 調査)

「内蔵で、Mac(Metal)と Linux(CUDA)の両方で、LoRA と参照画像編集が動く」を満たす候補:

| 候補 | ライセンス | Mac | CUDA | LoRA 推論 | 参照編集 | 形 | 判定 |
|---|---|---|---|---|---|---|---|
| **stable-diffusion.cpp**(leejet, ggml) | MIT | Metal ○(公式 arm64 バイナリ) | ○(自前ビルド `-DSD_CUDA=ON`。公式は Linux CPU/Vulkan/ROCm、Win CUDA) | ○ `<lora:name:scale>` | ○ `-r` 参照画像(klein/Qwen-Edit/Kontext) | 単一バイナリ `sd-cli` / `sd-server`(OpenAI 互換+非同期ジョブ API) | **採用(内蔵)** |
| mflux(MLX) | Apache-2.0 | ◎ | ✗ | ○ | ○ | Python venv、`mflux-train` で LoRA 学習も可 | Mac 学習側の候補(別アプリ) |
| diffusers/torch(atelier 現行) | — | △ MPS(遅い・大食い) | ◎ | ○ | ○ | Python、モデル毎に実装差 | CUDA ワーカー/学習(別アプリ) |
| ComfyUI(farm1 :8188 で稼働中 0.34.0) | GPL-3 | ○ | ◎ | ○ | ○ | HTTP `/prompt` + ワークフロー JSON | 外部 GPU プロバイダ |
| クラウド API(OpenRouter 統一 Image API: Nano Banana 2 / gpt-image-2 / FLUX.2 pro / Seedream) | 従量 | — | — | ✗ | ○(多参照) | HTTPS | GPU 無し環境・高精度の保険 |

**sd.cpp を内蔵に選ぶ理由**: llama.cpp と同じ ggml 系で、gallery が既にやっている「公式リリースの llama-server を
Resources に同梱して子プロセスで持つ」(vlm.rs)がそのまま二つ目の子プロセスになる。Python が要らないので
.app 同梱と notarize が楽。GGUF 量子化で Mac のメモリ帯域に優しい。**テキストエンコーダを内蔵 LLM と共有できる**
(Z-Image は Qwen3-4B-Instruct-2507、llm.rs が既に DL 済みの GGUF そのもの。klein-4B は Qwen3-4B 無印)。

### 2.1 既定モデル(全部 Apache-2.0 = ストア版でも同梱可)

| 役 | モデル | 用途 | 参照 | ファイル(GGUF) |
|---|---|---|---|---|
| 既定(t2i+編集) | **FLUX.2 klein 4B**(BFL, Apache-2.0, 4 steps, cfg 1) | 文だけの生成も、参照画像ありの変種も 1 モデルで | 多参照 `-r` | `flux-2-klein-4b-Q8_0.gguf` 4.3GB + `flux2-vae.safetensors` 336MB + `Qwen3-4B-Q4_K_M.gguf` 2.5GB |
| 別の絵柄・文字描画 | **Z-Image Turbo**(Tongyi, Apache-2.0, 8 steps, 6B。本機の実測では klein より遅い §9) | 日英の文字描画、klein と違う画風が欲しい時 | ✗(t2i のみ) | `z_image_turbo-Q8_0.gguf` 6.6GB + FLUX.1 `ae.safetensors` 335MB + 内蔵 LLM の `Qwen3-4B-Instruct-2507-Q4_K_M.gguf`(共有・追加 DL なし) |
| 編集の精度 | Qwen-Image-Edit 2509(Apache-2.0, 20B) | 構図保持の着せ替え(atelier の教師) | 多参照 | 重い(TE=Qwen2.5-VL-7B)。Mac 256GB なら可、CUDA 24GB は量子化必須 |
| 非同梱 | FLUX.2 klein 9B / FLUX.1 dev / Kontext dev | 非商用ライセンス | | ユーザー持ち込みなら可(LoRA と同じ扱い) |

klein 4B は「参照なし=t2i」「参照あり=編集」を同じ重みで賄えるので、生成フォルダの既定は klein 一本。
Z-Image Turbo は量産速度が要るとき(棚の素材 1,000 枚級)に切り替える。

### 2.2 実測(M3 Ultra 256GB、sd.cpp master-841-6b3edaa、Metal)

スパイク: `scratchpad/spike/spike.sh`(バイナリ DL→klein t2i ×2→参照編集→LoRA→sd-server 常駐 3 連→Z-Image)。
結果はこの節に追記する(§9 参照)。

## 3. 生成フォルダ(まず「棚の素材の自動生成」)

収集フォルダとの差は **入口が検索ではなく生成** というだけ。器(アルバム+goal+agent+動作リミット+♻)は同じ。

```
▶ / ♻
  │
  ├ 1. 計画(plan)      レシピ(文+部品) → 内蔵 LLM が「英語の具体プロンプト N 本」+ 参照の束ね方 を出す
  │                    (多様性軸: 被写体の違い/構図/視点/光/背景/季節。atelier genraw の 24 本方式)
  │                    ブースト ON なら Claude。台帳に prompt を残し、次回は未使用分から
  ├ 2. 生成(generate)  プロバイダに 1 枚ずつ依頼(内蔵 sd-server の非同期ジョブ)。参照は sha→原本を b64 で渡す
  ├ 3. 門前払い(gate)  収集と同じ三段: pHash 近重複(既存+never_again) → CLIP 多様性(直近採用と近すぎれば却下=モード崩壊防止)
  │                    → 内蔵 VLM 目利き(goal の意味で合否+破綻検出: 指 6 本/文字化け/構図崩れ) → 顔ID(参照人物がいれば本人)
  ├ 4. 収蔵(ingest)    source="gen:<folder>"(infer_origin が synthetic に倒す)、サイドカー gen 来歴+cost+rights
  └ 5. 目標枚数まで / 時間 / 連続失敗 / 予算 で停止。♻ は autopilot の見回りに相乗り(kind=gen も同じループで補充)
```

- **VLM 無しでも走る**(収集と違う点): 生成物は権利ゴミがなく、破綻検出は「あれば良い」なので、内蔵 VLM 不在なら
  pHash+CLIP だけで収蔵し `gate:"none"` を残す。あとから自動エンリッチが属性を付ける。
- **参照フォルダの束ね方**: 1 枚生成するごとにフォルダから k 枚(既定 1)を抽出。その画像の VLM caption を計画 LLM に
  `[REF1: 柴犬、屋外、走っている]` の形で見せてプロンプトの主語を合わせる(参照とプロンプトが喧嘩すると編集モデルが壊れる)。
- **枚数の考え方**: 目標(target)は棚に欲しい枚数。1 回の ▶ は batch(既定 30)。per_ref(参照 1 枚あたりの変種数、既定 4)は
  参照ありのときだけ。
- **失敗の扱い**: プロバイダ失敗(OOM/タイムアウト)は連続 8 で ⏸(収集と同じ)。内蔵で OOM が続く場合は解像度を一段落とす
  (1024→768)のを 1 回だけ自動で試す(atelier の「省 VRAM で再開」の置き換え)。

### 3.1 UI(v3 憲法: フォルダが主語 / 空で生まれる / dialog 禁止 / SVG)

- ＋ → フォルダ種別に **生成** を追加(AI 収集 / スマート / 生成)。空状態の文言「作りたいものを書いて ▶ を押すと、ここに生まれ始めます」。
- ルールパネル(ヘッダ)は収集と同じ骨: 目標文(=レシピ本文)、目標枚数、1 回の枚数、予算、AI 配役(**生成: 内蔵 klein / Z-Image / リモート / ComfyUI / API**、
  **計画: 内蔵 LLM / Claude**、**目利き: 内蔵 VLM / OFF**)、サイズ、LoRA(棚から選ぶ、強さ)、♻。
- 実況: 生成中は「宙から降ってくる」drop(収集の実況 drop を流用)。recent ストリップに合否と理由。
- 右クリック(サムネ/選択): 「**似た画像を作る**」= この画像を `@画像` にした生成フォルダを新規作成(design.md 魔法 3b)。
  データセットの「AI で増やす」= そのデータセットを `@フォルダ` にした生成フォルダ(旧 expandDataset の置き換え)。
- 稼働ボードに「生成」ワーカー(内蔵 sd-server の起動/モデル DL 進捗/枚数/秒/枚/エラー)。

### 3.2 一目で分かる(2026-09-04 指摘: 「金を使っているのか」「今クロール中なのか」「収集と生成の見分け」)

いまのサイドバー行は 収集も生成も同じ `bot` アイコン、実行中は 11px の ▶、有料かどうかは「使った後」にしか出ない。
直すのは 4 点(v3 憲法 6 条: ストロークSVG、絵文字禁止):

1. **種別アイコンを分ける**(行の左端 15px、色相も分ける)
   - 収集 = `globe`(Web から集める)青系 / **生成 = 新規 `wand`(杖)または `flask`** 紫系 / スマート = `smart` 灰。`bot` は廃止(どちらにも見える)。
   - 種別は `kind`(crawl / gen / smart)から決める(goal の有無で推定しない)。
2. **状態は種別アイコンの右下の点で常に出す**(走っていない時も「何もしていない」が分かる)
   - 動作中 = 緑の点が脈打つ(CSS 1.2s)+行の背景をうっすら点灯+数字欄が `12/30`(今回の進み)に切り替わる。hover で今のクエリ/プロンプト
   - ♻待機 = 薄い `loop` / ⏸(予算切れ・連続失敗・手動停止)= 琥珀の一時停止 / 🔴エラー = 赤の点、title に最後のエラー
3. **お金は「設定」で見せる**(使った後でなく、使う設定なら走っていなくても見える)
   - `coin` を名前の右に 3 状態で: 無し = 無料設定(内蔵だけ) / 輪郭だけ = 有料の役(ブースト/API 生成)が ON だが今は消費なし /
     塗り(琥珀)+`$0.12` = 今の run で消費中(課金のたびに一瞬光る)。累計はいまの coin クリック切替(トークン/$/¥)を維持
   - フォルダのルールパネルに **配役ライン**を常設: 「計画: 内蔵LLM(無料) · 目利き: OpenRouter Qwen72B(有料 ≈0.05円/枚) · 生成: 内蔵 klein(無料)」。
     有料の役だけ琥珀色。これが「このフォルダは金を使うのか」の正解表示
4. **「いま AI が何をしているか」を 1 行で常設**(サイドバー最上段、何か動いている時だけ現れる)
   - 例: 「● 収集中 柴犬 12/30 · 内蔵AI(無料)」「● 生成中 shiba_gen 4/30 · 9.8秒/枚」「● 属性付け 残120」。クリックでそのフォルダへ
   - 何も動いていない時は「待機中 · 次の見回り 12分後」(autopilot の next_at)。Tauri 版は Dock バッジに同じ点を出す(後回し)
   - 実体は既存の `/api/activity`(crawl/enrich/seg/autopilot/workers)+ 新 `gen`。UI は 2 秒ポーリングのまま

5. **動作中の行は綺麗に動かし、ホバーで「どの AI が何をしているか」を出す**(2026-09-04 追加指示)
   - アニメーション: 種別アイコンの外周に細いリングが進捗(12/30)ぶん埋まり、行の背景を左から右へゆっくり流れる薄い光(shimmer、6 秒周期)。
     点滅や派手な動きは禁物(閲覧の邪魔)。`prefers-reduced-motion` ではリングだけ。収蔵が 1 枚増えるたびにリングが一瞬明るくなる
   - ホバー(0.4 秒)で **配役カード**(title 属性でなく自前ポップオーバー、v3 憲法どおり dialog ではない):
     ```
     ● 収集中 柴犬  12/30 · 4分12秒 · 通過率 38%
     計画    内蔵 LLM  Qwen3-4B          [内蔵]
     目利き  OpenRouter Qwen2.5-VL 72B   [外部・有料]  $0.004 今回
     生成    sd-server klein 4B (farm1)  [外部・無料]  27秒/枚
     いま:  「shiba inu running park photo」を検査中(3/8 枚目)
     ```
     役ごとに **内蔵 / 外部・無料 / 外部・有料** のタグ(チップ形のアイコン=内蔵、雲=外部、雲+coin=外部・有料。色は 灰 / 青 / 琥珀)。
     どの機械で動いているか(このMac / farm1 / OpenRouter)も名前で出す
   - UI が推定しない: `/api/activity` の各ジョブに `roles:[{role, model, where:"builtin"|"remote"|"api", host, paid, unit_cost, spent}]` と
     `now:{step, detail, i, n}` を載せ、UI はそれを描くだけ(内蔵/外部の判定はサーバが知っている事実)

行の tooltip には 種別 / 状態 / 最終稼働 / 24h の収穫 / 通過率 / 累計消費 を並べる(design.md「エージェントの有効性が見えること」)。

### 3.3 速さは金で買う — ブースト = 有料で速く、無料 = ローカルでゆっくり(2026-09-04 指示)

収集の 💰ブースト(クエリ生成と目利きを外部 AI に格上げ+並列)を、生成にもそのまま広げる。スイッチは 1 つ、役ごとの中身は表で決める。
ブースト中のデータセット行には §3.2 の coin(塗り)が必ず付く。

| 役 | 無料(ブースト OFF) | ブースト ON |
|---|---|---|
| 計画(レシピ→プロンプト N 本) | 内蔵 LLM Qwen3-4B | Claude(brief 正規化も) |
| 生成 | 内蔵 sd-server(klein 4B、直列、1024² ≒ 30 秒/枚 → 300 枚で 1〜2 時間、夜間 ♻ 向き) | クラウド Image API(OpenRouter: Nano Banana 2 / gpt-image-2 / FLUX.2 pro)を 4〜8 並列(数秒/枚 → 300 枚で 10〜20 分)。リモート sd-server(4090/farm1)が居ればそちらを先に使う(無料のまま速い) |
| 目利き | 内蔵 VLM(直列) | OpenRouter Qwen2.5-VL 72B / Claude(並列) |
| 予算 | — | データセット累計の上限(既存 budget_usd)。超えたら無料に落として続行、行に ⏸予算 |

- 既定は無料。「今すぐ欲しい」時だけブーストを入れる。ブースト中でも収蔵・ゲート・来歴は同じなので、後から混ぜても区別は `cost.by` で付く。
- **マテリアルも自動で集まる**: マテリアル棚(§4)は普通のデータセット(集め方=収集「木目 テクスチャ CC0」/ 生成「seamless tile of …」)で、
  ゲートに「タイル性(端の連続)」と「単一素材か」を足すだけ。集まったマテリアルはそのまま `@マテリアル` 部品として他のデータセットの生成に使える。
  プリセット/色も同じ発想で「レシピから保存」または「この画像の色から抽出」で増える。

## 4. 指示の柔軟化 — 「レシピ = 1 本の文 + 部品チップ」

ユーザー質問「参照する画像やフォルダ、マテリアル、プリセット、色合いを指定したい。いい方法は?」への答え。

**方式: メンション付きの 1 本の文。** 入力欄は自由文 1 つ。`@` を打つ(または画像をドラッグして落とす)と部品が
本文の中にインラインのチップとして刺さる。フォームを増やさない(v3 憲法 4 条: ユーザー語彙だけ)。

```
@[柴犬](フォルダ) の犬が、夕方の公園を走っている。画風は @[水彩](プリセット)、
色合いは @[夕焼け](色)、地面は @[濡れたアスファルト](マテリアル)。人は写さない。
```

部品は 6 種類。内部表現(recipe)は文+部品配列で、部品がどう生成に効くかは種類で決まる:

| 部品 | 挿し方 | 実体 | 生成への効き方 |
|---|---|---|---|
| **画像** | サムネを落とす / `@` で検索 | sha1 | 編集モデルの参照入力(`-r`)。klein/Qwen-Edit/API(nano banana)は多参照可。Z-Image は不可→計画 LLM が caption を文に溶かす(劣化モード、UI に注記) |
| **フォルダ** | `@` でフォルダ名 | album + pick(random/round-robin, k) | 1 枚生成ごとに k 枚抽出して参照に。抽出した画像の caption/tags を計画 LLM に渡す(主語合わせ) |
| **マテリアル** | `@` でマテリアル名(棚: `store/materials/`) | 質感画像(テクスチャ/素材写真)+説明文 | 参照画像として「この材質で」の 1 枚に。テクスチャは事前にタイル状 512px に正規化 |
| **プリセット** | `@` でプリセット名 | 保存済みレシピ断片 `{prompt_add, negative, lora:[{file,scale}], size, steps, palette?}` | 文に句を足す+LoRA を装着+設定を上書き。fluent_scene の様式チップと同じ棚に並べる(ホバー染めで予告) |
| **色合い** | `@` で色名 / HEX / 「この画像の色」 | パレット `{name, colors:[hex], mood}` | 二段構え: ①計画 LLM が色を言葉に("warm amber sunset tones, teal shadows") ②生成後に非破壊 edits(WB/tint/彩度)で締める(決定的・やり直し可) |
| **LoRA** | `@` で棚の LoRA | file + scale | `<lora:file:scale>`。親モデル不一致は挿す時点で弾く(棚の base が生成モデルと一致する物だけ候補に出す) |

- **コンパイル**: 計画 LLM への入力は「文(部品はプレースホルダ `[REF1]`… に置換)+ 部品の説明(種類/caption/色名/プリセット句)」。
  出力は JSON `{prompts:[{text, refs:[...], negative}], notes}`。日本語→英語はここで一緒に済む。内蔵 4B が弱い所
  (固有名詞の捏造、否定の取り違え)は brief 正規化(crawl.rs の ensure_brief)を流用し、ブースト時は Claude。
- **会話で直す**: パネルの「直す」欄に「もっと夕方っぽく」「人を減らして」→ 計画 LLM がレシピ差分を返し、次のバッチから効く
  (atelier の `/api/refine` と同じ発想。レシピは自動保存、履歴 3 世代を台帳に残して戻せる)。
- **モデル別の対応表を UI に出す**: 部品が今の生成モデルで効かない場合(Z-Image に画像参照)はチップに ⚠ と代替(文に溶かす/klein に切替)を出す。
  黙って無視しない(design.md「死んでるのに動いてるフリが一番ダメ」)。
- **プリセット/マテリアル/色は棚として保存できる**: サイドバー「棚」に LoRA と並べる(§5)。プリセットは「この生成フォルダの
  レシピを保存」から作れる=レシピ複製の再利用(AIフォルダの ⧉ と同じ思想)。

## 5. LoRA 棚(+プリセット/マテリアル/色の棚)

「Mac/CUDA 両方で動くか」→ **推論は YES**。sd.cpp は Metal でも CUDA でも同じ safetensors LoRA を `<lora:name:scale>` で読む
(量子化ベースのときは at_runtime モードで精度を保つ、自動選択)。制約は「LoRA は親モデルごと」(klein-4B 用 / Z-Image 用 /
Qwen-Image 用 / FLUX.1 用は互換なし。9B 用は 4B に載らない)。**学習は別アプリ**(Mac=mflux-train(MLX)、CUDA=musubi-tuner/ai-toolkit。
実装が MLX と torch で別物なので両対応は別アプリ側で吸収し、gallery は「データセットを渡す/焼けた LoRA を受け取る」口だけ持つ)。

- 置き場: `store/lora/<file>.safetensors` + `<file>.json`
  `{name, base:"flux2-klein-4b"|"z-image"|"qwen-image"|"flux1", triggers:[], source(url), license, description, preview:[sha1…], imported, trained_from:{dataset, app}}`
- 取り込み: URL(Civitai/HF、atelier の `_resolve_lora_url` と同じ親モデル検問と作例 DL) / ローカルファイル(D&D) / 別アプリからの `POST /api/lora/import`。
- **試し描き(probe)**: 取り込み直後に固定 4 プロンプト×(なし/×0.6/×1.0/×1.4)を内蔵で描いて棚カードの顔にする(atelier の loracmp)。
  生成物は `source="lora_probe:<name>"` で収蔵(棚から消せば消える再生成可能物)。
- 棚 UI: サイドバーの「出荷」の木の隣に **「棚」の木**(LoRA / プリセット / マテリアル / 色)。木は別物、D&D は木の中だけ(ui-v3 の規則)。
  カードは lora.html の縦カード(作例が顔、親モデル・サイズ・トリガー語・使っているフォルダ)を SVG/自前ポップアップで移植。
  カードの主操作は「このLoRAで生成フォルダを作る」(=レシピに `@LoRA` を刺した新規フォルダ)。
- ストア版: LoRA は同梱しない(ユーザー持ち込み)。非商用ライセンスの LoRA は棚の json に license を残し、UI に表示するだけで止めない。

## 6. プロバイダ抽象(内蔵 / 外部 GPU / API を同じ契約に)

```rust
// src/gen.rs
pub struct GenJob { prompt: String, negative: String, refs: Vec<PathBuf>, lora: Vec<(String, f32)>,
                    w: u32, h: u32, steps: u32, cfg: f32, seed: u64, model: String }
pub struct GenOut { png: Vec<u8>, secs: f32, usd: f64, provider: String, model: String }
pub trait Provider { fn caps(&self) -> Caps /* refs, lora, models */; async fn generate(&self, j: &GenJob) -> Result<GenOut, String>; }
```

| プロバイダ | 実体 | 参照 | LoRA | 選ばれ方 |
|---|---|---|---|---|
| `builtin` | sd-server 子プロセス(Mac: Resources/sd/、Linux: engine/bin/)。`/sdcpp/v1/img_gen` 非同期ジョブ+poll+cancel | ○ | ○(`--lora-model-dir store/lora`) | 既定。モデル/バイナリが揃っていれば自動起動(vlm_wake と同じ) |
| `sdcpp` | 別マシンの sd-server(rtx4090/farm1 で CUDA ビルド)。`FG_GEN_BASE` または config の優先リスト | ○(b64) | ○(そのマシンの lora dir。棚と rsync するか、ジョブに同梱) | 内蔵が無い/遅い/塞がっている時 |
| `comfy` | farm1 :8188。`engine/comfy/{t2i,edit,lora}.json` の雛形に prompt/seed/参照を差し込み `/prompt`→`/history` | ○ | ○ | ユーザーが ComfyUI に入れた任意モデルを使いたい時 |
| `api` | OpenRouter 統一 Image API(`google/gemini-3.1-flash-image` 等)/ OpenAI Images(gpt-image-2) | ○(多参照) | ✗ | GPU 無し環境、または「本気の 1 枚」。💸 は cost サイドカーに実測 |

- 設定は Phase 1.6(engine-pr.md)の `vlm_backends` と同じ **優先リスト** `store/config.json: gen_backends:[…]`。上から順にヘルスが通る物。
  フォルダの AI 配役で明示指定もできる(収集の judge_model と同じ「フォルダ設定 > 既定」)。
- 参照画像・LoRA の受け渡しは **b64 同梱**(seg.rs と同じ薄い結合)。共有ストレージを仮定しない。
- ワーカーの空き判定は sd-server の `/sdcpp/v1/capabilities`(載っているモデル名)とジョブ状態で足りる。nvidia-smi/ssh はやらない。
- 同時実行: 内蔵は 1 本(VLM/LLM と GPU を分け合う。収集中は VLM 目利きが優先=`enrich.user_priority` の流儀)。リモートはワーカー毎に 1 本。

**sd-server の LoRA は `<lora:name:scale>` のプロンプト埋め込みを受け付けず、リクエストの `lora:[{path,multiplier}]` で渡す**(api.md 明記)。sd-cli は `--lora-model-dir` + プロンプト埋め込み。G4 のプロバイダ実装はこの差を吸収する。

### 6.1 Mac / CUDA の配布

| | Mac(.app) | Linux(rtx4090) |
|---|---|---|
| バイナリ | `mac/build_mac.sh` が sd.cpp 公式リリース(macOS arm64 zip、約 50MB)を `Resources/sd/sd-server` に同梱(llama と同じ手順) | `deploy.sh` で `cmake -DSD_CUDA=ON` ビルド → `engine/bin/sd-server`(公式 Linux CUDA バイナリは無い。Vulkan バイナリは保険) |
| 探索順 | `FG_SD_SERVER` → `engine/bin/` → 実行ファイルの隣 → `../Resources/sd/` → `/opt/homebrew/bin` → PATH(vlm.rs `server_bin` と同じ) | 同左 |
| モデル | `engine/models/` に初回 DL(vlm.rs `download` 流用、サイズ一致で完了判定、`.part` 再開)。AI 配役の「取得」ボタン、進捗は `/api/gen/status` | 同左(共有 Qwen3-4B は二重 DL しない) |
| メモリ | klein Q8_0 ≒ 7GB 常駐(統合メモリ)。VLM 3.3GB+LLM 2.5GB と同居可 | 24GB: klein Q8_0+TE Q4 で余裕。Qwen-Image-Edit は Q4 必須。ollama 9.8GB と同居中なので VRAM 譲り合いは `--offload-to-cpu` を予備に |

## 7. 精製(refine) — 最終目的への骨格

「棚に全部精製をかけられる」の精製=棚(データセット)を **学習可能な状態に磨く一連の決定的な処理**。生成はその材料を
増やす手段の一つ。ここでは生成と噛み合う骨格だけ決め、詳細は次の設計書(refine-design.md)で。

- 単位: 棚(データセット)1 つに「精製レシピ」を付けて ▶。結果は **新しいデータセット版**(`<name>@v2`)として出荷、元は不変
  (payout は symlink+manifest なので版を重ねてもディスクは増えない)。
- 工程(順序固定、各工程は on/off と閾値だけ):
  1. **掃除**: 壊れ/極小/透かし/NSFW(VLM 属性)/pHash 近重複クラスタ(ベスト 1 枚残す)/CLIP 外れ値
  2. **意味ゲート**: goal に対する目利き再審(収集時に通した物も含めて同じ基準で揃える)
  3. **正規化**: 被写体中心クロップ(seg/顔検出)、長辺リサイズ、色空間、EXIF 除去、非破壊 edits の焼き込み
  4. **注釈**: caption/tags の再生成と語彙の正規化(内蔵 VLM/LLM)、マスク(seg)、顔 ID タグ、`.txt` キャプション出力
  5. **均し**: クラス/属性の分布を見て足りない側を **生成で補う**(=ここで生成フォルダを自動発火: 参照フォルダ=その棚、指示=足りない条件)
  6. **検品**: 分布レポート(design.md 魔法 5「何が足りない?」)と精製前後の差分
- 生成側で今から守ること(精製が読む契約): サイドカーに `gen` 来歴(§8)を必ず残す、`origin=synthetic` を固定、参照 sha を残す
  (精製で「参照と近すぎる生成物」を落とせる)、rights=`generated:<model license>` を残す。

## 8. データモデル / API

**アルバム(既存 json に追加)**
```json
{"name":"shiba_gen","kind":"gen","folder":"犬","goal":"@[柴犬](フォルダ) の犬が夕方の公園を走っている…",
 "recipe":{"parts":[{"id":"REF1","kind":"folder","album":"shiba","pick":"random","k":1},
                    {"id":"PRE1","kind":"preset","name":"watercolor"},{"id":"PAL1","kind":"palette","name":"sunset"}],
           "model":"flux2-klein-4b","size":"1024x1024","steps":4,"cfg":1.0,"per_ref":4,"negative":"",
           "lora":[{"file":"pencil_v1","scale":0.8}],"diversity":["subject","composition","light","background"]},
 "agent":{"auto":false,"target":300,"batch":30,"budget_usd":3,"provider":"","planner":"","judge":"builtin","spent_usd":0},
 "criteria":{"source":"gen:shiba_gen"}}
```
**サイドカー(生成 1 枚)**
```json
{"source":"gen:shiba_gen","origin":"synthetic","rights":"generated:apache-2.0",
 "gen":{"provider":"builtin","model":"flux-2-klein-4b-Q8_0","prompt":"photorealistic photograph of a shiba inu…",
        "negative":"","seed":4123,"steps":4,"cfg":1.0,"w":1024,"h":1024,"refs":["<sha1>"],"lora":[{"file":"pencil_v1","scale":0.8}],
        "recipe_hash":"…","plan_id":"…","secs":9.8,"gate":"vlm"},
 "cost":{"usd":0,"by":"builtin"}}
```
**台帳** `store/gen_ledger/<album>.json`: `{prompts:[{text, used, ok, ng}], recipe_history:[…3], failures:[…]}`。
索引: `images` に列は足さない(`origin`/`source` で引ける)。gen 来歴はサイドカー正本、必要なら `gen_model` 列だけ後で。

**API(収集と対称)**
```
POST /api/gen              {album, n?, minutes?}        ▶(順番待ちは crawl_queue と同じ 1 本直列)
GET  /api/gen/status                                    GenState(alive, planned, generated, rejected, ingested, secs_per, last, recent, provider, model)
POST /api/gen/stop
POST /api/gen/plan         {album|recipe, n}            計画だけ返す(UI の「どんなプロンプトになるか」プレビュー、MCP)
POST /api/gen/one          {recipe, seed?}              1 枚だけ即時(ライトボックスの「似た画像を作る」試し)
GET  /api/gen/engine       {bin, models:[{id, present, size_mb, downloading}], running, port}
POST /api/gen/pull         {model}                      モデル DL
GET/POST/DELETE /api/lora  /api/lora/import {url|path}  /api/lora/{name}/probe
GET/POST/DELETE /api/presets, /api/materials, /api/palettes    (棚の CRUD、json 1 件=1 ファイル)
```
`/api/genvar` は `POST /api/gen` に吸収(UI の「🌱 指示で量産」「AI で増やす」は生成フォルダ作成に置換)。`/api/activity` に `gen` を足す。
MCP(`mcp/gallery_mcp.py`)に `gen_start / gen_status / gen_plan / lora_list` を追加(AI 1st)。
回帰テスト(`tests/ui_regression.js`)に: 生成フォルダが空で生まれる / ▶ で recent が増える / 部品チップの挿入と削除 / 棚カード表示 の 4 項目。

## 8.1 設定画面(2026-09-04 指示: いま無いので追加する) — ✅ v1 実装 2026-09-05

実装済(src/config.rs, `GET/PATCH /api/settings`, `POST /api/settings/test`, サイドバー最下段の歯車 → 設定ページ): AIの役(目利き既定) / 接続先(gen.base, gen.port, vlm.base) / APIキー 6 種(末尾4桁表示・疎通確認・消す) / モデル一覧と取得 / 外部ツールのパス上書き / 自動運転(周期・お手入れ ON/OFF) / 生成の既定(サイズ・steps) / 保存先とキャッシュ上限 / 情報(版・feature・環境変数の上書き)。`config::set` は既定に無いパスと型違いを拒否し、既定値や空なら項目を消す(未実装の設定を貯めない)。
未実装(表にあるが v1 に無い): お金の全体上限(円/日、実装は集計が要る)、取り込み(写真.app・監視フォルダ)、表示(言語)、権利と安全、モデルの削除、接続先の「優先リスト」(v1 は内蔵 or 1 つの外部 URL)。Pexels は無効キーでも 200 を返すので疎通確認は接続のみ。

いまは API キーが `~/ml-hub/config/settings.json`(Linux の ml-hub の置き場)から読まれ、バックエンドは環境変数(`FG_VLM_BASE` 等)、
それ以外は UI の各所に散っている。Mac 単体アプリでは成り立たないので、**正本を `store/config.json` に一本化**し、設定画面から自動保存で触る。

- 置き場と優先順: 環境変数(開発・一時上書き) > `store/config.json`(正本、UI が書く) > 旧 `~/ml-hub/config/settings.json`(Linux の後方互換、読むだけ)。
  キーは平文で store 内(バックアップ対象外の `store/config.json` に限定し、書き出し/zip には絶対に含めない)。
- 入口: サイドバー最下段の歯車(SVG)→ メインに設定ページ(`loc.type='settings'`、URL ハッシュ復元可)。ネイティブ dialog なし、保存ボタンなし(行ごとに自動保存+保存済みの小さな✓)。
- API: `GET /api/settings`(キーは末尾 4 桁だけ返す)/ `PATCH /api/settings {path, value}` / `POST /api/settings/test {provider}`(疎通確認、既存 `/api/llm/test` と同型)。
  変更は即時反映(ホットリロード: バックエンド優先リスト、予算、周期)。再起動が要るものは行に「再起動後」と出す。

| 節 | 項目 |
|---|---|
| **AI の役(既定)** | 計画 / 目利き / 属性付け / 生成 / 自然言語検索 それぞれの既定(内蔵 or 外部モデル)。データセット側の設定が無い時にここが効く |
| **接続先(優先リスト)** | VLM・生成・LLM のバックエンドを上から順に試す一覧(内蔵 / 別マシンの llama-server・sd-server / ollama / ComfyUI / API)。各行に URL・モデル・ヘルス(●/○)・並列数。追加・並べ替え・無効化 |
| **API キー** | Anthropic / OpenAI / OpenRouter / xAI / Pexels / Pixabay / Civitai(LoRA 取り込み用)。マスク表示、「確認」で疎通 |
| **モデル** | 内蔵モデルの一覧(LLM / VLM / CLIP / 顔ID / 生成 klein・Z-Image・Qwen-Edit): 有無・サイズ・取得/削除・置き場の空き容量。ここが AI 配役の「取得」の本籍 |
| **お金** | 全体の上限(円/日=グローバルブレーキ、design.md の安全装置)、データセット既定の予算、通貨表示(トークン/$/¥)、為替 |
| **自動運転** | ♻ 見回り周期(既定 30 分)、自動エンリッチ/自動マスクの ON/OFF、閲覧中は AI が道を譲る秒数、夜間だけ動かす時間帯 |
| **保存先と容量** | データの置き場(Mac: ~/Library/Application Support/FluentGallery)、キャッシュ上限(preview/render の LRU)、ゴミ箱の保持日数、prune の世代 |
| **取り込み** | 写真.app ライブラリの許可と対象アルバム、監視フォルダ、動画のフレーム間隔、URL 取り込みの禁止ホスト(ストア版は固定) |
| **外部ツール** | yt-dlp / ffmpeg / llama-server / sd-server の検出結果とパス上書き(`FG_LLAMA_SERVER` 等に相当) |
| **表示** | 言語(日/英、M8 の i18n と同時)、サムネ既定サイズ、種別アイコンの色、稼働ボードの表示、実況 drop の ON/OFF |
| **権利と安全** | 権利クリーンの既定、セーフサーチ、生成の NSFW フィルタ(ストア版は固定 ON)、収集の禁止語 |
| **情報** | バージョン、ビルド feature(faceid/store/metal/cuda)、ログの場所、診断情報のコピー |

既存の散らばりの回収: AI 配役の「取得」ボタンはそのまま残し、設定ページの「モデル」節と同じ API を呼ぶ。`gallery_judge_model` 等
ml-hub 由来のキー名は config.json では役の名前(`roles.judge`)に改名し、読み込み時に旧名を写す。

## 9. スパイク結果(M3 Ultra 256GB、macOS 26、sd.cpp master-841-6b3edaa 公式 arm64 バイナリ、Metal)

構成: `flux-2-klein-4b-Q8_0.gguf`(4.3GB)+ `Qwen3-4B-Q4_K_M.gguf`(2.5GB)+ `flux2-vae.safetensors`(336MB)、`--diffusion-fa`、euler、cfg 1.0、4 steps。

| 試験 | 結果 | 備考 |
|---|---|---|
| バイナリ | 公式 zip(50MB: sd-cli / sd-server / libstable-diffusion.dylib)を展開してそのまま動く | quarantine 解除のみ。同梱は llama-server と同じ手順で可 |
| klein t2i 1024²(sd-cli、ロード込み) | 29.3 秒 / 28.9 秒(2 回) | 最大 RSS 10.7GB。絵は写実の柴犬、破綻なし |
| klein 参照編集(`-r` 640×568 の実写→水彩) | 20.4 秒(ロード込み) | 構図保持で水彩化、良好。RSS 12.8GB |
| klein + LoRA(fal の klein-4B 用 safetensors 168MB、`<lora:test_lora:1>`) | 読み込み 136 tensors、`apply lora at runtime`(量子化ベースなので自動で at_runtime)、出力は同 seed の素の生成と別物 | **Metal で LoRA が効く**ことを確認。様式が出なかったのはトリガー語未使用(棚の triggers を必ず prompt に入れる設計の根拠) |
| sd-server 常駐、OpenAI 互換 `/v1/images/generations` | 46〜49 秒/枚 | **罠**: `<sd_cpp_extra_args>` の `cfg_scale` は効かず既定 `txt_cfg 7.0` のまま=2 回走って 2 倍遅い。cfg は `sample_params.guidance.txt_cfg` に入れる |
| sd-server 常駐、ネイティブ `/sdcpp/v1/img_gen`(txt_cfg 1.0, 4 steps, 1024²) | 1 回目 29.4 秒(遅延ロード込み)、**2 回目以降 27.2 / 27.3 秒/枚** | 202 → `poll_url` → `status: queued/generating/completed`、結果は `result.images[].b64_json`。実装はこの口を使う |
| `/sdcpp/v1/capabilities` | `model.name`、`loras[]`(--lora-model-dir の中身)、`defaults.sample_params` が取れる | リモートの空き/載っているモデルの判定はこれで足りる |
| Z-Image Turbo Q8_0(6.6GB)、1024² 8 steps、TE = **gallery 同梱の Qwen3-4B-Instruct-2507**(llm.rs の GGUF そのもの) | 67.8 秒(ロード込み)、RSS 12.0GB、絵は写実で良好 | 素の Qwen3-4B でも 67.1 秒・同等 → **追加 DL なしで 2 モデル目が使える** |

**G2 追試(2026-09-05、sd-cli 方式)**: 参照画像つき 768² 4 steps = 32 秒/枚(途中経過 proj 書き出し込み)、`/api/gen/preview` は生成中 3 秒ごとの確認で毎回 200。柴犬の参照に「短い編集指示(Change the season to winter…, keep the dog exactly the same)」でも「計画 LLM の長い編集指示(Keep the same shiba inu from the reference image and place it running through a snowy forest…)」でも被写体を保って季節だけ変わった → 計画は現行の書き方で可。参照を間違えると(少女の写真+「柴犬」)参照側の上着とポーズが勝つ=参照の効きは強い。

含意: klein 4B は **1024² で 27 秒/枚(常駐)**。棚の素材 300 枚 ≒ 2.3 時間 → 夜間 ♻ 向き。速さが要る時は 768²(概ね半分)かブースト。
Z-Image Turbo は本機では klein より遅い(6B・8 steps)ので「速い t2i」ではなく「文字描画・別の絵柄」の選択肢と位置づけ直す(§2.1 の表を修正)。
メモリは統合 256GB では問題なし。klein Q8_0 と内蔵 VLM(3.3GB)・LLM(2.5GB)は同居できる。
実装メモ: 蒸留モデル(klein / Z-Image Turbo)は `txt_cfg=1.0` 固定、`distilled_guidance` は既定 3.5 のまま。`--diffusion-fa` 必須。

## 10. マイルストーン

- **G1 棚の素材の自動生成(最短)** ✅ 2026-09-05 実装(src/gen.rs、/api/gen/*、UI の「作る」種別、autopilot、build_mac.sh の sd 同梱、回帰テスト 1 項目。Mac 実測: 768² 17 秒/枚、3/3 収蔵): `src/gen.rs`(sd-server 子プロセス+builtin プロバイダ+DL)、生成フォルダ(kind=gen、文だけ、klein)、
  計画(内蔵 LLM)→生成→pHash/CLIP/VLM ゲート→収蔵、`/api/gen/*`、稼働ボード、autopilot 相乗り。build_mac.sh に sd 同梱。
- **G2 参照** ✅ 2026-09-05 実装: recipe.refs(画像=固定 / フォルダ・データセット=毎回 k 枚抽選)、計画 LLM は参照があると「編集指示」を書く、右クリック/ライトボックス「似た画像を作る」(生成フォルダを見ていれば参照に足す)、データセット「AI で増やす」と選択の量産は参照つき生成フォルダを作る、`/api/genvar` は生成フォルダに吸収。**ローカルは sd-cli 方式に変更**(`--preview proj` で途中経過を各ステップ書き出し `/api/gen/preview`、stderr の N/M で進捗、常駐なし。sd-server は別マシン用と sd-cli 無し環境の保険)。モデル台帳 klein / Z-Image Turbo / Qwen-Image-Edit-2509 をフォルダ単位で選択(未取得は▶で自動 DL、設定画面から個別取得)。
- **G3 レシピ部品**: メンション入力 UI、プリセット/マテリアル/色の棚と効き方、会話で直す、モデル別対応表の ⚠。
- **G4 LoRA 棚**: import(URL/ファイル)、親モデル検問、probe、カード UI、生成フォルダからの装着。
- **G5 外部**: 優先リスト config、リモート sd-server(rtx4090 CUDA ビルド)、ComfyUI 雛形、OpenRouter/OpenAI、予算ガード。
- **G7 映像(候補)**: いま映像は入力側だけ(動画→フレーム、URL 取得、フレームへの VLM/CLIP)。sd.cpp は Wan 2.2 / LTX-2 の映像生成に対応しているので、同じ子プロセスで「映像も作る」を後から足せる(映像データセット=フレーム列の量産)。
- **G6 精製 v0**: 棚の版出荷+掃除/正規化/注釈(refine-design.md を先に書く)。均しの「足りない分を生成」はここで生成フォルダと接続。
- 別アプリ(atelier 後継、名称未定): データセット→LoRA 学習(Mac=mflux-train / CUDA=musubi-tuner)→gallery の棚へ import。
  gallery 側の口は G4 の `/api/lora/import` と既存の datasets 出荷だけ。

## 11. atelier の扱い(推奨)

**生成と LoRA 棚は gallery に統合、学習・蒸留・配備は別アプリ。** 理由: 生成物は「棚の素材」なので gallery の収蔵/ゲート/精製の
流れに乗せるのが本筋(atelier が別 HTTP で収蔵していた結合が消える)。LoRA は生成が使う資産で棚 UI も gallery 側にある方が自然。
学習は GPU 常駐時間が長く、環境(MLX/torch/musubi)が重いので gallery の単一バイナリに入れない。両者の契約は
「データセット(symlink+manifest+captions)」と「LoRA(safetensors+json)」の 2 つのファイル形式だけ。

## 12. 未決(ユーザー判断待ち・急がない)

1. 既定モデルを klein 4B 一本にするか、Z-Image Turbo も同梱(+6.6GB)するか(ストア版の容量)。
2. 参照フォルダの抽出は random 既定でよいか(ラウンドロビンで全参照を均等に使う方がデータセット向きかもしれない)。
3. 「色合い」の後段(非破壊 edits で締める)を既定 ON にするか(生成モデルの色を尊重したい場合もある)。
4. リモート sd-server の LoRA 受け渡し: 棚を rsync するか、ジョブに b64 同梱(数百 MB)か。
5. 別アプリの名前と置き場(atelier リポジトリを改名して再出発 / 新規)。

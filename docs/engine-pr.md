# engine PR — マスク/CLIP/顔ID (2026-09-03着手・正本)

目的: フォルダは目標=クラスを知っている。だから **収蔵物に意味の層(マスク/埋め込み/顔)を自動で持たせる**。

## 実装順(この順で1つずつ完成させる)

### Phase 1: マスク(gdino2seg) — 今回
- **計算はml-hubに委譲**(実戦済みgdino2seg=GDINO→SAM2、コンテナにtorch/重み有り)
- ml-hub側: `POST /annotation/engine-on-image {engine, image_b64, prompt, threshold?, output?}`
  → 一時ファイル→ `local_annotate.annotate_one()` → shapes(polygon/bbox)を返す薄い口を追加
  (デプロイ=docker cp api/main.py → コンテナ再起動。確認なし再起動OKのマシン)
- gallery側:
  - `POST /api/seg {shas?, album?, prompt?}` — ジョブ(SegState、1枚ずつ、ユーザー優先に道を譲る)。
    promptが空ならアルバムのgoalから対象語を抽出(内蔵LLMで名詞1-3語)
  - サイドカー: `seg: {prompt, shapes: [{cls, conf, points:[x,y,...]}], model: "gdino2seg", ts}`
  - 索引: `seg INT`列(マスク有無) → チップ「マスク済み」(q.seg=1)
  - ライトボックス: マスクの**ふわっとハイライト**(SVG polygonオーバーレイ・トグル)
  - フォルダ条件パネルに「マスク生成」ボタン(未マスク分に実行)
- 将来hook: 収集ゲート通過時に自動マスク(album.agent.auto_seg)

### Phase 1.5: セグメントの「本当の内蔵」(2026-09-03ユーザー指示: 外部いる？人物は軽いでしょ)
その通りで、**定番クラスなら外部不要**。二段構えにする:
- **内蔵(ort=onnxruntime crate + ONNXモデル直リンク)**: yolov8n-seg(13MB・COCO80クラス=person/dog/cat/car…)
  → 人物・動物・日用品は**完全内蔵・オフライン・ミリ秒級**。人物特化ならMODNet(25MB)で髪の毛レベルのマッティングも可
- **外部(ml-hub gdino2seg)**: オープン語彙(「アスパラの病斑」等COCO外の珍しい語)だけエスカレート
- 判定: goalから抽出したクラス語がCOCO80にマップできれば内蔵、できなければml-hub — コスト階段と同じ思想
- **ort基盤はCLIP ONNXにも使い回せる**(Phase 2の似た検索も内蔵化できる=一石二鳥)。M8配布版の core になる

### Phase 1.6: VLMバックエンドの複数登録+優先順位(2026-09-03指示)
- 設定 `store/config.json`: `vlm_backends: [{"type":"builtin"},{"type":"openai_compat","base":"http://farm1:11434","model":"qwen2.5vl:7b"},{"type":"gpt"},{"type":"claude"}]`
- 上から順に試す(ヘルス落ち→次へ)。farmの5090群(ml-hub分散farm設計)をopenai互換エンドポイントとして登録できる形
- enrich/judge/nlq全部この優先リストを通す。UIは設定画面でなくconfig直編集でv1は可

### Phase 1.7: 画像フィルタ拡張 — 高画質化/デノイズ(2026-09-03ユーザー要望)
- **非破壊editsのopとして追加**(原本不変・履歴スタックに乗る): `{op:"upscale", params:{scale:2|4}}` / `{op:"denoise", params:{strength}}`
- 実装はort基盤(Phase1.5と同じonnxruntime)に載せる:
  - 高画質化 = Real-ESRGAN系ONNX(realesr-general-x4v3 ≈5MB が軽くて汎用、アニメ絵はrealesrgan-x4plus-anime)
  - デノイズ = SCUNet/FBCNN系ONNX(JPEGノイズ・クロールゴミ画像の救済に効く)
- UI: 編集パネルに「✨高画質化」「デノイズ」ボタン、適用はサーバ焼き(renderキャッシュに乗る)
- 用途: 低解像度クロール画像の学習素材化・グッズ写真の救済。収集ゲートと組ませて「小さいが内容は良い」画像を昇格させる道もある
- **美肌/美白も追加(2026-09-03指示)**: 人物フォルダ向け。美肌=エッジ保存平滑化(bilateral)を肌領域に、美白=肌トーンの明度/彩度補正。seg/顔検出と組めば「顔だけに効かせる」が可能
- **基盤はfluent_scene(2026-09-03ユーザー決定)**: いまのedits.rs filterはFS_*名を真似たRust再実装(grayscale/sepia等の簡易フィルタのみ)。本命はfluent_scene本体(~/fluent_scene、C++)のフィルタパイプラインに委譲する形に組み替える — 美肌/美白/高画質化/デノイズはfluent_scene側にフィルタとして実装し、galleryは {op:"filter", params:{model:"FS_*", strength}} で呼ぶだけ。呼び出し口(CLI/共有ライブラリ/サービス化)は要調査。これでvlabor/fv系と同じフィルタ資産を共有できる
- ml-hubコンテナ内でopen_clip/transformers CLIP実行できるか確認→ダメならengine/に薄いvenv
- `emb BLOB`(512-dim f16)列 or store/emb/ + 総当たりkNN(10万枚までOK)
- 似た画像(ライトボックス「似た画像」→現フィルタ合成)/意味検索/重複クラスタ掃除/多様性ゲート

### Phase 3: 顔ID(参照画像ゲート)
- insightface(ml-hubコンテナ or engine venv)
- アルバムに参照sha数枚(⭐で指定)→顔埋め込み→収集ゲートで距離判定→人物フォルダ根治

## 既知の環境事実
- ml-hub: :7000 稼働中、コンテナ=ml-hub-app-1、api/は焼き込み(docker cp+restart)、shm 2gb
- ml-hubコンテナがギャラリーのstoreを読める保証は無い→**b64で画像を渡す**(結合を薄く)
- VLM/バックフィルとGPU共存: seg実行時はenrich.user_priorityで譲らせる
- UI憲法=docs/ui-v3.md(SVGアイコン/dialog禁止/フォルダ主語)

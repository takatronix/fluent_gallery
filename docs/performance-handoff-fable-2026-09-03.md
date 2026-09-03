# fluent_gallery 小サムネイル表示の性能調査 — Fable引き継ぎ

調査日: 2026-09-03  
対象: 現行Rustサーバー + `web/index.html`、実データ約12.8万枚  
結論: Rustという言語の問題ではない。一次原因はブラウザ側の全セルFLIPと画像tier切替不足。深くスクロールするとSQLiteの未索引ソートとfacet集計ロックも重なる。

## 実測サマリ

- DB画像: 約127,914件、原本約19.8GiB
- 360pxサムネ: 約127,896件、約3.32GiB、平均約27.9KiB
- 120px micro: 約4,381件、約13.4MiB、平均約3.2KiB。生成済みは約3.4%だけ
- `/api/images?limit=200`: offset 0 は19–21ms、20kは127–182ms、50kは406–489ms、100kは433–440ms
- `/api/facets`: cold 293–513ms、cache hit 0.4–0.7ms。cold実行と同時の通常22msのimages要求が441msまで待った
- 画像のRGBA展開量の上限目安:
  - 最新200件: thumb360 約70.3MiB / micro120 約7.8MiB
  - 2,400件: thumb360 約803.6MiB / micro120 約89.3MiB
- 稼働中CLIP backfillは約1.7 CPU core、RSS約773MiBまで上昇。主因ではないがDB書込み競合のノイズになる

## 原因

### P0-1. サイズ変更が全DOMセルに対してO(N) FLIPを毎入力で実行

- rangeは [`oninput="setCell(...)"`](../web/index.html#L305) なのでドラッグ中に高頻度実行される。
- [`setCell`](../web/index.html#L1259) は列数が変わるたび、全`.cell`の旧矩形を読む → gridを変更 → 次フレームに全`.cell`の新矩形を読む → 全件`animate()`する。
- 初期200セルでも1入力400回の矩形取得。DOM上限2,400セルでは4,800回の矩形取得と最大2,400アニメーションを、入力イベントごとに起こす。
- 36px付近は4pxの入力差でも列数が変わるため、ほぼ毎入力でこの経路へ入る。

これが「小さくする操作がガクガク」の最有力原因。

### P0-2. 小さくしても既存セルは360px画像のまま

- [`cellHtml`](../web/index.html#L1154) は新規生成時だけ、110px以下なら`/micro/`を選ぶ。
- `setCell`は既存`img.src`を変えない。そのため172pxから92/36pxへ縮小した直後のセルは360px JPEGのデコード済みsurfaceを保持する。
- microは約9分の1のRGBA量になるが、現状は縮小後の既存セルでその効果を得られない。

### P0-3. 固定2,400セルと密度非依存のセルアニメーション

- 現在の仮想化は [`DOM_WIN = 2400`](../web/index.html#L1105) の履歴上限であり、viewport基準のrow virtualizationではない。
- 通常サイズでは何十画面分もの画像ノードを保持する。小サイズでは画面内セル自体が数百になる。
- [`appear`](../web/index.html#L1200) は各セルを個別observeし、表示時にstagger付きtransformとtimeoutを作る。小サイズでは数百アニメーションが同時に始まる。
- 全セルに常時`will-change`を足す案は採らない。GPU texture/layer memoryをさらに悪化させる。

### P0-4. compact表示の初期fillがIntersectionObserver任せで不安定

- [`#more` observer](../web/index.html#L2358) はboot前の空gridですでにintersectする。その時点では`items`が空なので`loadMore()`はreturnする。
- 初回200件を描画後もsentinelがroot margin内のままならthreshold crossingがなく、再通知されない場合がある。
- 1920x1080・36pxでは約46列なので、200件は約5行しかなく画面を満たさない。

### P1-1. microの96.6%を閲覧要求中に生成

- [`/micro` miss処理](../src/main.rs#L435) は360 JPEGをdecode → 120へresize → JPEG encode → writeしてから返す。
- miss 1件は実測約1.84ms、直後のhitは0.29–0.70ms。単発は小さいが、compact表示で数百要求が集中するとCPU・I/O・decodeが重なる。
- ingestは [`process_one`](../src/store.rs#L298) と [`ingest_bytes`](../src/store.rs#L346) で360だけを生成しており、既にdecode済みの画像を活用できていない。

### P1-2. images一覧にORDER BY対応索引がない

- [`api_images`](../src/main.rs#L233) は毎ページCOUNT後、`ORDER BY ... LIMIT ... OFFSET ...`を実行する。
- 現スキーマの索引はtag/source/originのみで、通常表示は`SCAN images` + `USE TEMP B-TREE FOR ORDER BY`。
- 単純な`INDEX(ingested)`だけを追加すると希少filterが逆に遅くなることも実証済み:
  - rare source 約18ms → 約622ms
  - keep=1（0件）約607ms
  - seg=1 約505ms
- DBコピーでfilter列をindex内評価できるbrowse indexを使うと、offset 100kは528ms→4.4ms、keep=1は13.8ms、seg=1は16ms。source専用複合indexでrare sourceは0.37ms。

### P1-3. facet集計のcache TTLとUI周期が噛み合わず、単一DB Mutexを塞ぐ

- [`api_facets`](../src/main.rs#L281) は単一connectionのMutexを保持したまま多数のCOUNT/GROUP BYを直列実行する。
- サーバTTLは2.5秒だが、UIは [`30秒poll`](../web/index.html#L2379)。したがって定期pollはほぼ毎回cold集計になる。

### P2. async handler内の同期I/Oとbackfill競合

- `/thumb`、`/micro`などのhitでもasync handler内で`std::fs::read`してbyte vectorを毎回確保する。
- CLIP backfillは2,000件単位、各embeddingを個別autocommitし、短い休止だけで次batchへ進む。
- `tokio::sync::Mutex`への置換だけでは同期SQLがexecutorを止める問題は解決しない。

## 採用する修正

### Phase 1 — まず体感を直す

1. サイズinputを`requestAnimationFrame`でcoalesceし、1 frameにCSS列変更を1回だけ行う。ドラッグ中は矩形読取りとFLIPを一切しない。`localStorage`書込みも`change`/pointerup時だけ。
2. 操作終了時にアニメーションする場合は、viewport内セルのみ・最大100件。110px以下では個別stagger/hover拡大も無効または軽量化し、container-level fade程度にする。
3. 固定`DOM_WIN`方式を、`scrollTop / rowHeight / columns`から開始・終了rowを求める真のrow virtualizationへ変更。上・下spacerを持ち、DOMは「可視範囲 + 前後1 viewport程度」に限定する。36pxでは可視セル自体が多いので固定300件などにはしない。
4. サイズ確定後、現在のvirtual windowを再生成し、110px以下の既存セルも`/micro/`へ移す。microをpreload/decodeしてから小batchでswapし、360画像ノードを残さない。
5. `loadMore`に`inFlight`、request generation、重複排除を追加。初回描画とresize後に`scrollHeight >= clientHeight + overscan`になるまで明示的に最大500件ずつ逐次fillする。observerだけに初期fillを任せない。
6. `flipRender`、fit切替、出現演出もvirtual window内だけを対象にする。compact時は大量の個別アニメーションを作らない。

### Phase 2 — asset/APIを直す

1. ingestの共通thumb helperで、1回decodeした画像から360pxと120pxを同時生成する。
2. 既存約12.3万枚は低優先度・同時数制限付きでmicro backfillする。UIアクセス直後はpause。完了後はmicro件数をgrid thumb件数と一致させる。
3. `/micro` missは非常用として残すが、同一SHAの同時生成をsingle-flightにする。
4. 通常browseは`(ingested DESC, sha1 DESC, filter columns...)`の軽量なscan用index、sourceは`(source COLLATE NOCASE, ingested DESC, sha1 DESC)`、quality/bytes/costは専用sort indexを候補にする。`ANALYZE`後、下記query matrixで採否を決める。単純ingested indexだけを入れない。
5. UIの次ページはOFFSETからkeyset cursor（sort値 + ingested + sha1）へ移す。初回だけtotalを返し、次ページで同じCOUNTを繰り返さない。旧offsetは互換用に残す。
6. facetは別readonly connectionのblocking workerで再計算し、stale-while-revalidateで古い値を即返す。TTLは60秒以上、できれば画像更新時invalidate。

候補DDL（盲目的に採用せずquery matrixで検証）:

```sql
CREATE INDEX IF NOT EXISTS idx_images_browse
ON images(
  ingested DESC, sha1 DESC,
  source COLLATE NOCASE, origin, vlm_model,
  scene, subject, style, gender, people_count, age_group, framing, animal,
  nsfw, keep, seg, watermark, rights, quality, bytes, cost
);

CREATE INDEX IF NOT EXISTS idx_images_source_browse
ON images(source COLLATE NOCASE, ingested DESC, sha1 DESC);

CREATE INDEX IF NOT EXISTS idx_images_sort_quality
ON images((quality IS NULL), quality DESC, ingested DESC, sha1 DESC);

CREATE INDEX IF NOT EXISTS idx_images_sort_bytes
ON images(bytes DESC, ingested DESC, sha1 DESC);

CREATE INDEX IF NOT EXISTS idx_images_sort_cost
ON images((cost IS NULL), cost DESC, ingested DESC, sha1 DESC);
```

### Phase 3 — 競合を減らす

1. thumbnail hit配信を`tokio::fs`/streaming responseまたは`ServeFile`へ移す。
2. rusqlite処理をblocking worker + readonly connection poolへ分離する。
3. CLIP結果を短いtransactionでbatch保存し、最近UI要求が来たらbackfillを一時停止する。
4. `/api/activity`の外部commandと`/api/cache/stats`の全FS走査もblocking worker/cacheへ移す。これはP0/P1後。

## やらないこと

- Rustで書き直す（既にRustで、一次原因はDOM/layout/decode）
- page limitや`DOM_WIN`を増やす
- 360px画像をCSSだけで小さく見せる
- 全セルへ`will-change`を付ける
- `INDEX(ingested)`だけを追加して完了とする
- `std::sync::Mutex`を`tokio::sync::Mutex`へ機械置換して完了とする

## 受入基準

実DB約127k件、1920x1080、deviceScaleFactor 1/2、セル172→92→36pxで測る。

1. 2秒のサイズscrubで50ms超Long Taskが0、frame interval p95が20ms以下。
2. 110px以下へ確定後、viewport内の画像URLが`/micro/`、natural sizeが120px以下。古い360 nodeが残らない。
3. 1万件まで往復scroll後もDOM数・JS heap・decoded image量が履歴に比例して増えない。
4. compact時の初期fillでviewportに空白がなく、同一offset/cursorの重複requestがない。
5. micro backfill後、小サイズ閲覧中の`.m.jpg`新規writeが0。micro欠損0。
6. cursorによる通常次ページp95 <20ms。互換offset 0/20k/50k/100kはp95 <50ms。
7. query matrix（default、rare source、keep、seg、origin、animal、quality/bytes/cost sort）が全て30ms以内で、通常sortにTEMP B-TREEが出ない。
8. facet cold更新中でもimages p95 <30ms。facet通常応答はstale hitで5ms以内。
9. lightbox、selection、filter、戻りscroll位置、追加画像のdrop演出を回帰させない。

## 実装順

`全件FLIP廃止 + tier swap` → `row virtualization + fill制御` → `micro ingest/backfill` → `cursor/index` → `facet分離` → `I/O/backfill競合低減`

## Fable確認結果

既存の`claude-fable-5`開発セッションへ本書を渡し、コードとの照合まで完了した。Fableも上記の一次原因と実装順を採用し、最初の安全な作業単位を「`web/index.html`だけで全件FLIP廃止 + compact時のtier swap」とした。

Fableから追加された実装上の注意:

- 23列のbrowse indexは、進行中のauto-enrichが索引収載列を更新するたび書込み増幅を起こす。query matrixだけでなく、enrich 1件の時間・WAL量・backfill速度も比較し、作成時期を決める。
- row virtualizationはselection、lightboxのmorph-back、`c_<sha>`参照、liveDrop、NEW、戻りscroll位置の回帰面が広い。Phaseごとに確認する。
- micro backfill前のtier swapは数百missを発生させ得るので、小batchで平準化する。
- NULLを含むquality/cost sortのkeyset cursorは複雑なので、default/source cursorを先行し、旧offset互換を残す。

計測によりmicro missを1件生成した以外、調査側は製品コードを変更していない。

---
## 実施記録(2026-09-03 Fable)

- 作業単位1+2実装済(web/index.htmlのみ): 全件FLIP廃止/rAF集約/dense≤110px/可視tier切替/真のrow virtualization(差分更新+分割構築+ドラッグ間引き)。
- 実測(headless Chrome, サイズドラッグ模擬): 平均907→50ms, p95 4007→172ms, 最悪9086→455ms(通常)・~3.3s(dense突入の単発), LongTask合計93s→3.5-5s。DOMセル1402→120-180(通常)。
- dense突入時の単発3秒級スパイクが残: 千数百セルのlayout+decode。content-visibilityは効果なし(計測済)。次はPhase2のmicro事前生成が本命(decode量1/9)。
- 検索は7エンジン並列化済(crawl.rs)。次候補=クエリパイプライン(判定中に次クエリの検索を先読み)。DDG BAN回避のため検索自体の多重化はしない。

## 実施記録2(2026-09-03 Fable, Phase 2)

- Phase2のmicro事前生成を実装・デプロイ済:
  - `store::write_thumbs`: ingest時にdecode1回で360+120を同時焼き(process_one/ingest_bytes共通化)。
  - `store::micro_backfill` + 常駐タスク: 既存分を2000件/batch・UI直近10秒アクセスで遠慮しながら焼き切る。実績: 121,943枚を約12分で生成、カバレッジ100%(micro 128,277 / thumb360 128,103、差分は並行ingest分)。以後は6時間毎の安全弁見回り。
  - `/micro` missはsingle-flight化(App.micro_inflight、同一SHA同時生成を1回に)。削除経路(trash/source_trash)にmicro削除を追加。janitorは.p.jpgのみ対象なのでmicroは掃除されない(確認済)。
  - UIアクセス検知=App.ui_hot(api_images/thumb/microでtouch)。
- ついで修正: reload()/loadMore()に世代ガード(rgen)追加 — ナビ連打で遅い古応答が新しい場所を上書きする「メニュー切り替えても前の内容が残る」バグの根治(ユーザー報告2026-09-03)。
- 残り: Phase2のcursor/index(P1-2)、facet分離(P1-3)、Phase3のI/O・backfill競合低減。
- クローラのクエリパイプライン実装済(同日追記): spawn_img_searches()で画像5エンジンを関数化、現クエリのゲート/判定中に次クエリの検索を先読み(prefetched)。各エンジン同時1本の原則は維持(DDG BAN回避)。YouTube/X(オプトイン系)は対象外。E2Eスモーク済。

## 多様テストケースの知見(2026-09-03)
- 全4テスト目標15/15達成。非人物は無料7Bで十分: 錆テクスチャ75%・ヨナグニサン(ニッチ蛾)47%。
- 種の識別は難度高: 甲斐犬20%(他犬種の却下に検査76回)・ニジイロクワガタ34%(Gemini)。犬種版顔IDは将来課題。
- ニッチ被写体でも候補~330件は取れる(即枯渇しない)。seed_queriesテンプレは全テストで初手から機能。
- Gemini 2.5 Flash判定: YENA 10枚$0.016で成功(Haiku比~1/2、品質同等目視待ち)。judge単価: 7B$0 / Gemini~$0.0016 / Haiku~$0.003 / Sonnet~$0.01(全て/枚検査)。

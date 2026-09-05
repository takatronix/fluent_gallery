//! fluent_gallery — 画像ライブラリ本体(Rust)。
//! 原則: 人を待たせない(重い処理は全てバックグラウンドジョブ+進捗)、UIをロックしない、
//!       正本はサイドカー・SQLiteは使い捨て索引、AI 1st(全操作がAPI=MCP化可能)。

mod crawl;
mod edits;
mod config;
mod enrich;
mod gen;
mod lora;
mod llm;
mod media;
#[cfg(feature = "faceid")]
mod faceid;
mod samples;
mod urlimport;
mod onnx;
mod vlm;
mod seg;
mod store;

use axum::{
    extract::{Path as AxPath, Query, State},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};

struct App {
    db: Mutex<Connection>,
    root: PathBuf,
    ingest: Arc<store::Progress>,
    ingest_label: Mutex<String>,
    enrich: Arc<enrich::EnrichState>,
    crawl: Arc<crawl::CrawlState>,
    crawl_queue: Mutex<Vec<CrawlIn>>, // 順番待ち(同時1本=VLM直列の現実に合わせ、弾かず並ばせる)
    gen: Arc<gen::GenState>, // AI生成フォルダ(sd-server 子プロセス+ジョブ状態、docs/gen-design.md)
    lora: Arc<lora::LoraState>, // LoRA 棚の取り込み/試し描きの進捗
    llm: Arc<llm::LlmState>,
    vlm: Arc<vlm::VlmState>, // 内蔵VLM(llama-server 子プロセス)
    seg: Arc<seg::SegState>,
    http: reqwest::Client,
    ui_hot: std::sync::atomic::AtomicU64, // 最後にUIが画像/一覧を要求したunix秒(backfillの遠慮判断)
    micro_inflight: Mutex<std::collections::HashSet<String>>, // /micro miss生成のsingle-flight
    atlas_inflight: Mutex<std::collections::HashSet<String>>, // /atlas miss生成のsingle-flight(key+fit)
    workers: Mutex<serde_json::Map<String, Value>>, // 裏方常駐の自己申告黒板(AI稼働ボードに出す)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl App {
    fn touch_ui(&self) {
        let t = now_secs();
        self.ui_hot.store(t, Relaxed);
        self.crawl.ui_hot.store(t, Relaxed); // 収集の内蔵VLM判定も閲覧中は道を譲る(CPU16コア占有でUI窒息の再発防止)
        self.gen.ui_hot.store(t, Relaxed); // 生成も閲覧中は次の1枚を待つ(GPU を取り合わない)
        self.enrich.user_priority(10); // 属性付けバックフィルは1件ごとに譲る(既存機構に配線)
    }
    fn ui_recent(&self, secs: u64) -> bool {
        now_secs().saturating_sub(self.ui_hot.load(Relaxed)) < secs
    }
    /// 裏方常駐が「今なにしてる」を書き込む(AI稼働ボードで見える)
    fn set_worker(&self, name: &str, on: bool, detail: String) {
        self.workers.lock().unwrap().insert(name.into(), json!({"on": on, "detail": detail}));
    }
}

type S = State<&'static App>;

/// 待受ポート(常駐ワーカーの自己呼び出し用。以前は 8790 固定で PORT を変えると空振りしていた)
static BIND_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(8790);
/// ♻自動収集の見回り周期と次回時刻(epoch 秒)。UI に出すために公開する
/// ♻見回りの周期(設定 autopilot.interval_min、既定 30 分)。ループが毎回読むので設定変更が次の周期から効く
fn autopilot_secs() -> u64 { config::get_u64("autopilot.interval_min", 30).clamp(1, 24 * 60) * 60 }
static AUTOPILOT_NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// ---------- 読み ----------

#[derive(Deserialize, Default)]
struct Q {
    #[serde(default)] q: String,
    #[serde(default)] sem: String, // CLIP テキスト意味検索(英語)。空でも q が 0 件なら自動でこちらに落ちる
    #[serde(default)] tag: String,
    #[serde(default)] source: String,
    #[serde(default)] origin: String,
    #[serde(default)] vlm_: String,
    #[serde(default)] scene: String,
    #[serde(default)] subject: String,
    #[serde(default)] style: String,
    #[serde(default)] gender: String,
    #[serde(default)] people_count: String,
    #[serde(default)] age_group: String,
    #[serde(default)] framing: String,
    #[serde(default)] animal: String,
    #[serde(default)] seg: String, // "1"=マスク済みのみ
    #[serde(default)] watermark: String, // "0"=透かし無しのみ / "1"=ありのみ
    #[serde(default)] nsfw: String,
    #[serde(default)] keep: String,
    #[serde(default)] rights: String, // "clean"=権利明示あり / "unknown"=未確認 / 具体ライセンス名
    #[serde(default)] sort: String,
    #[serde(default)] min_quality: i64,
    #[serde(default)] similar: String, // sha1 — この画像にCLIPで似ている順(似た画像フォルダの芯)
    #[serde(default)] view: String, // "grid"=UI一覧に必要な最小フィールドだけ
    #[serde(default = "d_limit")] limit: i64,
    #[serde(default)] offset: i64,
}
fn d_limit() -> i64 { 200 }

const COLS: &str = "sha1, ext, w, h, bytes, phash, source, origin, ingested, tint, vlm_model, caption, quality, nsfw, scene, subject, lighting, style, keep, cost, rights, gender, people_count, age_group, framing, watermark, animal, erev";
// 画像バイトはsha1 URLで別配信。一覧は初期箱/セル/選択に要る値と属性3bitだけにする。
// attrs: 1=権利クリーン / 2=有料 / 4=セーフ。詳細は開いた時の/api/metaが正本。
const GRID_COLS: &str = "sha1, w, h, source, keep, erev, \
    (CASE WHEN rights IS NOT NULL AND rights NOT IN ('unknown','') THEN 1 ELSE 0 END) + \
    (CASE WHEN cost > 0 THEN 2 ELSE 0 END) + \
    (CASE WHEN nsfw = 0 THEN 4 ELSE 0 END) AS attrs";

fn row_to_json(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    let g = |i: usize| r.get::<_, Option<String>>(i).ok().flatten().map(Value::from).unwrap_or(Value::Null);
    let gi = |i: usize| r.get::<_, Option<i64>>(i).ok().flatten().map(Value::from).unwrap_or(Value::Null);
    Ok(json!({
        "sha1": g(0), "ext": g(1), "w": gi(2), "h": gi(3), "bytes": gi(4), "phash": g(5),
        "source": g(6), "origin": g(7), "ingested": r.get::<_, Option<f64>>(8).ok().flatten(),
        "tint": g(9), "vlm_model": g(10), "caption": g(11), "quality": gi(12), "nsfw": gi(13),
        "scene": g(14), "subject": g(15), "lighting": g(16), "style": g(17), "keep": gi(18),
        "cost": r.get::<_, Option<f64>>(19).ok().flatten(), "rights": g(20), "gender": g(21),
        "people_count": g(22), "age_group": g(23), "framing": g(24), "watermark": gi(25), "animal": g(26),
        "erev": g(27),
    }))
}

fn row_to_grid_json(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    let g = |i: usize| r.get::<_, Option<String>>(i).ok().flatten().map(Value::from).unwrap_or(Value::Null);
    let gi = |i: usize| r.get::<_, Option<i64>>(i).ok().flatten().map(Value::from).unwrap_or(Value::Null);
    Ok(json!({
        "sha1": g(0), "w": gi(1), "h": gi(2), "source": g(3), "keep": gi(4),
        "erev": g(5), "attrs": gi(6),
    }))
}

const ATLAS_COLS: usize = 20;
const ATLAS_MAX_ITEMS: usize = 200;
const ATLAS_TILE: u32 = 120;
const ATLAS_QUALITY: u8 = 72;
const ATLAS_VERSION: u8 = 1;

#[derive(Clone, Deserialize, Serialize)]
struct AtlasMember {
    sha1: String,
    erev: String,
}

#[derive(Deserialize, Serialize)]
struct AtlasManifest {
    version: u8,
    items: Vec<AtlasMember>,
}

fn valid_sha1(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn atlas_key(items: &[AtlasMember]) -> String {
    let mut h = Sha1::new();
    h.update(b"fluent-gallery-atlas\0");
    h.update([ATLAS_VERSION]);
    h.update((ATLAS_COLS as u32).to_be_bytes());
    h.update(ATLAS_TILE.to_be_bytes());
    h.update([ATLAS_QUALITY]);
    h.update(b"jpeg-triangle-cover-contain-v1\0");
    h.update((items.len() as u32).to_be_bytes());
    for item in items {
        h.update(item.sha1.as_bytes());
        h.update((item.erev.len() as u32).to_be_bytes());
        h.update(item.erev.as_bytes());
    }
    hex::encode(h.finalize())
}

fn atlas_dir(root: &Path) -> PathBuf {
    root.join("store/renders/atlas")
}

fn atlas_manifest_path(root: &Path, key: &str) -> PathBuf {
    atlas_dir(root).join(format!("{key}.json"))
}

fn atlas_image_path(root: &Path, key: &str, fit: bool) -> PathBuf {
    atlas_dir(root).join(format!("{key}.{}.jpg", if fit { "contain" } else { "cover" }))
}

/// 同じディレクトリの一時ファイルからrenameし、読み手に途中のJSON/JPEGを見せない。
fn atomic_publish(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let parent = path.parent().ok_or_else(|| std::io::Error::other("no parent"))?;
    std::fs::create_dir_all(parent)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("atlas");
    let tmp = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), nonce));
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(_) if path.exists() => {
            let _ = std::fs::remove_file(tmp);
            Ok(()) // 同じcontent keyを別requestが先にpublishした
        }
        Err(e) => {
            let _ = std::fs::remove_file(tmp);
            Err(e)
        }
    }
}

/// grid APIの並びそのものをcontent keyにする。offset/queryをkeyにしないので誤った再利用をしない。
fn prepare_grid_atlas(root: &Path, items: &[Value]) -> Option<Value> {
    if items.is_empty() || items.len() > ATLAS_MAX_ITEMS {
        return None;
    }
    let members: Vec<AtlasMember> = items
        .iter()
        .map(|item| {
            let sha1 = item["sha1"].as_str()?.to_string();
            let erev = item["erev"].as_str().unwrap_or("").to_string();
            (valid_sha1(&sha1) && erev.len() <= 128).then_some(AtlasMember { sha1, erev })
        })
        .collect::<Option<Vec<_>>>()?;
    let key = atlas_key(&members);
    let manifest = AtlasManifest { version: ATLAS_VERSION, items: members };
    let bytes = serde_json::to_vec(&manifest).ok()?;
    if let Err(e) = atomic_publish(&atlas_manifest_path(root, &key), &bytes) {
        eprintln!("atlas manifest write failed: {e}");
        return None;
    }
    Some(json!({
        "id": key,
        "cols": ATLAS_COLS,
        "rows": items.len().div_ceil(ATLAS_COLS),
    }))
}

fn build_where(q: &Q) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let mut wh: Vec<String> = vec!["1=1".into()];
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![];
    if !q.source.is_empty() {
        wh.push("source LIKE ?".into());
        args.push(Box::new(format!("{}%", q.source)));
    }
    // 実写/生成はVLMの見た目(style)を最優先(sourceからの推定はイラストDLを「実写」と嘘をつく)
    match q.origin.as_str() {
        "real" => wh.push("(style='photo' OR (style IS NULL AND origin='real'))".into()),
        "synthetic" => {
            wh.push("(origin='synthetic' OR style IN ('illustration','anime','3dcg','painting','sketch'))".into())
        }
        _ => {}
    }
    match q.vlm_.as_str() {
        "done" => wh.push("vlm_model IS NOT NULL".into()),
        "none" => wh.push("vlm_model IS NULL".into()),
        // stale=新属性(gender等)が無い物。バックフィルが再起動しても済んだ分をスキップできる
        "stale" => wh.push("(vlm_model IS NULL OR gender IS NULL)".into()),
        _ => {}
    }
    if q.watermark == "0" {
        wh.push("(watermark IS NULL OR watermark=0)".into());
    } else if q.watermark == "1" {
        wh.push("watermark=1".into());
    }
    for (col, v) in [("scene", &q.scene), ("subject", &q.subject), ("style", &q.style), ("gender", &q.gender),
                     ("people_count", &q.people_count), ("age_group", &q.age_group), ("framing", &q.framing),
                     ("animal", &q.animal)] {
        if !v.is_empty() {
            wh.push(format!("{col}=?"));
            args.push(Box::new(v.clone()));
        }
    }
    if q.nsfw == "0" || q.nsfw == "1" {
        wh.push("nsfw=?".into());
        args.push(Box::new(q.nsfw.parse::<i64>().unwrap()));
    }
    if q.keep == "1" {
        wh.push("keep=1".into());
    }
    if q.seg == "1" {
        wh.push("seg=1".into());
    }
    match q.rights.as_str() {
        "" => {}
        "clean" => wh.push("rights IS NOT NULL AND rights NOT IN ('unknown','')".into()),
        "unknown" => wh.push("(rights IS NULL OR rights IN ('unknown',''))".into()),
        r => {
            wh.push("rights=?".into());
            args.push(Box::new(r.to_string()));
        }
    }
    if q.min_quality > 0 {
        wh.push("quality>=?".into());
        args.push(Box::new(q.min_quality));
    }
    if !q.tag.is_empty() {
        wh.push("sha1 IN (SELECT sha1 FROM tags WHERE tag=?)".into());
        args.push(Box::new(q.tag.to_lowercase()));
    }
    if !q.q.is_empty() {
        // キャプションFTSに加えてタグ(顔IDの人物名/手動タグ)もヒットさせる(2026-09-03要望)
        wh.push("(sha1 IN (SELECT sha1 FROM captions WHERE captions MATCH ?) OR sha1 IN (SELECT sha1 FROM tags WHERE tag LIKE ?))".into());
        args.push(Box::new(q.q.clone()));
        args.push(Box::new(format!("%{}%", q.q.trim().to_lowercase())));
    }
    (wh.join(" AND "), args)
}

fn query_shas(app: &App, q: &Q) -> Vec<String> {
    let (cond, args) = build_where(q);
    let db = app.db.lock().unwrap();
    let params: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    db.prepare(&format!("SELECT sha1 FROM images WHERE {cond} ORDER BY ingested DESC, sha1 DESC"))
        .and_then(|mut st| {
            st.query_map(params.as_slice(), |r| r.get::<_, String>(0))
                .map(|rows| rows.filter_map(Result::ok).collect())
        })
        .unwrap_or_default()
}

// ---- 似た画像(CLIP埋め込み全探索。11万枚×512次元でも数十ms) ----
static EMBS: std::sync::OnceLock<Mutex<(usize, Vec<(String, Vec<f32>)>)>> = std::sync::OnceLock::new();

fn emb_from_bytes(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// クエリshaに似ている順のsha一覧(自分を先頭に含む)。埋め込みはRAMキャッシュ(件数が増えたら再読込)
fn similar_ranked(app: &App, sha: &str, top: usize) -> Vec<String> {
    let query_emb: Option<Vec<f32>> = (|| {
        let db = app.db.lock().unwrap();
        let stored: Option<Vec<u8>> = db
            .query_row("SELECT emb FROM embs WHERE sha1=?1 AND length(emb)>=2048", [sha], |r| r.get(0))
            .ok();
        if let Some(b) = stored {
            return Some(emb_from_bytes(&b));
        }
        // 未計算ならその場で計算して保存(1枚十数ms)
        let ext: String = db
            .query_row("SELECT ext FROM images WHERE sha1=?1", [sha], |r| r.get(0))
            .unwrap_or_else(|_| "jpg".into());
        drop(db);
        let img = image::open(store::thumb_path(&app.root, sha))
            .or_else(|_| image::open(store::image_path(&app.root, sha, &ext)))
            .ok()?;
        let e = onnx::embed(&app.root, &img)?;
        let bytes: Vec<u8> = e.iter().flat_map(|f| f.to_le_bytes()).collect();
        let _ = app.db.lock().unwrap().execute(
            "INSERT OR REPLACE INTO embs(sha1, emb) VALUES(?2, ?1)",
            rusqlite::params![bytes, sha],
        );
        Some(e)
    })();
    let Some(qe) = query_emb else { return vec![] };
    ranked_by_emb(app, &qe, top)
}

/// 任意の 512 次元(画像でもテキストでも)に近い順の sha 一覧。埋め込みは RAM キャッシュ(件数が変わったら再読込)
fn ranked_by_emb(app: &App, qe: &[f32], top: usize) -> Vec<String> {
    let cache = EMBS.get_or_init(|| Mutex::new((0, vec![])));
    let mut c = cache.lock().unwrap();
    let n: usize = app
        .db
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM embs WHERE length(emb)>=2048", [], |r| r.get(0))
        .unwrap_or(0);
    if n != c.0 {
        let db = app.db.lock().unwrap();
        c.1 = db
            .prepare("SELECT sha1, emb FROM embs WHERE length(emb)>=2048")
            .and_then(|mut st| {
                st.query_map([], |r| Ok((r.get::<_, String>(0)?, emb_from_bytes(&r.get::<_, Vec<u8>>(1)?))))
                    .map(|rows| rows.filter_map(Result::ok).collect())
            })
            .unwrap_or_default();
        c.0 = n;
        println!("🧭 埋め込みキャッシュ再読込: {n}件");
    }
    let mut scored: Vec<(f32, &String)> = c
        .1
        .iter()
        .map(|(s, e)| (e.iter().zip(qe).map(|(a, b)| a * b).sum::<f32>(), s))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().take(top).map(|(_, s)| s.clone()).collect()
}

async fn api_images(State(app): S, Query(q): Query<Q>) -> Json<Value> {
    app.touch_ui();
    let grid_view = q.view == "grid";
    if !q.similar.is_empty() {
        // 似ている順ビュー(v1: 他フィルタは適用しない・順位が正)
        let sha = q.similar.clone();
        let (limit, offset) = (q.limit.clamp(1, 500) as usize, q.offset.max(0) as usize);
        let app2 = app;
        let ranked = tokio::task::spawn_blocking(move || similar_ranked(app2, &sha, 4000))
            .await
            .unwrap_or_default();
        let db = app.db.lock().unwrap();
        let items: Vec<Value> = ranked
            .iter()
            .skip(offset)
            .take(limit)
            .filter_map(|s| {
                if grid_view {
                    db.query_row(&format!("SELECT {GRID_COLS} FROM images WHERE sha1=?1"), [s], row_to_grid_json).ok()
                } else {
                    db.query_row(&format!("SELECT {COLS} FROM images WHERE sha1=?1"), [s], row_to_json).ok()
                }
            })
            .collect();
        drop(db);
        let atlas = grid_view.then(|| prepare_grid_atlas(&app.root, &items)).flatten();
        let mut payload = json!({"total": ranked.len(), "items": items});
        if let Some(atlas) = atlas {
            payload["atlas"] = atlas;
        }
        return Json(payload);
    }
    // 意味検索(CLIP テキスト): sem= を明示するか、q の全文検索が 0 件(キャプション未取得の Mac 初日など)なら
    // 英語 q を CLIP テキスト埋め込みにして似ている順で返す。VLM 無しでも「dog」「a boat on water」が引ける
    let sem_query = if !q.sem.is_empty() { Some(q.sem.clone()) } else { None };
    // DB ガードと Box<dyn ToSql>(どちらも Send でない)を await をまたいで持たないよう、件数はブロック内で取る
    let total: i64 = {
        let (cond, args) = build_where(&q);
        let db = app.db.lock().unwrap();
        let params: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        db.query_row(&format!("SELECT COUNT(*) FROM images WHERE {cond}"), params.as_slice(), |r| r.get(0)).unwrap_or(0)
    };
    let sem_query = sem_query.or_else(|| (total == 0 && !q.q.trim().is_empty() && onnx::text_present(&app.root)).then(|| q.q.clone()));
    if let Some(text) = sem_query {
        let (limit, offset) = (q.limit.clamp(1, 500) as usize, q.offset.max(0) as usize);
        let app2 = app;
        let ranked = tokio::task::spawn_blocking(move || {
            onnx::embed_text(&app2.root, &text).map(|e| ranked_by_emb(app2, &e, 400)).unwrap_or_default()
        }).await.unwrap_or_default();
        let db = app.db.lock().unwrap();
        let items: Vec<Value> = ranked.iter().skip(offset).take(limit)
            .filter_map(|s| db.query_row(&format!("SELECT {COLS} FROM images WHERE sha1=?1"), [s], row_to_json).ok())
            .collect();
        return Json(json!({"total": ranked.len(), "items": items, "semantic": true}));
    }
    let (cond, args) = build_where(&q);
    let db = app.db.lock().unwrap();
    let params: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    // 並び順はホワイトリスト(SQL注入防止)。NULLは常に後ろへ
    let order = match q.sort.as_str() {
        "old" => "ingested ASC, sha1 ASC",
        "quality" => "quality IS NULL, quality DESC, ingested DESC, sha1 DESC",
        "big" => "bytes DESC, sha1 DESC",
        "cost" => "cost IS NULL, cost DESC, ingested DESC, sha1 DESC",
        _ => "ingested DESC, sha1 DESC",
    };
    let cols = if grid_view { GRID_COLS } else { COLS };
    let items: Vec<Value> = db
        .prepare(&format!(
            "SELECT {cols} FROM images WHERE {cond} ORDER BY {order} LIMIT {} OFFSET {}",
            q.limit.clamp(1, 500),
            q.offset.max(0)
        ))
        .and_then(|mut st| {
            st.query_map(params.as_slice(), |r| {
                if grid_view { row_to_grid_json(r) } else { row_to_json(r) }
            })
                .map(|rows| rows.filter_map(Result::ok).collect())
        })
        .unwrap_or_default();
    drop(db);
    let atlas = grid_view.then(|| prepare_grid_atlas(&app.root, &items)).flatten();
    let mut payload = json!({"total": total, "items": items});
    if let Some(atlas) = atlas {
        payload["atlas"] = atlas;
    }
    Json(payload)
}

async fn api_facets(State(app): S) -> Json<Value> {
    // 2.5秒TTLキャッシュ: 全画像のGROUP BY×15本で1.8秒かかり、2秒ポーラーと重なって
    // dbロック渋滞→UI全体もっさりの主因だった(ml-hub metrics-poll-hangと同じ病 2026-09-03)
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<(std::time::Instant, Value)>>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(None));
    if let Some((t, v)) = cache.lock().unwrap().as_ref() {
        if t.elapsed().as_millis() < 2500 {
            return Json(v.clone());
        }
    }
    let db = app.db.lock().unwrap();
    let count = |sql: &str| -> i64 { db.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
    let group = |sql: &str| -> Value {
        let mut m = serde_json::Map::new();
        if let Ok(mut st) = db.prepare(sql) {
            if let Ok(rows) = st.query_map([], |r| {
                Ok((r.get::<_, Option<String>>(0)?.unwrap_or_else(|| "?".into()), r.get::<_, i64>(1)?))
            }) {
                for (k, v) in rows.flatten() {
                    m.insert(k, v.into());
                }
            }
        }
        Value::Object(m)
    };
    let v = json!({
        "total": count("SELECT COUNT(*) FROM images"),
        "bytes": count("SELECT COALESCE(SUM(bytes),0) FROM images"),
        "enriched": count("SELECT COUNT(*) FROM images WHERE vlm_model IS NOT NULL"),
        "cost_usd": db.query_row("SELECT COALESCE(SUM(cost),0) FROM images", [], |r| r.get::<_, f64>(0)).unwrap_or(0.0),
        "source_cost": group("SELECT source, CAST(SUM(cost) AS INT) FROM images WHERE cost IS NOT NULL GROUP BY source"),
        "rights": group("SELECT COALESCE(NULLIF(rights,''),'unknown'), COUNT(*) FROM images GROUP BY 1 ORDER BY 2 DESC"),
        "origins": group("SELECT origin, COUNT(*) FROM images GROUP BY origin"),
        "sources": group("SELECT source, COUNT(*) FROM images GROUP BY source ORDER BY 2 DESC"),
        "scenes": group("SELECT scene, COUNT(*) FROM images WHERE scene IS NOT NULL GROUP BY scene ORDER BY 2 DESC LIMIT 20"),
        "subjects": group("SELECT subject, COUNT(*) FROM images WHERE subject IS NOT NULL GROUP BY subject ORDER BY 2 DESC LIMIT 20"),
        "styles": group("SELECT style, COUNT(*) FROM images WHERE style IS NOT NULL GROUP BY style ORDER BY 2 DESC LIMIT 12"),
        "genders": group("SELECT gender, COUNT(*) FROM images WHERE gender IS NOT NULL AND gender NOT IN ('none','') GROUP BY gender ORDER BY 2 DESC"),
        "framings": group("SELECT framing, COUNT(*) FROM images WHERE framing IS NOT NULL GROUP BY framing ORDER BY 2 DESC"),
        "ages": group("SELECT age_group, COUNT(*) FROM images WHERE age_group IS NOT NULL AND age_group NOT IN ('none','') GROUP BY age_group ORDER BY 2 DESC"),
        "watermarked": count("SELECT COUNT(*) FROM images WHERE watermark=1"),
        "segmented": count("SELECT COUNT(*) FROM images WHERE seg=1"),
        "animals": group("SELECT animal, COUNT(*) FROM images WHERE animal IS NOT NULL AND animal NOT IN ('none','') GROUP BY animal ORDER BY 2 DESC"),
        "tags": group("SELECT tag, COUNT(*) FROM tags GROUP BY tag ORDER BY 2 DESC LIMIT 40"),
    });
    drop(db);
    *cache.lock().unwrap() = Some((std::time::Instant::now(), v.clone()));
    Json(v)
}

async fn api_meta(State(app): S, AxPath(sha1): AxPath<String>) -> impl IntoResponse {
    match store::load_meta(&app.root, &sha1) {
        Some(mut m) => {
            // UIがレンダURLをrevで固定キャッシュできるよう同梱
            let e = m.get("edits").cloned().unwrap_or_else(|| json!([]));
            m["edits_rev"] = json!(edits::rev(&e));
            Json(m).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn mime(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "image/jpeg",
    }
}

const IMMUTABLE: &str = "public, max-age=31536000, immutable"; // sha1 URLは内容不変

async fn img(State(app): S, AxPath(sha1): AxPath<String>) -> impl IntoResponse {
    let Some(m) = store::load_meta(&app.root, &sha1) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let ext = m["ext"].as_str().unwrap_or("png").to_string();
    match std::fs::read(store::image_path(&app.root, &sha1, &ext)) {
        Ok(b) => ([(header::CONTENT_TYPE, mime(&ext)), (header::CACHE_CONTROL, IMMUTABLE)], b).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------- 書き出し: 個別ダウンロード(原本そのまま) / zip(画像+サイドカー+manifest) ----------
/// Content-Disposition(日本語ファイル名は RFC 5987 の filename*=UTF-8''… で)
fn attachment(name: &str) -> String {
    let ascii: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
    let enc: String = name.bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
        _ => format!("%{b:02X}"),
    }).collect();
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{enc}")
}

/// 原本をファイルとしてダウンロード。URL末尾の {fname} は保存名(Tauriの on_download はURLしか見ないので経路に含める)
async fn dl_img(State(app): S, AxPath((sha1, fname)): AxPath<(String, String)>) -> impl IntoResponse {
    let Some(m) = store::load_meta(&app.root, &sha1) else { return StatusCode::NOT_FOUND.into_response() };
    let ext = m["ext"].as_str().unwrap_or("png").to_string();
    match std::fs::read(store::image_path(&app.root, &sha1, &ext)) {
        Ok(b) => ([(header::CONTENT_TYPE, mime(&ext).to_string()), (header::CONTENT_DISPOSITION, attachment(&fname))], b).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Clone, serde::Serialize)]
struct ExportJob { name: String, total: usize, done: usize, ready: bool, error: String, bytes: u64 }
static EXPORTS: std::sync::OnceLock<Mutex<std::collections::HashMap<String, ExportJob>>> = std::sync::OnceLock::new();
fn exports() -> &'static Mutex<std::collections::HashMap<String, ExportJob>> { EXPORTS.get_or_init(|| Mutex::new(Default::default())) }
fn export_dir(root: &std::path::Path) -> PathBuf { root.join("store/.export") }

#[derive(Deserialize)]
struct ExportIn { shas: Vec<String>, #[serde(default)] name: String }

/// zip書き出しジョブ: {name}/{sha1}.{ext} + {name}/meta/{sha1}.json(サイドカー=出典/ライセンス/編集履歴) + {name}/manifest.json
/// 無圧縮(JPEG/PNGは縮まない)・zip64。進捗は GET /api/export/{id}、完成品は GET /export/{id}/{name}.zip
async fn api_export(State(app): S, Json(i): Json<ExportIn>) -> impl IntoResponse {
    if i.shas.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": "書き出す画像がありません"}))).into_response();
    }
    let raw = i.name.trim();
    let name: String = if raw.is_empty() { format!("fluent_gallery_{}", chrono_like_now()) }
        else { raw.chars().map(|c| if "/\\:*?\"<>|".contains(c) { '_' } else { c }).collect() };
    let id = format!("{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    let dir = export_dir(&app.root);
    let _ = std::fs::create_dir_all(&dir);
    // 1日より古い書き出しは片付ける(ダウンロード済みの残骸)
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            if e.metadata().and_then(|m| m.modified()).map(|t| t.elapsed().map(|d| d.as_secs() > 86400).unwrap_or(false)).unwrap_or(false) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    exports().lock().unwrap().insert(id.clone(), ExportJob { name: name.clone(), total: i.shas.len(), done: 0, ready: false, error: String::new(), bytes: 0 });
    // zip内のフォルダ名は ASCII に限定(日本語だと macOS の unzip 等で文字化けして見える)。ダウンロード名(filename*)には日本語を残す
    let inner: String = if name.is_ascii() { name.clone() } else { format!("fluent_gallery_{}", chrono_like_now()) };
    let (root, id2, name2, shas) = (app.root.clone(), id.clone(), inner, i.shas);
    let name_title = name.clone();
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let path = export_dir(&root).join(format!("{id2}.zip"));
        let r = (|| -> Result<u64, String> {
            let f = std::fs::File::create(&path).map_err(|e| e.to_string())?;
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored).large_file(true);
            let mut manifest = Vec::new();
            for sha in &shas {
                let Some(m) = store::load_meta(&root, sha) else { continue };
                let ext = m["ext"].as_str().unwrap_or("png").to_string();
                let Ok(data) = std::fs::read(store::image_path(&root, sha, &ext)) else { continue };
                zw.start_file(format!("{name2}/{sha}.{ext}"), opts).map_err(|e| e.to_string())?;
                zw.write_all(&data).map_err(|e| e.to_string())?;
                zw.start_file(format!("{name2}/meta/{sha}.json"), opts).map_err(|e| e.to_string())?;
                zw.write_all(serde_json::to_string_pretty(&m).unwrap_or_default().as_bytes()).map_err(|e| e.to_string())?;
                manifest.push(json!({"sha1": sha, "file": format!("{sha}.{ext}"), "source": m["source"], "rights": m["rights"],
                                     "credit": m["credit"], "title": m["crawl"]["title"], "landing": m["crawl"]["landing"], "url": m["crawl"]["url"]}));
                if let Some(j) = exports().lock().unwrap().get_mut(&id2) { j.done += 1; }
            }
            zw.start_file(format!("{name2}/manifest.json"), opts).map_err(|e| e.to_string())?;
            let mf = json!({"app": "fluent_gallery", "version": env!("CARGO_PKG_VERSION"), "name": name2, "title": name_title,
                            "exported_at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
                            "count": manifest.len(), "items": manifest,
                            "note": "meta/*.json はサイドカー(出典・ライセンス・編集履歴・顔ID等)。このzipをそのまま取り込みにドロップすれば復元される"});
            zw.write_all(serde_json::to_string_pretty(&mf).unwrap_or_default().as_bytes()).map_err(|e| e.to_string())?;
            zw.finish().map_err(|e| e.to_string())?;
            Ok(std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0))
        })();
        let mut ex = exports().lock().unwrap();
        if let Some(j) = ex.get_mut(&id2) {
            match r { Ok(b) => { j.ready = true; j.bytes = b; } Err(e) => { j.error = e; let _ = std::fs::remove_file(&path); } }
        }
    });
    Json(json!({"ok": true, "id": id, "name": name})).into_response()
}

/// 日時スタンプ(依存なし): YYYYMMDD_HHMMSS(UTC)
fn chrono_like_now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let days = secs.div_euclid(86400); let rem = secs.rem_euclid(86400);
    // 1970-01-01 起点の日数→暦(civil_from_days)
    let z = days + 719468; let era = z.div_euclid(146097); let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1; let m = if mp < 10 { mp + 3 } else { mp - 9 }; let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}_{:02}{:02}{:02}", rem / 3600, (rem % 3600) / 60, rem % 60)
}

async fn api_export_status(AxPath(id): AxPath<String>) -> impl IntoResponse {
    match exports().lock().unwrap().get(&id) {
        Some(j) => Json(json!(j)).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// 完成した zip をストリーム配信(数GBでもメモリに載せない)
async fn export_zip(State(app): S, AxPath((id, fname)): AxPath<(String, String)>) -> impl IntoResponse {
    let ready = exports().lock().unwrap().get(&id).map(|j| j.ready).unwrap_or(false);
    let path = export_dir(&app.root).join(format!("{id}.zip"));
    if !ready || !path.exists() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(f) = tokio::fs::File::open(&path).await else { return StatusCode::NOT_FOUND.into_response() };
    let len = f.metadata().await.map(|m| m.len()).unwrap_or(0);
    let body = axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(f));
    ([(header::CONTENT_TYPE, "application/zip".to_string()), (header::CONTENT_DISPOSITION, attachment(&fname)),
      (header::CONTENT_LENGTH, len.to_string())], body).into_response()
}

/// 条件に合う全画像の sha1 だけ(上限なし)。フォルダ/取り込み元の一括書き出し用
async fn api_images_shas(State(app): S, Query(q): Query<Q>) -> Json<Value> {
    let (cond, args) = build_where(&q);
    let db = app.db.lock().unwrap();
    let params: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let shas: Vec<String> = db
        .prepare(&format!("SELECT sha1 FROM images WHERE {cond} ORDER BY ingested DESC, sha1 DESC"))
        .and_then(|mut st| st.query_map(params.as_slice(), |r| r.get::<_, String>(0)).map(|rows| rows.flatten().collect()))
        .unwrap_or_default();
    Json(json!({"total": shas.len(), "shas": shas}))
}

async fn thumb(State(app): S, AxPath(sha1): AxPath<String>) -> impl IntoResponse {
    app.touch_ui();
    let jpg = store::thumb_path(&app.root, &sha1);
    if let Ok(b) = std::fs::read(&jpg) {
        return ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, IMMUTABLE)], b).into_response();
    }
    // 旧webpキャッシュ or その場生成(ingest時焼きが基本なのでここは稀)
    let webp = app.root.join("store/thumbs").join(store::shard(&sha1)).join(format!("{sha1}.webp"));
    if let Ok(b) = std::fs::read(&webp) {
        return ([(header::CONTENT_TYPE, "image/webp"), (header::CACHE_CONTROL, IMMUTABLE)], b).into_response();
    }
    let Some(m) = store::load_meta(&app.root, &sha1) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let src = store::image_path(&app.root, &sha1, m["ext"].as_str().unwrap_or("png"));
    let out = tokio::task::spawn_blocking(move || -> Option<Vec<u8>> {
        let im = image::open(&src).ok()?.thumbnail(360, 360).into_rgb8();
        let mut buf = std::io::Cursor::new(Vec::new());
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 82).encode_image(&im).ok()?;
        std::fs::create_dir_all(jpg.parent()?).ok()?;
        let _ = std::fs::write(&jpg, buf.get_ref());
        Some(buf.into_inner())
    })
    .await
    .ok()
    .flatten();
    match out {
        Some(b) => ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, IMMUTABLE)], b).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// CLIP埋め込みバックフィル(1バッチ)。thumb360から計算=軽い。読めない画像は空BLOB印で再挑戦しない
fn embed_backfill(root: &std::path::Path, batch: usize) -> usize {
    let Ok(db) = rusqlite::Connection::open(root.join("store/index.sqlite")) else {
        return 0;
    };
    let rows: Vec<(String, String)> = db
        .prepare("SELECT i.sha1, i.ext FROM images i LEFT JOIN embs e ON e.sha1=i.sha1 WHERE e.sha1 IS NULL LIMIT ?1")
        .and_then(|mut st| {
            st.query_map([batch as i64], |r| Ok((r.get(0)?, r.get(1)?)))
                .map(|rs| rs.filter_map(Result::ok).collect())
        })
        .unwrap_or_default();
    if rows.is_empty() {
        return 0;
    }
    let mut done = 0usize;
    for (sha, ext) in rows {
        let img = image::open(store::thumb_path(root, &sha))
            .or_else(|_| image::open(store::image_path(root, &sha, &ext)));
        let Ok(img) = img else {
            let _ = db.execute("INSERT OR REPLACE INTO embs(sha1, emb) VALUES(?1, x'')", [&sha]);
            continue;
        };
        match onnx::embed(root, &img) {
            Some(e) => {
                let b: Vec<u8> = e.iter().flat_map(|f| f.to_le_bytes()).collect();
                let _ = db.execute("INSERT OR REPLACE INTO embs(sha1, emb) VALUES(?2, ?1)", rusqlite::params![b, sha]);
                done += 1;
                // 全力で回すとUI/収集が重くなる。1枚ごとに小休止=裏方の速度に落とす
                std::thread::sleep(std::time::Duration::from_millis(8));
            }
            None => return done, // モデル読めず — 次周期に任せる
        }
    }
    done
}

/// マイクロ段(120px) — 俯瞰表示用。360サムネを更に縮めてデコード費を1/10にする
/// (小セルで360を1500枚デコードするとブラウザが止まる問題の根治 2026-09-03)。
/// 基本はingest時焼き+backfill済みでhitする。missは非常用でsingle-flight(同一SHAの同時生成を1回に)。
async fn micro(State(app): S, AxPath(sha1): AxPath<String>) -> impl IntoResponse {
    app.touch_ui();
    let p = store::micro_path(&app.root, &sha1);
    if let Ok(b) = std::fs::read(&p) {
        return ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, IMMUTABLE)], b).into_response();
    }
    // 既に他リクエストが同じSHAを焼いている最中なら、ファイルが現れるのを待つ
    if !app.micro_inflight.lock().unwrap().insert(sha1.clone()) {
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if let Ok(b) = std::fs::read(&p) {
                return ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, IMMUTABLE)], b)
                    .into_response();
            }
        }
        return StatusCode::NOT_FOUND.into_response();
    }
    let root = app.root.clone();
    let pw = p.clone();
    let sha = sha1.clone();
    let out = tokio::task::spawn_blocking(move || -> Option<Vec<u8>> {
        let im = image::open(store::thumb_path(&root, &sha)).ok().or_else(|| {
            let m = store::load_meta(&root, &sha)?;
            image::open(store::image_path(&root, &sha, m["ext"].as_str().unwrap_or("png"))).ok()
        })?;
        let th = im.thumbnail(120, 120).into_rgb8();
        let mut buf = std::io::Cursor::new(Vec::new());
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 72).encode_image(&th).ok()?;
        std::fs::create_dir_all(pw.parent()?).ok()?;
        let _ = std::fs::write(&pw, buf.get_ref());
        Some(buf.into_inner())
    })
    .await
    .ok()
    .flatten();
    app.micro_inflight.lock().unwrap().remove(&sha1);
    match out {
        Some(b) => ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, IMMUTABLE)], b).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize, Default)]
struct AtlasQ {
    #[serde(default)]
    fit: u8, // 0=cover / 1=contain
}

fn load_atlas_manifest(root: &Path, key: &str) -> Option<AtlasManifest> {
    let bytes = std::fs::read(atlas_manifest_path(root, key)).ok()?;
    if bytes.len() > 128 * 1024 {
        return None;
    }
    let manifest: AtlasManifest = serde_json::from_slice(&bytes).ok()?;
    if manifest.version != ATLAS_VERSION
        || manifest.items.is_empty()
        || manifest.items.len() > ATLAS_MAX_ITEMS
        || manifest.items.iter().any(|m| !valid_sha1(&m.sha1) || m.erev.len() > 128)
        || atlas_key(&manifest.items) != key
    {
        return None;
    }
    Some(manifest)
}

fn atlas_member_image(root: &Path, item: &AtlasMember) -> Option<image::DynamicImage> {
    if item.erev.is_empty() {
        if let Ok(im) = image::open(store::micro_path(root, &item.sha1))
            .or_else(|_| image::open(store::thumb_path(root, &item.sha1)))
        {
            return Some(im);
        }
        let meta = store::load_meta(root, &item.sha1)?;
        return image::open(store::image_path(
            root,
            &item.sha1,
            meta["ext"].as_str().unwrap_or("jpg"),
        ))
        .ok();
    }
    // 編集直後はmicroの非同期焼き直しよりatlas要求が先に来得る。新revのURLへ旧画像を
    // immutable保存しないよう、編集履歴からこの場で正しい120px像を得る。
    let meta = store::load_meta(root, &item.sha1)?;
    let history = meta.get("edits").cloned().unwrap_or_else(|| json!([]));
    if edits::rev(&history) != item.erev {
        // 古い索引でerevだけがずれた既存データも、200枚すべてをatlas不可にはしない。
        // 現在の正本(history)を描き、content keyのファイルは最初のatomic publish後に不変。
        // 次に索引が更新されれば正しいrevの別keyへ自然に切り替わる。
        eprintln!("atlas: stale edit revision for {}", item.sha1);
    }
    let ext = meta["ext"].as_str().unwrap_or("jpg");
    let bytes = edits::render(root, &item.sha1, ext, &history, ATLAS_TILE, None)?;
    image::load_from_memory(&bytes).ok()
}

fn generate_atlas(root: &Path, key: &str, fit: bool) -> Option<Vec<u8>> {
    let manifest = load_atlas_manifest(root, key)?;
    let rows = manifest.items.len().div_ceil(ATLAS_COLS) as u32;
    let panel = image::Rgb([0x14, 0x15, 0x1a]);
    let mut sheet = image::RgbImage::from_pixel(ATLAS_COLS as u32 * ATLAS_TILE, rows * ATLAS_TILE, panel);
    for (i, item) in manifest.items.iter().enumerate() {
        // 1枚でも欠けたsheetをimmutable公開すると、その黒タイルを永久キャッシュしてしまう。
        let im = atlas_member_image(root, item)?;
        let col = i as u32 % ATLAS_COLS as u32;
        let row = i as u32 / ATLAS_COLS as u32;
        if fit {
            let tile = im.resize(ATLAS_TILE, ATLAS_TILE, image::imageops::FilterType::Triangle).into_rgb8();
            let x = col * ATLAS_TILE + (ATLAS_TILE - tile.width()) / 2;
            let y = row * ATLAS_TILE + (ATLAS_TILE - tile.height()) / 2;
            image::imageops::replace(&mut sheet, &tile, i64::from(x), i64::from(y));
        } else {
            let tile = im
                .resize_to_fill(ATLAS_TILE, ATLAS_TILE, image::imageops::FilterType::Triangle)
                .into_rgb8();
            image::imageops::replace(
                &mut sheet,
                &tile,
                i64::from(col * ATLAS_TILE),
                i64::from(row * ATLAS_TILE),
            );
        }
    }
    let mut buf = std::io::Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, ATLAS_QUALITY)
        .encode_image(&sheet)
        .ok()?;
    let bytes = buf.into_inner();
    let _ = atomic_publish(&atlas_image_path(root, key, fit), &bytes);
    Some(bytes)
}

/// 36pxグリッド用contact sheet。1 request/1 decodeで最大200セルを満たす。
async fn atlas(State(app): S, AxPath(key): AxPath<String>, Query(q): Query<AtlasQ>) -> impl IntoResponse {
    if !valid_sha1(&key) || q.fit > 1 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    app.touch_ui();
    let fit = q.fit == 1;
    let path = atlas_image_path(&app.root, &key, fit);
    if let Ok(bytes) = std::fs::read(&path) {
        return ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, IMMUTABLE)], bytes)
            .into_response();
    }
    if !atlas_manifest_path(&app.root, &key).exists() {
        return StatusCode::NOT_FOUND.into_response();
    }

    let flight = format!("{key}:{}", q.fit);
    let owner = { app.atlas_inflight.lock().unwrap().insert(flight.clone()) };
    if !owner {
        // 同じsheetを別requestが生成中。atomic publish後の完成ファイルだけを読む。
        for _ in 0..500 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if let Ok(bytes) = std::fs::read(&path) {
                return ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, IMMUTABLE)], bytes)
                    .into_response();
            }
        }
        return StatusCode::NOT_FOUND.into_response();
    }

    let root = app.root.clone();
    let worker_key = key.clone();
    let out = tokio::task::spawn_blocking(move || generate_atlas(&root, &worker_key, fit))
        .await
        .ok()
        .flatten();
    app.atlas_inflight.lock().unwrap().remove(&flight);
    match out {
        Some(bytes) => ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, IMMUTABLE)], bytes)
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------- 非破壊調整(M4): 原本不変・editsはサイドカーの履歴スタック ----------

async fn api_edits_get(State(app): S, AxPath(sha1): AxPath<String>) -> impl IntoResponse {
    let Some(m) = store::load_meta(&app.root, &sha1) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let e = m.get("edits").cloned().unwrap_or_else(|| json!([]));
    Json(json!({"edits": e, "rev": edits::rev(&e)})).into_response()
}

#[derive(Deserialize)]
struct EditIn {
    action: String, // push | pop | clear
    #[serde(default)] edit: Value,
}

async fn api_edits_put(State(app): S, AxPath(sha1): AxPath<String>, Json(e): Json<EditIn>) -> impl IntoResponse {
    let Some(mut m) = store::load_meta(&app.root, &sha1) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut list = m.get("edits").and_then(|v| v.as_array().cloned()).unwrap_or_default();
    match e.action.as_str() {
        "push" => {
            if e.edit["op"].as_str().is_none() {
                return (StatusCode::BAD_REQUEST, Json(json!({"detail": "edit.op がありません"}))).into_response();
            }
            let mut ed = e.edit;
            ed["ts"] = json!(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64());
            list.push(ed);
        }
        "pop" => { list.pop(); }
        "clear" => list.clear(),
        _ => return (StatusCode::BAD_REQUEST, Json(json!({"detail": "action は push/pop/clear"}))).into_response(),
    }
    m["edits"] = json!(list);
    if store::save_meta(&app.root, &m).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    edits::clear_renders(&app.root, &sha1); // 履歴が変わればレンダは捨てる(全て再生成可能)
    store::index_meta(&app.db.lock().unwrap(), &m); // erev(サムネ版)を索引へ
    // サムネ/プレビューも編集後の見た目に焼き直す(グリッドが原本のままにならないように)
    {
        let root = app.root.clone();
        let sha = sha1.clone();
        let ext = m["ext"].as_str().unwrap_or("jpg").to_string();
        let ed = m["edits"].clone();
        tokio::task::spawn_blocking(move || {
            if let Some(pv) = edits::render(&root, &sha, &ext, &ed, 1080, None) {
                let _ = std::fs::write(store::preview_path(&root, &sha), &pv);
                if let Ok(img) = image::load_from_memory(&pv) {
                    // 360だけ書き換えると小型グリッドが編集前のmicroを出し続ける。
                    // 共通helperで360+120を同じrevの見た目へ同時更新する。
                    store::write_thumbs(&root, &sha, &img);
                }
            }
        });
    }
    let e = m["edits"].clone();
    Json(json!({"edits": e, "rev": edits::rev(&e)})).into_response()
}

#[derive(Deserialize)]
struct RenderQ {
    #[serde(default)] w: u32,
    #[serde(default)] v: String, // クライアントがrevをURLに入れてキャッシュを効かせる(サーバは常に現行revで応える)
    #[serde(default)] seg: String, // "1"=マスク輪郭を焼いて返す
}

async fn render_img(State(app): S, AxPath(sha1): AxPath<String>, Query(rq): Query<RenderQ>) -> impl IntoResponse {
    let _ = &rq.v;
    let Some(m) = store::load_meta(&app.root, &sha1) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let e = m.get("edits").cloned().unwrap_or_else(|| json!([]));
    let ext = m["ext"].as_str().unwrap_or("png").to_string();
    let seg_shapes = (rq.seg == "1").then(|| m["seg"]["shapes"].clone()).filter(|s| s.is_array());
    // 履歴なし・原寸要求・マスク無しなら原本をそのまま(コピーゼロ)
    if e.as_array().map(|a| a.is_empty()).unwrap_or(true) && rq.w == 0 && seg_shapes.is_none() {
        return match std::fs::read(store::image_path(&app.root, &sha1, &ext)) {
            Ok(b) => ([(header::CONTENT_TYPE, mime(&ext)), (header::CACHE_CONTROL, IMMUTABLE)], b).into_response(),
            Err(_) => StatusCode::NOT_FOUND.into_response(),
        };
    }
    let root = app.root.clone();
    let w = rq.w;
    let out = tokio::task::spawn_blocking(move || edits::render(&root, &sha1, &ext, &e, w, seg_shapes.as_ref()))
        .await
        .ok()
        .flatten();
    match out {
        Some(b) => ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, IMMUTABLE)], b).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// 切り抜きPNG(透過・フェザー・編集適用済み) — 「背景なかったことにする」
async fn cutout(State(app): S, AxPath(sha1): AxPath<String>) -> impl IntoResponse {
    let Some(m) = store::load_meta(&app.root, &sha1) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let shapes = m["seg"]["shapes"].clone();
    if !shapes.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": "マスクがありません(先にマスク生成)"}))).into_response();
    }
    let ext = m["ext"].as_str().unwrap_or("jpg").to_string();
    let edits = m.get("edits").cloned().unwrap_or_else(|| json!([]));
    let root = app.root.clone();
    let out = tokio::task::spawn_blocking(move || edits::cutout_png(&root, &sha1, &ext, &edits, &shapes, 2048))
        .await
        .ok()
        .flatten();
    match out {
        Some(b) => ([(header::CONTENT_TYPE, "image/png"),
                     (header::CONTENT_DISPOSITION, "inline; filename=\"cutout.png\""),
                     (header::CACHE_CONTROL, "no-cache")], b).into_response(),
        None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// preview段(長辺1080) — ライトボックスを待たせない中間サムネ。ingest時に焼き、無ければその場生成。
async fn preview(State(app): S, AxPath(sha1): AxPath<String>) -> impl IntoResponse {
    let p = store::preview_path(&app.root, &sha1);
    if let Ok(b) = std::fs::read(&p) {
        return ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, IMMUTABLE)], b).into_response();
    }
    let Some(m) = store::load_meta(&app.root, &sha1) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let src = store::image_path(&app.root, &sha1, m["ext"].as_str().unwrap_or("png"));
    let out = tokio::task::spawn_blocking(move || -> Option<Vec<u8>> {
        let im = image::open(&src).ok()?.thumbnail(1080, 1080).into_rgb8();
        let mut buf = std::io::Cursor::new(Vec::new());
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 88).encode_image(&im).ok()?;
        std::fs::create_dir_all(p.parent()?).ok()?;
        let _ = std::fs::write(&p, buf.get_ref());
        Some(buf.into_inner())
    })
    .await
    .ok()
    .flatten();
    match out {
        Some(b) => ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, IMMUTABLE)], b).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------- ⭐keep(「これは！」保護) ----------

#[derive(Deserialize)]
struct KeepIn {
    shas: Vec<String>,
    keep: bool,
}

async fn api_keep(State(app): S, Json(k): Json<KeepIn>) -> impl IntoResponse {
    let mut n = 0;
    for sha1 in &k.shas {
        if let Some(mut m) = store::load_meta(&app.root, sha1) {
            m["keep"] = json!(k.keep);
            if store::save_meta(&app.root, &m).is_ok() {
                store::index_meta(&app.db.lock().unwrap(), &m);
                n += 1;
            }
        }
    }
    Json(json!({"ok": true, "updated": n, "keep": k.keep}))
}

// ---------- 削除(DEL一発)とundo — 即deleteせず.trash経由、30日はいつでも戻せる。AI/MCPからも同じ口 ----------

#[derive(Deserialize)]
struct ShasIn {
    shas: Vec<String>,
}

async fn api_trash(State(app): S, Json(k): Json<ShasIn>) -> impl IntoResponse {
    if k.shas.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": "shasが空です"}))).into_response();
    }
    // 専用接続+blockingスレッドで実行: グローバルDBロックを握ったままfs renameすると
    // 他API(一覧/facet)と数珠つなぎになり削除が数秒詰まる(連打スタックの真因の片方 2026-09-03)
    let root = app.root.clone();
    let shas = k.shas.clone();
    let n = tokio::task::spawn_blocking(move || {
        let db = Connection::open(root.join("store/index.sqlite")).ok()?;
        let _ = db.busy_timeout(std::time::Duration::from_secs(5));
        Some(store::trash_shas(&root, &db, &shas))
    })
    .await
    .ok()
    .flatten()
    .unwrap_or(0);
    Json(json!({"ok": true, "trashed": n, "undo": "POST /api/trash/restore に同じshasで戻せます(30日以内)"})).into_response()
}

/// ゴミ箱の中身一覧(30日で自動消滅する退避層を「見える」ようにする)
async fn api_trash_list(State(app): S) -> Json<Value> {
    let mut out = vec![];
    let now_day = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86400)
        .unwrap_or(0);
    if let Ok(rd) = std::fs::read_dir(app.root.join("store/.trash")) {
        for d in rd.flatten() {
            let dname = d.file_name().to_string_lossy().into_owned();
            let Some(day) = dname.strip_prefix('d').and_then(|s| s.parse::<u64>().ok()) else { continue };
            if let Ok(fs) = std::fs::read_dir(d.path()) {
                for f in fs.flatten() {
                    let p = f.path();
                    if p.extension().map(|e| e == "json").unwrap_or(false) {
                        if let Ok(m) = std::fs::read_to_string(&p).map(|t| serde_json::from_str::<Value>(&t)) {
                            if let Ok(m) = m {
                                out.push(json!({
                                    "sha1": m["sha1"], "ext": m["ext"], "w": m["w"], "h": m["h"],
                                    "source": m["source"], "tint": m["tint"],
                                    "days_left": 30i64.saturating_sub((now_day.saturating_sub(day)) as i64),
                                }));
                            }
                        }
                    }
                }
            }
        }
    }
    out.reverse(); // 新しく捨てた物を先頭に(日付dirの順で概ね)
    Json(json!(out))
}

/// ゴミ箱内の画像本体(日付dirを横断して探す・キャッシュ無し)
async fn trash_img(State(app): S, AxPath(sha1): AxPath<String>) -> impl IntoResponse {
    if sha1.len() < 3 || !sha1.chars().all(|c| c.is_ascii_hexdigit()) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Ok(rd) = std::fs::read_dir(app.root.join("store/.trash")) {
        for d in rd.flatten() {
            if let Ok(fs) = std::fs::read_dir(d.path()) {
                for f in fs.flatten() {
                    let p = f.path();
                    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    let ext = p.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
                    if stem == sha1 && ext != "json" {
                        if let Ok(b) = std::fs::read(&p) {
                            return ([(header::CONTENT_TYPE, mime(&ext)), (header::CACHE_CONTROL, "no-cache")], b)
                                .into_response();
                        }
                    }
                }
            }
        }
    }
    StatusCode::NOT_FOUND.into_response()
}

/// 取り込み元ごとゴミ箱へ(30日は戻せる)。件数が多いと時間がかかるのでblockingで
#[derive(Deserialize)]
struct SourceTrashIn {
    source: String,
}

async fn api_source_trash(State(app): S, Json(s): Json<SourceTrashIn>) -> impl IntoResponse {
    if s.source.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": "sourceが空です"}))).into_response();
    }
    let root = app.root.clone();
    let src = s.source.clone();
    let (n, shas) = tokio::task::spawn_blocking(move || {
        let db = Connection::open(root.join("store/index.sqlite")).unwrap();
        store::ensure_schema(&db);
        let shas: Vec<String> = db
            .prepare("SELECT sha1 FROM images WHERE source=?")
            .and_then(|mut st| {
                st.query_map([src.as_str()], |r| r.get::<_, String>(0)).map(|rs| rs.filter_map(Result::ok).collect())
            })
            .unwrap_or_default();
        let n = store::trash_shas(&root, &db, &shas);
        (n, shas)
    })
    .await
    .unwrap_or((0, vec![]));
    let undo = if shas.len() <= 2000 { json!(shas) } else { json!(null) };
    Json(json!({"ok": true, "trashed": n, "shas": undo})).into_response()
}

/// D&D移動: 画像のsourceを付け替える(フォルダ=source条件のスマートフォルダなので移動=source変更)。
/// DB行とmetaファイル両方を書く(正本はmeta、DBは索引 — 片方だけだとrebuildで巻き戻る)
#[derive(Deserialize)]
struct MoveIn {
    shas: Vec<String>,
    source: String,
}

async fn api_move(State(app): S, Json(p): Json<MoveIn>) -> impl IntoResponse {
    let src = p.source.trim().to_string();
    if src.is_empty() || p.shas.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": "sourceとshasが必要です"}))).into_response();
    }
    let root = app.root.clone();
    let n = tokio::task::spawn_blocking(move || {
        let db = Connection::open(root.join("store/index.sqlite")).unwrap();
        store::ensure_schema(&db);
        let origin = store::infer_origin(&src);
        let mut n = 0usize;
        for sha in &p.shas {
            let hit = db
                .execute("UPDATE images SET source=?1, origin=?2 WHERE sha1=?3",
                         rusqlite::params![src, origin, sha])
                .unwrap_or(0);
            if hit > 0 {
                if let Some(mut m) = store::load_meta(&root, sha) {
                    m["source"] = json!(src);
                    m["origin"] = json!(origin);
                    let _ = store::save_meta(&root, &m);
                }
                n += 1;
            }
        }
        n
    })
    .await
    .unwrap_or(0);
    Json(json!({"ok": true, "moved": n})).into_response()
}

// ==== 顔ID(docs/face-id-design.md 2026-09-03) ====
/// 顔IDモデル(buffalo_l 288MB・非商用限定)の初回取得。進捗は /api/ai/status の faceid
#[cfg(feature = "faceid")]
async fn api_faces_pull(State(app): S) -> Json<Value> {
    let client = app.http.clone();
    tokio::spawn(async move {
        if let Err(e) = faceid::ensure_models(&client).await {
            println!("🧭 顔IDモデルDL失敗: {e}");
        }
    });
    Json(json!({"ok": true, "note": "進捗は GET /api/ai/status"}))
}

#[cfg(feature = "faceid")]
#[derive(Deserialize)]
struct FaceEnrollIn {
    album: String,
    person: String,
    shas: Vec<String>,
    #[serde(default)] point: Option<[f32; 2]>, // 正規化座標(0-1)。指定=その点に一番近い顔を登録(2ショット対応)
}

#[cfg(feature = "faceid")]
#[derive(Deserialize)]
struct FaceDetectIn {
    sha1: String,
}

#[cfg(feature = "faceid")]
/// 画像内の顔位置+登録台帳との照合結果を返す。
/// person=本人確定(FACE_SAME以上) / maybe=似ている(中間帯) / どちらも無し=未登録or別人
async fn api_faces_detect(State(app): S, Json(p): Json<FaceDetectIn>) -> impl IntoResponse {
    let root = app.root.clone();
    let out = tokio::task::spawn_blocking(move || -> Option<Value> {
        let db = Connection::open(root.join("store/index.sqlite")).ok()?;
        let (ext, source): (String, String) = db
            .query_row("SELECT ext, source FROM images WHERE sha1=?1", [p.sha1.as_str()],
                       |r| Ok((r.get(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default())))
            .unwrap_or(("jpg".into(), String::new()));
        let album = source.strip_prefix("crawl:").unwrap_or("").to_string();
        let refs = store::face_refs(&db, &album);
        // 顔の位置/埋め込みは画像につき一度だけ計算してimg_facesへ永続(2回目以降は数ms)。
        // 台帳と独立な生データなので人物登録の増減で無効化不要。idx=-1空行=顔なし済みの印
        let mut stored: Vec<(String, Vec<u8>)> = db
            .prepare("SELECT bbox, emb FROM img_faces WHERE sha1=?1 ORDER BY idx")
            .and_then(|mut st| {
                st.query_map([&p.sha1], |r| Ok((r.get(0)?, r.get(1)?))).map(|rs| rs.flatten().collect())
            })
            .unwrap_or_default();
        if stored.is_empty() {
            // モデル未取得のまま「顔なし」を永続化すると、取得後も二度と再検出されない(2026-09-04 指摘)→ 書かずに空を返す
            if !faceid::models_present() {
                return Some(json!({"faces": [], "note": "顔IDモデル未取得"}));
            }
            let img = image::open(store::image_path(&root, &p.sha1, &ext)).ok()?;
            let (w, h) = (img.width() as f32, img.height() as f32);
            let faces = faceid::detect_faces(&img);
            if faces.is_empty() {
                let _ = db.execute("INSERT OR REPLACE INTO img_faces VALUES(?1, -1, '', x'')", [&p.sha1]);
            }
            for (i, f) in faces.iter().take(8).enumerate() {
                let bb = json!([f.bbox[0] / w, f.bbox[1] / h, f.bbox[2] / w, f.bbox[3] / h]).to_string();
                let eb: Vec<u8> = faceid::embed_face(&img, &f.kps)
                    .map(|e| e.iter().flat_map(|x| x.to_le_bytes()).collect())
                    .unwrap_or_default();
                let _ = db.execute("INSERT OR REPLACE INTO img_faces VALUES(?1, ?2, ?3, ?4)",
                                   rusqlite::params![p.sha1, i as i64, bb, eb]);
                stored.push((bb, eb));
            }
        }
        Some(json!({"faces": stored.iter().filter(|(b, _)| !b.is_empty()).map(|(bb, eb)| {
            let bbox: Vec<f32> = serde_json::from_str(bb).unwrap_or_default();
            let e: Vec<f32> = eb.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
            let (mut who, mut best) = (None::<String>, -1.0f32);
            if !refs.is_empty() && e.len() >= 512 {
                for (nm, rs) in &refs {
                    let s = faceid::best_sim(&e, rs);
                    if s > best { best = s; who = Some(nm.clone()); }
                }
            }
            json!({
                "bbox": bbox,
                "person": who.as_ref().filter(|_| best >= faceid::FACE_SAME),
                "maybe": who.as_ref().filter(|_| best >= faceid::FACE_DIFF && best < faceid::FACE_SAME),
                "sim": (best * 100.0).round() / 100.0,
            })
        }).collect::<Vec<_>>()}))
    })
    .await
    .ok()
    .flatten();
    match out {
        Some(v) => Json(v).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(feature = "faceid")]
async fn api_faces_enroll(State(app): S, Json(p): Json<FaceEnrollIn>) -> impl IntoResponse {
    let person = p.person.trim().to_lowercase();
    if person.is_empty() || p.shas.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": "personとshasが必要です"}))).into_response();
    }
    let root = app.root.clone();
    let (ok, failed) = tokio::task::spawn_blocking(move || {
        let db = Connection::open(root.join("store/index.sqlite")).unwrap();
        store::ensure_schema(&db);
        let mut ok = 0usize;
        let mut failed: Vec<String> = vec![];
        for sha in &p.shas {
            let ext: String = db
                .query_row("SELECT ext FROM images WHERE sha1=?1", [sha.as_str()], |r| r.get(0))
                .unwrap_or_else(|_| "jpg".into());
            let Ok(img) = image::open(store::image_path(&root, sha, &ext)) else {
                failed.push(sha.clone());
                continue;
            };
            let faces = faceid::detect_faces(&img);
            // point指定=その点に中心が一番近い顔(2ショットでクリック選択)。無指定=最大の顔
            let pick = if let Some([px, py]) = p.point {
                let (w, h) = (img.width() as f32, img.height() as f32);
                faces.iter().min_by(|a, b| {
                    let d = |f: &faceid::Face| {
                        ((f.bbox[0] + f.bbox[2]) / 2.0 / w - px).powi(2)
                            + ((f.bbox[1] + f.bbox[3]) / 2.0 / h - py).powi(2)
                    };
                    d(a).partial_cmp(&d(b)).unwrap_or(std::cmp::Ordering::Equal)
                })
            } else {
                faces.iter().max_by(|a, b| {
                    let ar = (a.bbox[2] - a.bbox[0]) * (a.bbox[3] - a.bbox[1]);
                    let br = (b.bbox[2] - b.bbox[0]) * (b.bbox[3] - b.bbox[1]);
                    ar.partial_cmp(&br).unwrap_or(std::cmp::Ordering::Equal)
                })
            };
            let Some(f) = pick else {
                failed.push(sha.clone());
                continue;
            };
            let Some(emb) = faceid::embed_face(&img, &f.kps) else {
                failed.push(sha.clone());
                continue;
            };
            let blob: Vec<u8> = emb.iter().flat_map(|x| x.to_le_bytes()).collect();
            let _ = db.execute(
                "INSERT OR REPLACE INTO faces VALUES(?1,?2,?3,?4,?5)",
                rusqlite::params![person, p.album, sha, blob, format!("{:?}", f.bbox)],
            );
            ok += 1;
        }
        (ok, failed)
    })
    .await
    .unwrap_or((0, vec![]));
    Json(json!({"ok": true, "enrolled": ok, "failed": failed})).into_response()
}

#[cfg(feature = "faceid")]
#[derive(Deserialize)]
struct FacesQ {
    #[serde(default)] album: String,
}

#[cfg(feature = "faceid")]
async fn api_faces_list(State(app): S, axum::extract::Query(q): axum::extract::Query<FacesQ>) -> impl IntoResponse {
    let db = app.db.lock().unwrap();
    let mut rows: Vec<(String, String, String)> = vec![];
    if let Ok(mut st) = db.prepare(
        "SELECT album, person, GROUP_CONCAT(sha1) FROM faces WHERE album=?1 OR ?1='' GROUP BY album, person ORDER BY album, person",
    ) {
        if let Ok(rs) = st.query_map([q.album.as_str()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        }) {
            rows = rs.filter_map(Result::ok).collect();
        }
    }
    Json(json!(rows
        .iter()
        .map(|(a, p, shas)| {
            let s: Vec<&str> = shas.split(',').collect();
            json!({"album": a, "person": p, "refs": s.len(), "shas": s})
        })
        .collect::<Vec<_>>()))
}

#[cfg(feature = "faceid")]
#[derive(Deserialize)]
struct FaceDelIn {
    album: String,
    person: String,
    #[serde(default)] sha1: Option<String>, // 指定=その参照顔1枚だけ削除、無指定=人物ごと削除
}

#[cfg(feature = "faceid")]
async fn api_faces_delete(State(app): S, Json(p): Json<FaceDelIn>) -> impl IntoResponse {
    let db = app.db.lock().unwrap();
    let n = match &p.sha1 {
        Some(s) => db
            .execute("DELETE FROM faces WHERE album=?1 AND person=?2 AND sha1=?3",
                     [p.album.as_str(), p.person.as_str(), s.as_str()])
            .unwrap_or(0),
        None => db
            .execute("DELETE FROM faces WHERE album=?1 AND person=?2", [p.album.as_str(), p.person.as_str()])
            .unwrap_or(0),
    };
    Json(json!({"ok": true, "deleted": n}))
}

#[cfg(feature = "faceid")]
#[derive(Deserialize)]
struct FaceScanIn {
    album: String,
}

#[cfg(feature = "faceid")]
/// 遡及スキャン: フォルダ全画像を顔照合し、本人タグ付与+集計を返す(非破壊)
async fn api_faces_scan(State(app): S, Json(p): Json<FaceScanIn>) -> impl IntoResponse {
    let root = app.root.clone();
    let album = p.album.clone();
    let out = tokio::task::spawn_blocking(move || {
        let db = Connection::open(root.join("store/index.sqlite")).unwrap();
        store::ensure_schema(&db);
        let refs = store::face_refs(&db, &album);
        if refs.is_empty() {
            return json!({"detail": "このフォルダに登録メンバーがいません"});
        }
        let shas: Vec<(String, String)> = db
            .prepare("SELECT sha1, ext FROM images WHERE source=?1")
            .and_then(|mut st| st.query_map([format!("crawl:{album}")], |r| Ok((r.get(0)?, r.get(1)?)))
                .map(|rs| rs.filter_map(Result::ok).collect()))
            .unwrap_or_default();
        let mut per: std::collections::HashMap<String, usize> = Default::default();
        let (mut noface, mut mismatch) = (0usize, 0usize);
        for (sha, ext) in &shas {
            let Ok(img) = image::open(store::image_path(&root, sha, ext)) else { continue };
            let faces = faceid::detect_faces(&img);
            if faces.is_empty() {
                noface += 1;
                continue;
            }
            let mut best = -1.0f32;
            let mut who: Option<&str> = None;
            for f in faces.iter().take(4) {
                if let Some(e) = faceid::embed_face(&img, &f.kps) {
                    for (name, rs) in &refs {
                        let sim = faceid::best_sim(&e, rs);
                        if sim > best {
                            best = sim;
                            who = Some(name);
                        }
                    }
                }
            }
            if best >= faceid::FACE_SAME {
                let w = who.unwrap().to_string();
                let _ = db.execute("INSERT OR IGNORE INTO tags VALUES(?1,?2)", [sha.as_str(), w.as_str()]);
                // meta側にも永続化(rebuildで消えないように)
                if let Some(mut m) = store::load_meta(&root, sha) {
                    let mut tags: Vec<String> = m["crawl"]["tags"].as_array().map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect()).unwrap_or_default();
                    if !tags.contains(&w) {
                        tags.push(w.clone());
                        m["crawl"]["tags"] = json!(tags);
                        let _ = store::save_meta(&root, &m);
                    }
                }
                *per.entry(w).or_default() += 1;
            } else if best < faceid::FACE_DIFF {
                mismatch += 1;
            }
        }
        json!({"ok": true, "scanned": shas.len(), "matched": per, "no_face": noface, "mismatch": mismatch})
    })
    .await
    .unwrap_or_else(|_| json!({"detail": "scan失敗"}));
    Json(out).into_response()
}

async fn api_trash_restore(State(app): S, Json(k): Json<ShasIn>) -> impl IntoResponse {
    let db = app.db.lock().unwrap();
    let n = store::restore_shas(&app.root, &db, &k.shas);
    Json(json!({"ok": true, "restored": n}))
}

// ---------- 動的アルバム(条件を保存、メンバーは常に今の検索結果=自動更新) ----------

fn album_dir(root: &std::path::Path) -> PathBuf {
    root.join("store/albums")
}

fn album_slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .take(48)
        .collect();
    if s.is_empty() { "album".into() } else { s }
}

fn load_albums(root: &std::path::Path) -> Vec<Value> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(album_dir(root)) {
        for e in rd.flatten() {
            if let Ok(t) = std::fs::read_to_string(e.path()) {
                if let Ok(a) = serde_json::from_str::<Value>(&t) {
                    out.push(a);
                }
            }
        }
    }
    out.sort_by(|a, b| b["created"].as_f64().partial_cmp(&a["created"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// フォルダパス("a/b/c")を安全化。ネスト自由、空要素は捨てる
fn folder_norm(f: &str) -> String {
    f.split('/')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && *s != "." && *s != "..")
        .collect::<Vec<_>>()
        .join("/")
        .chars()
        .take(120)
        .collect()
}

#[derive(Deserialize)]
struct AlbumIn {
    name: String,
    criteria: Value, // /api/images と同じパラメータ集合
    #[serde(default)] folder: String, // "植物/病害" のようなネスト可のフォルダパス
    #[serde(default)] goal: String, // AIフォルダの目標宣言(例「可愛い犬の画像を1000枚、実写優先」)。
                                    // コピー→書き換えでエージェントのレシピごと増殖する(常駐キュレーターの器)
    #[serde(default)] agent: Value, // {auto: bool, target: 枚数} — 置いとくと自動で増える(オートパイロット)
    #[serde(default)] keywords: Vec<String>, // 手動キーワード(LLM生成より優先で検索に使う)
    #[serde(default)] engines: Vec<String>, // 検索元の選択(空=全部)
    #[serde(default)] kind: String, // ""|"crawl"=収集 / "gen"=AI生成(docs/gen-design.md)。空なら既存を引き継ぐ
    #[serde(default)] recipe: Value, // 生成レシピ {size, steps, ...}。object 以外なら既存を引き継ぐ
}

async fn api_album_make(State(app): S, Json(a): Json<AlbumIn>) -> impl IntoResponse {
    let slug = album_slug(&a.name);
    let dir = album_dir(&app.root);
    // 上書き保存なので、UI の部分更新(自動保存・スイッチ)が送ってこない項目は既存から引き継ぐ
    let prev = std::fs::read_to_string(dir.join(format!("{slug}.json"))).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let kind = if !a.kind.is_empty() { a.kind.clone() } else { prev.as_ref().and_then(|p| p["kind"].as_str()).unwrap_or("").to_string() };
    let recipe = if a.recipe.is_object() { a.recipe.clone() } else { prev.as_ref().map(|p| p["recipe"].clone()).filter(|v| v.is_object()).unwrap_or(json!({})) };
    let mut rec = json!({"name": slug, "criteria": a.criteria, "folder": folder_norm(&a.folder), "goal": a.goal,
        "agent": if a.agent.is_object() { a.agent } else { json!({}) },
        "keywords": a.keywords, "engines": a.engines, "kind": kind, "recipe": recipe,
        "created": prev.as_ref().and_then(|p| p["created"].as_f64())
            .unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64())});
    if let Some(lr) = prev.as_ref().map(|p| p["last_run"].clone()).filter(|v| v.is_object()) {
        rec["last_run"] = lr; // 直近の成績も消さない
    }
    let _ = std::fs::create_dir_all(&dir);
    if std::fs::write(dir.join(format!("{slug}.json")), serde_json::to_string_pretty(&rec).unwrap()).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    Json(rec).into_response()
}

async fn api_albums(State(app): S) -> Json<Value> {
    let mut out = vec![];
    let running = app.crawl.alive.load(Relaxed);
    let running_album = app.crawl.album.lock().unwrap().clone();
    let gen_running = app.gen.alive.load(Relaxed);
    let gen_album = app.gen.album.lock().unwrap().clone();
    // 件数の高速路: フォルダの大半はsource条件だけなので source→枚数 を1クエリで引く。
    // 従来の「フォルダごとに query_shas で全sha取得→len」は19フォルダで1.2秒＝UIもっさりの主因(2026-09-03)
    let src_counts: std::collections::HashMap<String, i64> = {
        let db = app.db.lock().unwrap();
        db.prepare("SELECT source, COUNT(*) FROM images GROUP BY source")
            .ok()
            .and_then(|mut s| {
                s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                    .ok()
                    .map(|rs| rs.flatten().collect())
            })
            .unwrap_or_default()
    };
    for mut a in load_albums(&app.root) {
        // criteriaがsourceのみ(他キーは空)なら高速路、複雑な条件だけ従来の全評価
        let simple_src = a["criteria"].as_object().and_then(|o| {
            let others_empty = o.iter().all(|(k, v)| k == "source" || v.as_str().map(|s| s.is_empty()).unwrap_or(false));
            match (others_empty, o.get("source").and_then(|v| v.as_str())) {
                (true, Some(s)) if !s.is_empty() => Some(s.to_string()),
                _ => None,
            }
        });
        if let Some(s) = simple_src {
            a["count"] = json!(src_counts.get(&s).copied().unwrap_or(0));
        } else if let Ok(q) = serde_json::from_value::<Q>(a["criteria"].clone()) {
            a["count"] = json!(query_shas(app, &q).len());
        }
        let gen_on = gen_running && a["name"] == json!(gen_album.clone());
        a["running"] = json!((running && a["name"] == json!(running_album.clone())) || gen_on);
        if gen_on { a["running_kind"] = json!("gen"); }
        out.push(a);
    }
    Json(json!(out))
}

async fn api_album_del(State(app): S, AxPath(name): AxPath<String>) -> impl IntoResponse {
    let p = album_dir(&app.root).join(format!("{}.json", album_slug(&name)));
    if !p.exists() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let _ = std::fs::remove_file(p);
    Json(json!({"ok": true})).into_response()
}

// ---------- フォルダの整理: 改名 / 移動 / 合流(2026-09-04) ----------
// 名前はAIフォルダの持ち物の鍵(画像のsource="crawl:<name>" / 収集台帳 / 顔ID登録)。
// 改名では鍵を全部まとめて付け替える(片方だけ変えると画像が迷子になる)。

fn album_path(root: &std::path::Path, name: &str) -> PathBuf {
    album_dir(root).join(format!("{}.json", album_slug(name)))
}
fn load_album(root: &std::path::Path, name: &str) -> Option<Value> {
    std::fs::read_to_string(album_path(root, name)).ok().and_then(|t| serde_json::from_str(&t).ok())
}
fn save_album(root: &std::path::Path, rec: &Value) -> bool {
    let _ = std::fs::create_dir_all(album_dir(root));
    let name = rec["name"].as_str().unwrap_or("");
    !name.is_empty()
        && std::fs::write(album_path(root, name), serde_json::to_string_pretty(rec).unwrap()).is_ok()
}
fn ledger_file(root: &std::path::Path, name: &str) -> PathBuf {
    root.join("store/crawl").join(format!("{}.ledger.json", album_slug(name)))
}
/// 収集中/順番待ちのフォルダは触らせない(走っている足元の床は張り替えない)
fn album_busy(app: &App, name: &str) -> bool {
    let slug = album_slug(name);
    let running = app.crawl.alive.load(Relaxed) && album_slug(&app.crawl.album.lock().unwrap()) == slug;
    let generating = app.gen.alive.load(Relaxed) && album_slug(&app.gen.album.lock().unwrap()) == slug;
    running || generating || app.crawl_queue.lock().unwrap().iter().any(|c| album_slug(&c.album) == slug)
}
fn err_json(code: StatusCode, msg: &str) -> axum::response::Response {
    (code, Json(json!({"detail": msg}))).into_response()
}
/// 指定shaのsourceを付け替える(DB+サイドカーの両方。片方だけだと再構築で元に戻る)
fn retag_shas(root: &std::path::Path, db: &Connection, shas: &[String], to: &str) -> usize {
    let origin = store::infer_origin(to);
    let mut n = 0usize;
    for sha in shas {
        if db.execute("UPDATE images SET source=?1, origin=?2 WHERE sha1=?3",
                      rusqlite::params![to, origin, sha]).unwrap_or(0) > 0 {
            if let Some(mut m) = store::load_meta(root, sha) {
                m["source"] = json!(to);
                m["origin"] = json!(origin);
                let _ = store::save_meta(root, &m);
            }
            n += 1;
        }
    }
    n
}
fn retag_source(root: &std::path::Path, from: &str, to: &str) -> usize {
    let db = match Connection::open(root.join("store/index.sqlite")) { Ok(d) => d, Err(_) => return 0 };
    store::ensure_schema(&db);
    let shas: Vec<String> = db
        .prepare("SELECT sha1 FROM images WHERE source=?1")
        .ok()
        .and_then(|mut st| st.query_map([from], |r| r.get::<_, String>(0)).ok().map(|rs| rs.flatten().collect()))
        .unwrap_or_default();
    retag_shas(root, &db, &shas, to)
}

#[derive(Deserialize)]
struct AlbumRenameIn { to: String }

async fn api_album_rename(State(app): S, AxPath(name): AxPath<String>, Json(p): Json<AlbumRenameIn>) -> impl IntoResponse {
    let old = album_slug(&name);
    if p.to.trim().is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "新しい名前をください");
    }
    let new = album_slug(p.to.trim());
    let mut rec = match load_album(&app.root, &old) {
        Some(r) => r,
        None => return err_json(StatusCode::NOT_FOUND, "フォルダが見つかりません"),
    };
    if new == old {
        return Json(json!({"ok": true, "name": old, "moved": 0})).into_response();
    }
    if album_path(&app.root, &new).exists() {
        return err_json(StatusCode::CONFLICT, "その名前のフォルダはもうあります");
    }
    if album_busy(app, &old) {
        return err_json(StatusCode::CONFLICT, "収集中(または順番待ち)のフォルダは名前を変えられません。止めてからどうぞ");
    }
    let pfx = if rec["kind"].as_str() == Some("gen") { "gen:" } else { "crawl:" }; // 生成フォルダのバケツは gen:<name>
    let old_src = format!("{pfx}{old}");
    let new_src = format!("{pfx}{new}");
    // 自分のバケツ(crawl:<自分の名前>)を持つフォルダだけ、中身のsourceも一緒に引っ越す
    let owns = rec["criteria"]["source"].as_str() == Some(old_src.as_str());
    rec["name"] = json!(new);
    if owns { rec["criteria"]["source"] = json!(new_src.clone()); }
    if !save_album(&app.root, &rec) {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, "保存に失敗しました");
    }
    let _ = std::fs::remove_file(album_path(&app.root, &old));
    let _ = std::fs::rename(ledger_file(&app.root, &old), ledger_file(&app.root, &new)); // 既読台帳も名前について行く
    let _ = std::fs::rename(app.root.join("store/gen_ledger").join(format!("{old}.json")), app.root.join("store/gen_ledger").join(format!("{new}.json")));
    {
        let db = app.db.lock().unwrap();
        let _ = db.execute("UPDATE faces SET album=?1 WHERE album=?2", rusqlite::params![new, old]);
    }
    let moved = if owns {
        let root = app.root.clone();
        tokio::task::spawn_blocking(move || retag_source(&root, &old_src, &new_src)).await.unwrap_or(0)
    } else { 0 };
    Json(json!({"ok": true, "name": new, "moved": moved})).into_response()
}

#[derive(Deserialize)]
struct AlbumMoveIn { #[serde(default)] folder: String }

/// D&D: フォルダをグループへ入れる/外へ出す(folderは表示上の棚だけ。中身は動かない)
async fn api_album_move(State(app): S, AxPath(name): AxPath<String>, Json(p): Json<AlbumMoveIn>) -> impl IntoResponse {
    let slug = album_slug(&name);
    let mut rec = match load_album(&app.root, &slug) {
        Some(r) => r,
        None => return err_json(StatusCode::NOT_FOUND, "フォルダが見つかりません"),
    };
    let folder = folder_norm(&p.folder);
    rec["folder"] = json!(folder);
    if !save_album(&app.root, &rec) {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, "保存に失敗しました");
    }
    Json(json!({"ok": true, "name": slug, "folder": folder})).into_response()
}

#[derive(Deserialize)]
struct FolderRenameIn { from: String, #[serde(default)] to: String, #[serde(default)] kind: String }

/// グループ(ツリーの中間ノード)の改名と移動。実体は各フォルダのfolderパスの前方一致置換
async fn api_folder_rename(State(app): S, Json(p): Json<FolderRenameIn>) -> impl IntoResponse {
    let from = folder_norm(&p.from);
    let to = folder_norm(&p.to);
    if from.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "元のグループがありません");
    }
    if to == from {
        return Json(json!({"ok": true, "changed": 0, "to": to})).into_response();
    }
    if to.starts_with(&format!("{from}/")) {
        return err_json(StatusCode::BAD_REQUEST, "自分の中へは移せません");
    }
    let prefix = format!("{from}/");
    let mut changed = 0usize;
    // 出荷(データセット)の棚。名札はmanifest.folderにあり、ディレクトリは動かさない
    if p.kind == "dataset" {
        if let Ok(rd) = std::fs::read_dir(app.root.join("store/datasets")) {
            for e in rd.flatten() {
                let mf = e.path().join("manifest.json");
                let Some(mut m) = std::fs::read_to_string(&mf).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { continue };
                let cur = m["folder"].as_str().unwrap_or("").to_string();
                let next = if cur == from {
                    to.clone()
                } else if let Some(rest) = cur.strip_prefix(&prefix) {
                    if to.is_empty() { rest.to_string() } else { format!("{to}/{rest}") }
                } else {
                    continue;
                };
                m["folder"] = json!(folder_norm(&next));
                if std::fs::write(&mf, serde_json::to_string_pretty(&m).unwrap()).is_ok() { changed += 1; }
            }
        }
        return Json(json!({"ok": true, "changed": changed, "to": to})).into_response();
    }
    for a in load_albums(&app.root) {
        let cur = a["folder"].as_str().unwrap_or("").to_string();
        let next = if cur == from {
            to.clone()
        } else if let Some(rest) = cur.strip_prefix(&prefix) {
            if to.is_empty() { rest.to_string() } else { format!("{to}/{rest}") }
        } else {
            continue;
        };
        let mut rec = a.clone();
        rec["folder"] = json!(folder_norm(&next));
        if save_album(&app.root, &rec) { changed += 1; }
    }
    Json(json!({"ok": true, "changed": changed, "to": to})).into_response()
}

#[derive(Deserialize)]
struct AlbumMergeIn { from: String, into: String }

/// 合流: 元フォルダの中身を移動先のバケツへ移し、元フォルダ(条件と台帳)を畳む。画像は1枚も消えない
async fn api_album_merge(State(app): S, Json(p): Json<AlbumMergeIn>) -> impl IntoResponse {
    let from = album_slug(&p.from);
    let into = album_slug(&p.into);
    if from == into {
        return err_json(StatusCode::BAD_REQUEST, "同じフォルダ同士は合流できません");
    }
    let src = match load_album(&app.root, &from) {
        Some(r) => r,
        None => return err_json(StatusCode::NOT_FOUND, "元のフォルダが見つかりません"),
    };
    let mut dst = match load_album(&app.root, &into) {
        Some(r) => r,
        None => return err_json(StatusCode::NOT_FOUND, "移動先のフォルダが見つかりません"),
    };
    if album_busy(app, &from) || album_busy(app, &into) {
        return err_json(StatusCode::CONFLICT, "収集中(または順番待ち)のフォルダは合流できません。止めてからどうぞ");
    }
    let dst_src = dst["criteria"]["source"].as_str().unwrap_or("").to_string();
    if dst_src.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "移動先が条件フォルダなので受け皿がありません。取り込み先を持つフォルダへ合流してください");
    }
    // 条件が空(=ライブラリ全体)のフォルダを吸わせると全画像が付け替わる事故になるので断る
    let crit = src["criteria"].clone();
    let has_cond = crit.as_object().map(|o| o.iter().any(|(_, v)| match v {
        Value::String(s) => !s.is_empty(),
        Value::Null => false,
        _ => true,
    })).unwrap_or(false);
    if !has_cond {
        return err_json(StatusCode::BAD_REQUEST, "元フォルダの条件が空(=ライブラリ全体)なので合流できません");
    }
    let q = match serde_json::from_value::<Q>(crit) {
        Ok(q) => q,
        Err(_) => return err_json(StatusCode::BAD_REQUEST, "元フォルダの条件が読めません"),
    };
    let shas = query_shas(app, &q);
    let root = app.root.clone();
    let target = dst_src.clone();
    let moved = tokio::task::spawn_blocking(move || {
        let db = match Connection::open(root.join("store/index.sqlite")) { Ok(d) => d, Err(_) => return 0 };
        store::ensure_schema(&db);
        retag_shas(&root, &db, &shas, &target)
    })
    .await
    .unwrap_or(0);
    // 既読台帳を合流(同じURL/クエリを二度拾いに行かない)
    {
        let rd = |p: PathBuf| -> Value {
            std::fs::read_to_string(p).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(json!({}))
        };
        let (a, b) = (rd(ledger_file(&app.root, &from)), rd(ledger_file(&app.root, &into)));
        let strs = |v: &Value, k: &str| -> Vec<String> {
            v[k].as_array().map(|x| x.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default()
        };
        let mut queries = strs(&b, "queries");
        for x in strs(&a, "queries") { if !queries.contains(&x) { queries.push(x); } }
        let mut urls: std::collections::HashSet<String> = strs(&b, "urls").into_iter().collect();
        urls.extend(strs(&a, "urls"));
        let brief = match b["brief"].as_str().unwrap_or("") {
            "" => a["brief"].as_str().unwrap_or("").to_string(),
            s => s.to_string(),
        };
        let lp = ledger_file(&app.root, &into);
        let _ = std::fs::create_dir_all(lp.parent().unwrap());
        let _ = std::fs::write(&lp, serde_json::to_string(&json!({"queries": queries, "urls": urls, "brief": brief})).unwrap());
    }
    // キーワードと使用済みコストも引き継ぐ(合流で予算の記憶を失わない)
    {
        let mut kw: Vec<String> = dst["keywords"].as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        for k in src["keywords"].as_array().unwrap_or(&vec![]).iter().filter_map(|x| x.as_str()) {
            if !kw.iter().any(|e| e == k) { kw.push(k.to_string()); }
        }
        dst["keywords"] = json!(kw);
        let usd = dst["agent"]["spent_usd"].as_f64().unwrap_or(0.0) + src["agent"]["spent_usd"].as_f64().unwrap_or(0.0);
        let tok = dst["agent"]["spent_tok"].as_u64().unwrap_or(0) + src["agent"]["spent_tok"].as_u64().unwrap_or(0);
        if !dst["agent"].is_object() { dst["agent"] = json!({}); }
        dst["agent"]["spent_usd"] = json!((usd * 1000.0).round() / 1000.0);
        dst["agent"]["spent_tok"] = json!(tok);
    }
    save_album(&app.root, &dst);
    let _ = std::fs::remove_file(album_path(&app.root, &from));
    let _ = std::fs::remove_file(ledger_file(&app.root, &from));
    {
        let db = app.db.lock().unwrap();
        let _ = db.execute("UPDATE faces SET album=?1 WHERE album=?2", rusqlite::params![into, from]);
    }
    Json(json!({"ok": true, "moved": moved, "into": into})).into_response()
}

// ---------- M5 クローラ(AIフォルダの▶) ----------

#[derive(Deserialize, Clone)]
struct CrawlIn {
    album: String,                              // goal付きアルバム名
    #[serde(default = "d_crawl_n")] n: usize,   // 収蔵枚数の上限(動作リミット)
    #[serde(default = "d_crawl_min")] minutes: u64, // 時間上限
    #[serde(default = "d_judgeq")] min_quality: i64,
}
fn d_crawl_n() -> usize { 50 }
fn d_crawl_min() -> u64 { 15 }
fn d_judgeq() -> i64 { 5 }

fn start_crawl(app: &'static App, album: &str, n: usize, minutes: u64, min_quality: i64) -> Result<String, (StatusCode, String)> {
    if app.crawl.alive.load(Relaxed) {
        return Err((StatusCode::CONFLICT, "クローラは既に実行中です".into()));
    }
    let slug = album_slug(album);
    let rec = load_albums(&app.root).into_iter().find(|a| a["name"] == json!(slug.clone()));
    let Some(rec) = rec else {
        return Err((StatusCode::NOT_FOUND, format!("アルバム{slug}が見つかりません")));
    };
    let goal = rec["goal"].as_str().unwrap_or("").to_string();
    if goal.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "goalが空です — AIフォルダ(目標付き)だけが走れます".into()));
    }
    let strlist = |v: &Value| -> Vec<String> {
        v.as_array().map(|a| a.iter().filter_map(|k| k.as_str().map(String::from)).collect()).unwrap_or_default()
    };
    let keywords = strlist(&rec["keywords"]);
    let mut engines = strlist(&rec["engines"]);
    // 権利クリーンモード: CC/公式APIソースのみに強制(UI改変やAPI直叩きでも破れない・2026-09-03指示)。
    // ddg/youtube/xは権利不明になるため除外。rights=cleanで収蔵される検索元だけ残す。
    if rec["agent"]["rights_clean"].as_bool().unwrap_or(false) {
        const CLEAN: [&str; 4] = ["openverse", "wikimedia", "pexels", "pixabay"];
        engines.retain(|e| CLEAN.contains(&e.as_str()));
        if engines.is_empty() {
            engines = CLEAN.iter().map(|s| s.to_string()).collect();
        }
    }
    let st = app.crawl.clone();
    st.alive.store(true, Relaxed);
    st.stop.store(false, Relaxed);
    for a in [&st.found, &st.checked, &st.rejected, &st.ingested, &st.errors, &st.spent_cents, &st.uusd, &st.utok] {
        a.store(0, Relaxed); // utokをリセット漏れするとrun毎に累積が二重計上される(2026-09-03実害)
    }
    *st.album.lock().unwrap() = slug.clone();
    *st.last.lock().unwrap() = "起動中…".into();
    let limits = crawl::Limits {
        max_n: n.clamp(1, 2000),
        max_secs: minutes.clamp(1, 240) * 60,
        max_errors: 8,
        min_quality: min_quality.clamp(1, 10),
        boost: rec["agent"]["boost"].as_bool().unwrap_or(false),
        // 💰上限は「フォルダ累計」(2026-09-03指示: 回ではなくトータル)。使った分を引いた残り枠を今回使える
        max_cents: {
            let budget = rec["agent"]["budget_usd"].as_f64().unwrap_or(3.0).clamp(0.5, 500.0);
            let spent = rec["agent"]["spent_usd"].as_f64().unwrap_or(0.0);
            ((budget - spent).max(0.0) * 100.0) as usize
        },
        // 目利きモデル: フォルダ設定が最優先、無ければsettings.jsonの既定(gallery_judge_model)
        judge_model: rec["agent"]["judge_model"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(crawl::default_judge_model),
    };
    tokio::spawn(crawl::run(app.root.clone(), app.http.clone(), st, app.llm.clone(), app.enrich.clone(), slug.clone(), goal, keywords, engines, limits));
    Ok(slug)
}

#[derive(Deserialize)]
struct LedgerIn {
    album: String,
}

/// 台帳リセット=「探し直す」。使用済みクエリ/URLを忘れて次回まっさらに走る
async fn api_ledger_clear(State(app): S, Json(l): Json<LedgerIn>) -> impl IntoResponse {
    let p = app.root.join("store/crawl").join(format!("{}.ledger.json", album_slug(&l.album)));
    let existed = p.exists();
    let _ = std::fs::remove_file(p);
    Json(json!({"ok": true, "cleared": existed}))
}

async fn api_crawl(State(app): S, Json(c): Json<CrawlIn>) -> impl IntoResponse {
    // 実行中なら弾かずに順番待ちへ(同じフォルダの重複は上書き)
    if app.crawl.alive.load(Relaxed) {
        let mut q = app.crawl_queue.lock().unwrap();
        let slug = album_slug(&c.album);
        q.retain(|x| album_slug(&x.album) != slug);
        q.push(c);
        return Json(json!({"ok": true, "queued": true, "position": q.len(),
                           "note": "いまの収集が終わったら自動で始まります"})).into_response();
    }
    vlm_wake(app).await;
    match start_crawl(app, &c.album, c.n, c.minutes, c.min_quality) {
        Ok(slug) => Json(json!({"ok": true, "album": slug})).into_response(),
        Err((code, msg)) => (code, Json(json!({"detail": msg}))).into_response(),
    }
}

/// 4xxをパス付きでログ(422がどのAPIか現場で特定できるように 2026-09-03)
async fn log_client_errors(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let m = req.method().clone();
    let p = req.uri().path().to_string();
    let res = next.run(req).await;
    let status = res.status();
    if status.is_client_error() && status != StatusCode::NOT_FOUND {
        // 拒否理由(serdeのエラーメッセージ)ごとログへ。bodyは読み出して詰め直す
        let (parts, body) = res.into_parts();
        let bytes = axum::body::to_bytes(body, 64 * 1024).await.unwrap_or_default();
        println!("⚠ {m} {p} -> {status} | {}", String::from_utf8_lossy(&bytes));
        return axum::response::Response::from_parts(parts, axum::body::Body::from(bytes));
    }
    res
}

async fn api_crawl_status(State(app): S) -> Json<Value> {
    let mut s = app.crawl.status();
    s["queue"] = json!(app.crawl_queue.lock().unwrap().iter().map(|c| album_slug(&c.album)).collect::<Vec<_>>());
    // 💰全体累計 = 台帳(終了済みrun) + 実行中run
    let ledger = std::fs::read_to_string(app.root.join("store/crawl/spend.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["total_usd"].as_f64())
        .unwrap_or(0.0);
    let running = if s["alive"].as_bool().unwrap_or(false) { s["spent_usd"].as_f64().unwrap_or(0.0) } else { 0.0 };
    s["total_usd"] = json!(((ledger + running) * 1000.0).round() / 1000.0);
    Json(s)
}

/// 却下画像の小サムネ(途中経過ストリップ用・再生成不可の一時物なのでno-cache)
async fn crawl_reject_thumb(State(app): S, AxPath(uk): AxPath<String>) -> impl IntoResponse {
    if !uk.chars().all(|c| c.is_ascii_hexdigit()) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match std::fs::read(app.root.join("store/crawl/rejects").join(format!("{uk}.jpg"))) {
        Ok(b) => ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "no-cache")], b).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn api_crawl_stop(State(app): S) -> Json<Value> {
    app.crawl.stop.store(true, Relaxed);
    Json(json!({"ok": true}))
}

// ---------- ブラウザ/スマホからのアップロード(画像・動画・カメラ直撮り) ----------

async fn api_upload(State(app): S, mut mp: axum::extract::Multipart) -> impl IntoResponse {
    let mut source = "upload".to_string();
    let (mut added, mut dup, mut bad, mut frames_in, mut frames_skip) = (0, 0, 0, 0, 0);
    let scratch = app.root.join("store/.upload_tmp");
    while let Ok(Some(field)) = mp.next_field().await {
        let fname = field.file_name().unwrap_or("").to_lowercase();
        if field.name() == Some("source") {
            if let Ok(t) = field.text().await {
                if !t.trim().is_empty() {
                    source = t.trim().to_string();
                }
            }
            continue;
        }
        let Ok(data) = field.bytes().await else { bad += 1; continue };
        let ext = fname.rsplit('.').next().unwrap_or("").to_string();
        let root = app.root.clone();
        let src = source.clone();
        if ext == "zip" {
            // zip取り込み: 中の画像を収蔵。meta/<sha1>.json(このアプリの書き出し=サイドカー)があれば出典/ライセンス/編集履歴ごと復元
            let src_given = source != "upload";
            let zip_name = fname.trim_end_matches(".zip").to_string();
            let res = tokio::task::spawn_blocking(move || -> (usize, usize, usize, usize) {
                let db = Connection::open(root.join("store/index.sqlite")).unwrap();
                store::ensure_schema(&db);
                let Ok(mut ar) = zip::ZipArchive::new(std::io::Cursor::new(&data[..])) else { return (0, 0, 1, 0) };
                use std::io::Read;
                let mut metas: std::collections::HashMap<String, Value> = Default::default();
                for i in 0..ar.len() {
                    let Ok(mut e) = ar.by_index(i) else { continue };
                    let name = e.name().to_string();
                    let parts: Vec<&str> = name.split('/').collect();
                    if name.ends_with(".json") && parts.len() >= 2 && parts[parts.len() - 2] == "meta" {
                        let mut t = String::new();
                        if e.read_to_string(&mut t).is_ok() {
                            if let Ok(v) = serde_json::from_str::<Value>(&t) {
                                metas.insert(parts[parts.len() - 1].trim_end_matches(".json").to_string(), v);
                            }
                        }
                    }
                }
                let (mut a, mut d, mut b) = (0, 0, 0);
                for i in 0..ar.len() {
                    let Ok(mut e) = ar.by_index(i) else { continue };
                    if e.is_dir() { continue; }
                    let name = e.name().to_string();
                    let base = name.rsplit('/').next().unwrap_or("").to_string();
                    if base.starts_with("._") || base.starts_with('.') { continue; } // macOSのリソースフォーク等
                    let ext = base.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
                    if !["jpg", "jpeg", "png", "webp", "gif", "bmp", "tif", "tiff"].contains(&ext.as_str()) { continue; }
                    let mut bytes = Vec::new();
                    if e.read_to_end(&mut bytes).is_err() { b += 1; continue; }
                    let stem = base.trim_end_matches(&format!(".{}", base.rsplit('.').next().unwrap_or(""))).to_string();
                    let mut extra = metas.get(&stem).cloned().unwrap_or(json!({}));
                    let mut this_src = if src_given { src.clone() } else { format!("zip:{zip_name}") };
                    if let Some(o) = extra.as_object_mut() {
                        if !src_given { if let Some(s0) = o.get("source").and_then(|v| v.as_str()) { this_src = s0.to_string(); } }
                        for k in ["sha1", "ext", "w", "h", "bytes", "ingested", "phash", "tint", "source"] { o.remove(k); }
                        if !metas.contains_key(&stem) { o.insert("rights".into(), json!("unknown")); }
                    }
                    match store::ingest_bytes(&root, &db, &bytes, &ext, &this_src, &extra) {
                        Ok(_) => a += 1,
                        Err("dup") => d += 1,
                        Err(_) => b += 1,
                    }
                }
                (a, d, b, metas.len())
            }).await.unwrap_or((0, 0, 1, 0));
            added += res.0; dup += res.1; bad += res.2;
            println!("📦 zip取り込み {fname}: +{} (重複{} 不採用{} サイドカー{})", res.0, res.1, res.2, res.3);
            continue;
        }
        if media::VIDEO_EXTS.contains(&ext.as_str()) {
            // 動画→1fpsフレーム抽出→ソックリ連続フレームは間引いて収蔵(無駄な動画を入れない)
            let scratch2 = scratch.clone();
            let res = tokio::task::spawn_blocking(move || -> (usize, usize, usize, usize) {
                let db = Connection::open(root.join("store/index.sqlite")).unwrap();
                store::ensure_schema(&db);
                let Ok(frames) = media::extract_frames(&scratch2, &data, 1.0) else { return (0, 0, 1, 0) };
                let (mut a, mut d, mut skip) = (0, 0, 0);
                let mut last_ph: Option<String> = None;
                let total = frames.len();
                for f in frames {
                    let Ok(img) = image::load_from_memory(&f) else { continue };
                    let ph = store::phash64(&img);
                    if let Some(prev) = &last_ph {
                        if store_hamming(prev, &ph) <= 8 {
                            skip += 1; // ほぼ同じ場面の連写は捨てる
                            continue;
                        }
                    }
                    match store::ingest_bytes(&root, &db, &f, "jpg", &src, &json!({"video_frame": true})) {
                        Ok(_) => { a += 1; last_ph = Some(ph); }
                        Err("dup") => dup_inc(&mut d),
                        Err(_) => {}
                    }
                }
                (a, d, skip, total)
            })
            .await
            .unwrap_or((0, 0, 0, 0));
            added += res.0;
            dup += res.1;
            frames_skip += res.2;
            frames_in += res.3;
        } else {
            let r = tokio::task::spawn_blocking(move || {
                let db = Connection::open(root.join("store/index.sqlite")).unwrap();
                store::ensure_schema(&db);
                store::ingest_bytes(&root, &db, &data, &ext, &src, &json!({}))
            })
            .await
            .unwrap_or(Err("bad"));
            match r {
                Ok(_) => added += 1,
                Err("dup") => dup += 1,
                Err(_) => bad += 1,
            }
        }
    }
    Json(json!({"ok": true, "added": added, "dup": dup, "bad": bad,
                "video_frames": frames_in, "frames_skipped": frames_skip}))
}

fn dup_inc(d: &mut usize) { *d += 1; }

fn store_hamming(a: &str, b: &str) -> u32 {
    match (u64::from_str_radix(a, 16), u64::from_str_radix(b, 16)) {
        (Ok(x), Ok(y)) => (x ^ y).count_ones(),
        _ => 64,
    }
}

// ---------- 自動セグメント(フォルダは目標=クラスを知っている) ----------

#[derive(Deserialize)]
struct SegIn {
    #[serde(default)] album: String,
    #[serde(default)] shas: Vec<String>,
    #[serde(default)] prompt: String, // 空=goalから内蔵LLMが検出クラス語を抽出
}

async fn api_seg(State(app): S, Json(s): Json<SegIn>) -> impl IntoResponse {
    if app.seg.alive.load(Relaxed) {
        return (StatusCode::CONFLICT, Json(json!({"detail": "マスク生成が実行中です"}))).into_response();
    }
    let mut prompt = s.prompt.trim().to_string();
    let shas = if !s.shas.is_empty() {
        s.shas
    } else if !s.album.is_empty() {
        let slug = album_slug(&s.album);
        let Some(rec) = load_albums(&app.root).into_iter().find(|a| a["name"] == json!(slug.clone())) else {
            return (StatusCode::NOT_FOUND, Json(json!({"detail": "アルバムが見つかりません"}))).into_response();
        };
        if prompt.is_empty() {
            let goal = rec["goal"].as_str().unwrap_or(&slug).to_string();
            // 目標文→検出クラス語(英語1-3語)。内蔵LLMなので$0
            prompt = llm::chat(&app.root, &app.http, &app.llm,
                "Reply with ONLY 1-3 short English object class words, comma separated. No other text.",
                &format!("画像内で検出したい対象物を英語クラス語で。目標:「{goal}」"), 60)
                .await
                .ok()
                .map(|t| t.trim().trim_matches(['`', '"', '。', '.']).to_string())
                .filter(|t| !t.is_empty() && t.len() < 80)
                .unwrap_or_else(|| slug.clone());
        }
        serde_json::from_value::<Q>(rec["criteria"].clone()).map(|q| query_shas(app, &q)).unwrap_or_default()
    } else {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": "album か shas をください"}))).into_response();
    };
    if shas.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": "対象画像がありません"}))).into_response();
    }
    if prompt.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": "promptが決められません"}))).into_response();
    }
    let st = app.seg.clone();
    st.alive.store(true, Relaxed);
    st.stop.store(false, Relaxed);
    tokio::spawn(seg::run(app.root.clone(), app.http.clone(), st, shas, prompt.clone()));
    Json(json!({"ok": true, "prompt": prompt})).into_response()
}

/// 遅延マスク: 開いた1枚に無ければその場で切る(対象語は属性から自動: 動物→その動物、人→person)
#[derive(Deserialize)]
struct SegOneIn {
    sha1: String,
    #[serde(default)] prompt: String,
}

async fn api_seg_one(State(app): S, Json(s): Json<SegOneIn>) -> impl IntoResponse {
    let Some(mut m) = store::load_meta(&app.root, &s.sha1) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if m["seg"].is_object() && s.prompt.is_empty() {
        return Json(m).into_response(); // もう切ってある
    }
    let prompt = if !s.prompt.trim().is_empty() {
        s.prompt.trim().to_string()
    } else {
        let a = &m["vlm"]["attrs"];
        let animal = a["animal"].as_str().unwrap_or("");
        let subject = a["subject"].as_str().unwrap_or("");
        if !animal.is_empty() && animal != "none" {
            animal.to_string()
        } else if subject == "person" || subject == "face" {
            "person".to_string()
        } else if !subject.is_empty() && !["other", "text", "abstract"].contains(&subject) {
            subject.to_string()
        } else {
            return Json(json!({"skipped": true, "reason": "対象語を決められません(属性が無い/曖昧)"})).into_response();
        }
    };
    let ext = m["ext"].as_str().unwrap_or("jpg").to_string();
    let Ok(bytes) = std::fs::read(store::image_path(&app.root, &s.sha1, &ext)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match seg::seg_one(&app.http, &bytes, &prompt).await {
        Ok(shapes) => {
            m["seg"] = json!({"prompt": prompt, "model": "gdino2seg", "shapes": shapes,
                "ts": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()});
            if store::save_meta(&app.root, &m).is_ok() {
                store::index_meta(&app.db.lock().unwrap(), &m);
            }
            edits::clear_renders(&app.root, &s.sha1);
            Json(m).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"detail": e}))).into_response(),
    }
}

#[derive(Deserialize)]
struct SegRefineIn {
    sha1: String,
    #[serde(default)] points: Vec<Vec<f64>>, // 正規化0-1 [[x,y],..] クリック点
    #[serde(default)] labels: Vec<i64>,      // 1=前景 / 0=背景(右クリック除外)
    #[serde(default, rename = "box")] box_: Option<Vec<f64>>, // 正規化 [x1,y1,x2,y2] 範囲選択
    #[serde(default)] cls: String,
    #[serde(default)] replace: bool, // true=全置換 / false=既存マスクに追加
}

/// クリック/範囲選択でマスクを切り直す(ml-hub SAM2直叩き)。ml-hubアノテエディタ相当のUX
async fn api_seg_refine(State(app): S, Json(s): Json<SegRefineIn>) -> impl IntoResponse {
    let Some(mut m) = store::load_meta(&app.root, &s.sha1) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if s.points.is_empty() && s.box_.is_none() {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": "pointsかboxが必要です"}))).into_response();
    }
    let ext = m["ext"].as_str().unwrap_or("jpg").to_string();
    let Ok(bytes) = std::fs::read(store::image_path(&app.root, &s.sha1, &ext)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let cls = if s.cls.trim().is_empty() {
        // クラス未指定は既存マスクのクラス→無ければ属性から
        m["seg"]["shapes"][0]["cls"].as_str()
            .or(m["vlm"]["attrs"]["subject"].as_str())
            .unwrap_or("object")
            .to_string()
    } else {
        s.cls.trim().to_string()
    };
    match seg::sam_refine(&app.http, &bytes, &s.points, &s.labels, s.box_.as_deref(), &cls).await {
        Ok(shapes) if !shapes.is_empty() => {
            let mut cur: Vec<Value> = if s.replace {
                vec![]
            } else {
                m["seg"]["shapes"].as_array().cloned().unwrap_or_default()
            };
            cur.extend(shapes);
            let prompt = m["seg"]["prompt"].as_str().unwrap_or(&cls).to_string();
            m["seg"] = json!({"prompt": prompt, "model": "sam2:manual", "shapes": cur,
                "ts": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()});
            if store::save_meta(&app.root, &m).is_ok() {
                store::index_meta(&app.db.lock().unwrap(), &m);
            }
            edits::clear_renders(&app.root, &s.sha1);
            Json(m).into_response()
        }
        Ok(_) => (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"detail": "そこには物体を見つけられませんでした"}))).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"detail": e}))).into_response(),
    }
}

async fn api_seg_stop(State(app): S) -> Json<Value> {
    app.seg.stop.store(true, Relaxed);
    Json(json!({"ok": true}))
}

// ---------- 内蔵LLM(本当の内蔵: llama.cpp直リンク・GGUF自動DL・API代ゼロ) ----------

async fn api_llm_status(State(app): S) -> Json<Value> {
    Json(app.llm.status(&app.root))
}

/// 事前DL(2.4GB)を裏で開始。進捗は/api/llm/statusで見える
async fn api_llm_pull(State(app): S) -> Json<Value> {
    let root = app.root.clone();
    let client = app.http.clone();
    let st = app.llm.clone();
    tokio::spawn(async move {
        if let Err(e) = llm::ensure_model(&root, &client, &st).await {
            println!("🧠 内蔵LLM DL失敗: {e}");
        }
    });
    Json(json!({"ok": true, "note": "進捗は GET /api/llm/status"}))
}

#[derive(Deserialize)]
struct LlmTestIn {
    prompt: String,
    #[serde(default = "d_llm_max")] max_tokens: usize,
}
fn d_llm_max() -> usize { 300 }

async fn api_llm_test(State(app): S, Json(t): Json<LlmTestIn>) -> impl IntoResponse {
    match llm::chat(&app.root, &app.http, &app.llm, "You are a helpful assistant.", &t.prompt, t.max_tokens).await {
        Ok(text) => Json(json!({"ok": true, "text": text})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"detail": e}))).into_response(),
    }
}

/// GPU/RAM実測。nvidia-smi連打はml-hubでUIを殺した前科があるので3秒TTLキャッシュ必須
fn sys_stats() -> Value {
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};
    static CACHE: OnceLock<Mutex<(Instant, Value)>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new((Instant::now() - Duration::from_secs(10), json!(null))));
    let mut c = cache.lock().unwrap();
    if c.0.elapsed() < Duration::from_secs(3) && !c.1.is_null() {
        return c.1.clone();
    }
    // CPU使用率: /proc/statの差分(前回値をstaticに保持。初回は0%になるが2秒ポーリングで即収束)
    static CPU_PREV: OnceLock<Mutex<(u64, u64)>> = OnceLock::new();
    let cpu = std::fs::read_to_string("/proc/stat").ok().and_then(|s| {
        let v: Vec<u64> = s.lines().next()?.split_whitespace().skip(1)
            .filter_map(|x| x.parse().ok()).collect();
        if v.len() < 5 { return None; }
        let idle = v[3] + v[4];
        let total: u64 = v.iter().sum();
        let prev = CPU_PREV.get_or_init(|| Mutex::new((idle, total)));
        let mut p = prev.lock().unwrap();
        let (di, dt) = (idle.saturating_sub(p.0), total.saturating_sub(p.1));
        *p = (idle, total);
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        (dt > 0).then(|| json!({"pct": (100.0 * (1.0 - di as f64 / dt as f64) * 10.0).round() / 10.0,
                                "cores": cores}))
    }).unwrap_or(json!(null));
    let gpu = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=utilization.gpu,memory.used,memory.total", "--format=csv,noheader,nounits"])
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            let v: Vec<f64> = s.trim().split(',').filter_map(|x| x.trim().parse().ok()).collect();
            (v.len() == 3).then(|| json!({"util": v[0], "vram_used_mb": v[1], "vram_total_mb": v[2]}))
        })
        .unwrap_or(json!(null));
    let disk = std::process::Command::new("df")
        .args(["-Pk", "."])
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            let f: Vec<f64> = s.lines().nth(1)?.split_whitespace().skip(1).take(3)
                .filter_map(|x| x.parse().ok()).collect();
            (f.len() == 3).then(|| json!({"total_gb": f[0] / 1048576.0, "used_gb": f[1] / 1048576.0,
                                          "free_gb": f[2] / 1048576.0}))
        })
        .unwrap_or(json!(null));
    let ram = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|t| {
            let get = |k: &str| {
                t.lines().find(|l| l.starts_with(k))?.split_whitespace().nth(1)?.parse::<f64>().ok()
            };
            Some(json!({"used_gb": (get("MemTotal:")? - get("MemAvailable:")?) / 1048576.0,
                        "total_gb": get("MemTotal:")? / 1048576.0}))
        })
        .unwrap_or(json!(null));
    // macOS(Apple Silicon): /proc も nvidia-smi も無いので ioreg / vm_stat / ps から取る(統合メモリなので VRAM=RAM 総量)
    #[cfg(target_os = "macos")]
    let (cpu, gpu, ram) = { let _ = (&cpu, &gpu, &ram); mac_stats() };
    *c = (Instant::now(), json!({"cpu": cpu, "gpu": gpu, "ram": ram, "disk": disk}));
    c.1.clone()
}

/// Apple Silicon の CPU/GPU/RAM。ioreg の PerformanceStatistics(Device Utilization % / In use system memory)、
/// vm_stat(ページ数)、ps(%cpu 合計)。root 不要、合計 100ms 前後、3 秒 TTL で呼ばれる
#[cfg(target_os = "macos")]
fn mac_stats() -> (Value, Value, Value) {
    fn out(cmd: &str, args: &[&str]) -> String {
        std::process::Command::new(cmd).args(args).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
    }
    fn num_after(s: &str, key: &str) -> Option<f64> {
        let i = s.find(key)? + key.len();
        let d: String = s[i..].chars().take_while(|c| c.is_ascii_digit()).collect();
        d.parse().ok()
    }
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let memsize = out("sysctl", &["-n", "hw.memsize"]).trim().parse::<f64>().unwrap_or(0.0);
    // CPU: 全プロセスの %cpu 合計をコア数で割る(瞬間値。top -l 1 は 1 秒待つので使わない)
    let cpu_sum: f64 = out("ps", &["-A", "-o", "%cpu="]).lines().filter_map(|l| l.trim().parse::<f64>().ok()).sum();
    let cpu = json!({"pct": ((cpu_sum / cores as f64).min(100.0) * 10.0).round() / 10.0, "cores": cores});
    // GPU: IOAccelerator の統計。使っているプロセス(生成/VLM/本体)の常駐メモリも添える
    let io = out("ioreg", &["-r", "-d", "1", "-c", "IOAccelerator"]);
    let procs: Vec<Value> = out("ps", &["-A", "-o", "rss=,comm="]).lines().filter_map(|l| {
        let (rss, comm) = l.trim().split_once(' ')?;
        let name = comm.trim().rsplit('/').next()?.to_string();
        if !["sd-server", "llama-server", "fluent_gallery", "ollama"].iter().any(|k| name.starts_with(k)) { return None; }
        Some(json!({"name": name, "mem_mb": rss.trim().parse::<f64>().ok()? / 1024.0}))
    }).collect();
    let gpu = match (num_after(&io, "\"Device Utilization %\"="), num_after(&io, "\"In use system memory\"=")) {
        (Some(util), used) if memsize > 0.0 => json!({"util": util, "vram_used_mb": used.unwrap_or(0.0) / 1048576.0,
                                                     "vram_total_mb": memsize / 1048576.0, "unified": true, "procs": procs}),
        _ => json!(null),
    };
    // RAM: vm_stat のページ数(active + wired + compressor 占有)。free/inactive は空きとみなす
    let vs = out("vm_stat", &[]);
    let page = num_after(&vs, "page size of ").unwrap_or(16384.0);
    let pages = |k: &str| vs.lines().find(|l| l.starts_with(k)).and_then(|l| l.split(':').nth(1)).and_then(|v| v.trim().trim_end_matches('.').parse::<f64>().ok()).unwrap_or(0.0);
    let used = (pages("Pages active") + pages("Pages wired down") + pages("Pages occupied by compressor")) * page;
    let ram = if memsize > 0.0 { json!({"used_gb": used / 1073741824.0, "total_gb": memsize / 1073741824.0}) } else { json!(null) };
    (cpu, gpu, ram)
}

/// AI稼働状況の一枚板 — どのAIが今なにをしてるかを1回で返す(UIサイドバー常設パネル用)
/// 内蔵/外部AIの準備状況(UIの「AI配役」とサイドバー用)。ollama/ml-hub への接続確認があるので10秒キャッシュ
async fn ai_status(app: &'static App) -> Value {
    static CACHE: Mutex<Option<(std::time::Instant, Value)>> = Mutex::new(None);
    if let Some((t, v)) = CACHE.lock().unwrap().as_ref() {
        if t.elapsed() < std::time::Duration::from_secs(10) {
            return v.clone();
        }
    }
    let probe = |url: String| {
        let c = app.http.clone();
        async move { c.get(url).timeout(std::time::Duration::from_secs(1)).send().await.ok() }
    };
    // ml-hub は :7000 だが、Mac では AirPlay(ControlCenter)も :7000 を掴むので OpenAPI に annotation があるかで判定
    let (ollama, seg) = tokio::join!(probe(format!("{}/api/tags", enrich::OLLAMA)), probe("http://127.0.0.1:7000/openapi.json".into()));
    let seg_ok = match seg {
        Some(r) if r.status().is_success() => r.text().await.map(|t| t.contains("annotation")).unwrap_or(false),
        _ => false,
    };
    let local_vlm = enrich::local_vlm_ok(&app.http).await || vlm::health(&app.http).await;
    let vlm_reachable = local_vlm || ollama.is_some();
    let vlm_present = if local_vlm { true } else { match ollama {
        Some(r) => r.json::<Value>().await.ok()
            .and_then(|t| t["models"].as_array().map(|a| a.iter().any(|m| m["name"].as_str().unwrap_or("").starts_with("qwen2.5vl"))))
            .unwrap_or(false),
        None => false,
    } };
    let key = |k: &str| enrich::mlhub_key(k).is_some();
    let v = json!({
        "llm": app.llm.status(&app.root),
        "clip": onnx::status(&app.root),
        "vlm": {"backend": if local_vlm { "llama-server" } else { "ollama" }, "model": if local_vlm { vlm::MODEL_FILE } else { enrich::BUILTIN_MODEL },
                "reachable": vlm_reachable, "present": vlm_present || vlm::models_present(&app.root), "local": vlm::status(&app.root, &app.vlm)},
        "seg": {"backend": "ml-hub", "reachable": seg_ok},
        "gen": gen::engine_status(&app.root, &app.gen),
        "faceid": faceid_status(),
        "store": cfg!(feature = "store"),
        "tools": {"yt_dlp": tool_present("yt-dlp"), "ffmpeg": tool_present("ffmpeg")},
        "keys": {"anthropic": key("anthropic_api_key"), "openai": key("openai_api_key"), "openrouter": key("openrouter_api_key"),
                 "xai": key("xai_api_key"), "pexels": key("pexels_api_key"), "pixabay": key("pixabay_api_key")},
    });
    *CACHE.lock().unwrap() = Some((std::time::Instant::now(), v.clone()));
    v
}

#[cfg(feature = "faceid")]
fn faceid_status() -> Value { faceid::status() }
#[cfg(not(feature = "faceid"))]
fn faceid_status() -> Value { json!({"enabled": false, "present": false}) }

/// 外部ツールの有無(絶対パスで見つかるか、PATHで実行できるか)
fn tool_present(name: &str) -> bool {
    let p = media::tool_bin(name);
    if p.contains('/') { return std::path::Path::new(&p).exists(); }
    std::env::var("PATH").unwrap_or_default().split(':').any(|d| std::path::Path::new(d).join(&p).exists())
}

/// 内蔵VLM(llama-server)を必要なら起動し、enrich 側の base を確定する。外部指定(FG_VLM_BASE)/ollama があればそれでも良い
async fn vlm_wake(app: &'static App) -> bool {
    match vlm::ensure(&app.root, &app.http, &app.vlm).await {
        Ok(base) => { enrich::set_local_vlm_base(Some(base)); true }
        Err(_) => false,
    }
}
async fn api_vlm_status(State(app): S) -> Json<Value> { Json(vlm::status(&app.root, &app.vlm)) }
/// モデル(3.3GB)の取得と llama-server の起動を裏で開始。進捗は /api/vlm/status, /api/ai/status
async fn api_vlm_pull(State(app): S) -> Json<Value> {
    tokio::spawn(async move {
        match vlm::ensure(&app.root, &app.http, &app.vlm).await {
            Ok(base) => enrich::set_local_vlm_base(Some(base)),
            Err(e) => println!("👁 内蔵VLM 準備失敗: {e}"),
        }
    });
    Json(json!({"ok": true, "note": "進捗は GET /api/vlm/status"}))
}
async fn api_vlm_stop(State(app): S) -> Json<Value> { vlm::stop(&app.vlm); enrich::set_local_vlm_base(None); Json(json!({"ok": true})) }

async fn api_ai_status(State(app): S) -> Json<Value> {
    Json(ai_status(app).await)
}

/// CLIPモデル(350MB)の事前DLを裏で開始。進捗は /api/ai/status の clip
async fn api_clip_pull(State(app): S) -> Json<Value> {
    let root = app.root.clone();
    let client = app.http.clone();
    tokio::spawn(async move {
        if let Err(e) = onnx::ensure_model(&root, &client).await {
            println!("🧭 CLIPモデルDL失敗: {e}");
        }
        if let Err(e) = onnx::ensure_text_model(&root, &client).await {
            println!("🧭 CLIPテキストモデルDL失敗: {e}");
        }
    });
    Json(json!({"ok": true, "note": "進捗は GET /api/ai/status"}))
}

async fn api_activity(State(app): S) -> Json<Value> {
    let p = &app.ingest;
    let mut crawl = app.crawl.status();
    crawl["queue"] = json!(app.crawl_queue.lock().unwrap().iter().map(|c| album_slug(&c.album)).collect::<Vec<_>>());
    Json(json!({
        "ai": ai_status(app).await,
        "crawl": crawl,
        "gen": app.gen.status(),
        "lora": app.lora.status(),
        "enrich": app.enrich.status(),
        "llm": app.llm.status(&app.root),
        "seg": app.seg.status(),
        "ingest": {
            "alive": p.alive.load(Relaxed), "done": p.done.load(Relaxed), "total": p.total.load(Relaxed),
            "label": app.ingest_label.lock().unwrap().clone(),
        },
        "workers": Value::Object(app.workers.lock().unwrap().clone()),
        "autopilot": {"interval_secs": autopilot_secs(), "next_at": AUTOPILOT_NEXT.load(Relaxed), "per_run_default": 30, "run_minutes": 15, "min_quality": 5},
        "system": sys_stats(),
    }))
}

// ---------- AI生成フォルダ(G1, docs/gen-design.md): 収集の ▶ と対称 ----------

#[derive(Deserialize)]
struct GenIn {
    album: String,
    #[serde(default = "d_gen_n")] n: usize,        // この回で収蔵する枚数
    #[serde(default = "d_gen_min")] minutes: u64,  // 時間上限(1024² は 27 秒/枚なので収集より長め)
}
fn d_gen_n() -> usize { 30 }
fn d_gen_min() -> u64 { 180 }

fn start_gen(app: &'static App, album: &str, n: usize, minutes: u64) -> Result<String, (StatusCode, String)> {
    if app.gen.alive.load(Relaxed) {
        return Err((StatusCode::CONFLICT, "生成は既に実行中です(同時に1本)".into()));
    }
    let slug = album_slug(album);
    let rec = load_albums(&app.root).into_iter().find(|a| a["name"] == json!(slug.clone()));
    let Some(rec) = rec else {
        return Err((StatusCode::NOT_FOUND, format!("フォルダ{slug}が見つかりません")));
    };
    let goal = rec["goal"].as_str().unwrap_or("").to_string();
    if goal.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "作りたい物(目標)が空です".into()));
    }
    let recipe = &rec["recipe"];
    let size_s = recipe["size"].as_str().map(String::from).or_else(|| config::get_str("gen.size")).unwrap_or_else(|| "1024x1024".into());
    let (w, h) = size_s.split_once('x')
        .and_then(|(a, b)| Some((a.trim().parse::<u32>().ok()?, b.trim().parse::<u32>().ok()?)))
        .unwrap_or((1024, 1024));
    let snap = |v: u32| (v.clamp(512, 1536) / 64) * 64; // 潜在空間の都合で 64 の倍数
    let steps = recipe["steps"].as_u64().filter(|v| *v > 0).unwrap_or_else(|| config::get_u64("gen.steps", 0)).clamp(0, 50) as u32; // 0=モデルの既定
    let min_quality = rec["agent"]["min_quality"].as_i64().unwrap_or(5).clamp(1, 10);
    let model = recipe["model"].as_str().filter(|m| gen::MODELS.iter().any(|s| s.id == *m)).map(String::from).unwrap_or_else(gen::default_model_id);
    // 参照(G2): recipe.refs = [{kind:"image", sha} | {kind:"folder", album, k} | {kind:"dataset", name, k}]
    let mut refs = gen::RefPool::default();
    if let Some(arr) = recipe["refs"].as_array() {
        let albums = load_albums(&app.root);
        for r in arr {
            let k = r["k"].as_u64().unwrap_or(1).clamp(1, 3) as usize;
            let (label, shas): (String, Vec<String>) = match r["kind"].as_str().unwrap_or("") {
                "image" => {
                    if let Some(sha) = r["sha"].as_str() {
                        refs.fixed.push(sha.to_string());
                        let c = gen::ref_caption(&app.root, sha);
                        refs.notes.push(if c.is_empty() { "a reference image (keep its subject)".into() } else { c });
                    }
                    continue;
                }
                "folder" => {
                    let name = r["album"].as_str().unwrap_or("");
                    let shas = albums.iter().find(|a| a["name"] == json!(name))
                        .and_then(|a| serde_json::from_value::<Q>(a["criteria"].clone()).ok())
                        .map(|q| query_shas(app, &q)).unwrap_or_default();
                    (format!("folder '{name}'"), shas)
                }
                "dataset" => {
                    let name = r["name"].as_str().unwrap_or("");
                    let d = app.root.join("store/datasets").join(name);
                    let shas: Vec<String> = if name.is_empty() || name.contains('/') { vec![] } else {
                        std::fs::read_dir(d).map(|rd| rd.flatten()
                            .filter(|e| e.path().extension().and_then(|x| x.to_str()) != Some("json"))
                            .filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().into_owned())).collect()).unwrap_or_default()
                    };
                    (format!("dataset '{name}'"), shas)
                }
                _ => continue,
            };
            if shas.is_empty() { continue; }
            let mut shas = shas;
            shas.truncate(400);
            let sample: Vec<String> = shas.iter().take(3).map(|s| gen::ref_caption(&app.root, s)).filter(|c| !c.is_empty()).collect();
            refs.notes.push(format!("{k} random image(s) picked each time from {label} ({} images{})", shas.len(),
                if sample.is_empty() { String::new() } else { format!("; e.g. {}", sample.join(" / ")) }));
            refs.pools.push((shas, k));
        }
        refs.fixed.truncate(4);
    }
    // LoRA(G4): recipe.lora = [{file(stem), scale}]。棚に実在し、親モデルが選んだモデルに合う物だけ(klein 用を Qwen に着せない)
    let mut dropped_lora: Vec<String> = vec![];
    let lora_list: Vec<(String, f32)> = recipe["lora"].as_array().map(|a| a.iter().filter_map(|x| {
        let f = lora::safe_stem(x["file"].as_str()?);
        if !lora::file_path(&app.root, &f).exists() { return None; }
        let base = lora::load_meta(&app.root, &f)["base"].as_str().map(String::from).unwrap_or_else(|| lora::base_from_text(&f).to_string());
        if lora::model_for_base(&base) != Some(model.as_str()) { dropped_lora.push(format!("{f}({base}用)")); return None; } // 親モデル不明は klein 用扱い
        Some((f, x["scale"].as_f64().unwrap_or(1.0).clamp(0.05, 2.0) as f32))
    }).take(4).collect()).unwrap_or_default();
    if !dropped_lora.is_empty() { println!("🧬 親モデル違いで外した LoRA: {}", dropped_lora.join(", ")); }
    let st = app.gen.clone();
    st.alive.store(true, Relaxed);
    st.stop.store(false, Relaxed);
    for a in [&st.planned, &st.generated, &st.rejected, &st.ingested, &st.errors] { a.store(0, Relaxed); }
    st.started_at.store(now_secs(), Relaxed);
    st.recent.lock().unwrap().clear();
    *st.album.lock().unwrap() = slug.clone();
    *st.last.lock().unwrap() = "起動中…".into();
    let limits = gen::Limits { max_n: n.clamp(1, 2000), max_secs: minutes.clamp(1, 720) * 60, w: snap(w), h: snap(h), steps, min_quality };
    tokio::spawn(gen::run(app.root.clone(), app.http.clone(), st, app.llm.clone(), app.enrich.clone(), slug.clone(), goal, model, refs, lora_list, limits));
    *LAST_DROPPED_LORA.lock().unwrap() = dropped_lora;
    Ok(slug)
}
/// 直近の ▶ で親モデル違いのため外した LoRA(UI のトーストに出す)
static LAST_DROPPED_LORA: Mutex<Vec<String>> = Mutex::new(Vec::new());

async fn api_gen(State(app): S, Json(g): Json<GenIn>) -> impl IntoResponse {
    match start_gen(app, &g.album, g.n, g.minutes) {
        Ok(slug) => Json(json!({"ok": true, "album": slug, "dropped_lora": LAST_DROPPED_LORA.lock().unwrap().clone()})).into_response(),
        Err((code, msg)) => (code, Json(json!({"detail": msg}))).into_response(),
    }
}
async fn api_gen_status(State(app): S) -> Json<Value> {
    let mut s = app.gen.status();
    s["engine"] = gen::engine_status(&app.root, &app.gen);
    Json(s)
}
async fn api_gen_stop(State(app): S) -> Json<Value> {
    app.gen.stop.store(true, Relaxed);
    Json(json!({"ok": true}))
}
async fn api_gen_engine(State(app): S) -> Json<Value> { Json(gen::engine_status(&app.root, &app.gen)) }
#[derive(Deserialize, Default)]
struct GenPullIn { #[serde(default)] model: String }
/// モデル 1 式の取得を裏で開始(既定 klein 4B 7.1GB。model 指定で Z-Image / Qwen-Image-Edit も)。進捗は /api/gen/engine, /api/ai/status
async fn api_gen_pull(State(app): S, body: Option<Json<GenPullIn>>) -> Json<Value> {
    let id = body.map(|b| b.model.clone()).filter(|m| !m.is_empty()).unwrap_or_else(gen::default_model_id);
    let id2 = id.clone();
    tokio::spawn(async move {
        let s = gen::spec(&id2);
        if let Err(e) = gen::ensure_models(&app.root, &app.http, &app.gen, s).await {
            println!("🪄 生成モデル取得失敗({}): {e}", s.id);
        } else if gen::cli_bin(&app.root).is_none() && gen::external_base().is_none() {
            if let Err(e) = gen::start_server(&app.root, &app.http, &app.gen, s).await { println!("🪄 生成エンジン起動失敗: {e}"); }
        }
    });
    Json(json!({"ok": true, "model": id, "note": "進捗は GET /api/gen/engine"}))
}
/// 途中経過(sd-cli の --preview が各ステップで書く PNG)。生成中だけ存在する
async fn api_gen_preview(State(app): S) -> impl IntoResponse {
    match std::fs::read(gen::preview_path(&app.root)) {
        Ok(b) if b.len() > 100 => ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "no-store")], b).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}
async fn api_gen_engine_stop(State(app): S) -> Json<Value> { gen::stop_engine(&app.gen); Json(json!({"ok": true})) }

#[derive(Deserialize)]
struct GenPlanIn {
    #[serde(default)] album: String,
    #[serde(default)] goal: String,
    #[serde(default = "d_plan_n")] n: usize,
}
fn d_plan_n() -> usize { 8 }
/// 計画だけ返す(「どんなプロンプトになるか」の下見。台帳の使用済みは避ける)
async fn api_gen_plan(State(app): S, Json(g): Json<GenPlanIn>) -> impl IntoResponse {
    let mut goal = g.goal.clone();
    let mut used: Vec<String> = vec![];
    if !g.album.is_empty() {
        let slug = album_slug(&g.album);
        if let Some(rec) = load_albums(&app.root).into_iter().find(|a| a["name"] == json!(slug.clone())) {
            if goal.is_empty() { goal = rec["goal"].as_str().unwrap_or("").to_string(); }
        }
        used = gen::load_ledger(&app.root, &slug)["prompts"].as_array()
            .map(|a| a.iter().filter_map(|p| p["text"].as_str().map(String::from)).collect()).unwrap_or_default();
    }
    if goal.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": "作りたい物(目標)をください"}))).into_response();
    }
    let prompts = gen::plan(&app.root, &app.http, &app.llm, &goal, &used, g.n.clamp(1, 24), &[], &[]).await;
    Json(json!({"goal": goal, "prompts": prompts})).into_response()
}

// ---------- LoRA 棚(G4, docs/gen-design.md §5) ----------

async fn api_lora_list(State(app): S) -> Json<Value> {
    let albums = load_albums(&app.root);
    Json(json!({"items": lora::list(&app.root, &albums), "state": app.lora.status(),
                "models": gen::MODELS.iter().map(|m| json!({"id": m.id, "label": m.label, "present": gen::model_present(&app.root, m)})).collect::<Vec<_>>()}))
}

#[derive(Deserialize)]
struct LoraImportIn { url: String }
/// URL(Hugging Face / Civitai / 直リンク)から取り込む。裏で走り、進捗は /api/lora の state
async fn api_lora_import(State(app): S, Json(i): Json<LoraImportIn>) -> impl IntoResponse {
    if !i.url.trim().starts_with("http") {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": "URL をください"}))).into_response();
    }
    if app.lora.importing.load(Relaxed) {
        return (StatusCode::CONFLICT, Json(json!({"detail": "別の LoRA を取り込み中です"}))).into_response();
    }
    // 先に URL を解釈して親モデル検問(即答)。DL 自体は裏で
    let key = config::key("civitai");
    let res = match lora::resolve(&app.http, &i.url, key.as_deref()).await {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"detail": e}))).into_response(),
    };
    if lora::model_for_base(&res.base).is_none() {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": format!("親モデル「{}」は内蔵の生成モデルに載りません(対応: FLUX.2 klein 4B / Z-Image / Qwen-Image)", res.base)}))).into_response();
    }
    let url = i.url.clone();
    tokio::spawn(async move {
        match lora::import_url(&app.root, &app.http, &app.lora, &url).await {
            Ok(stem) => println!("🧬 LoRA 取り込み: {stem}"),
            Err(e) => println!("🧬 LoRA 取り込み失敗: {e}"),
        }
    });
    Json(json!({"ok": true, "name": res.name, "base": res.base, "file": res.file_name, "triggers": res.triggers, "note": "裏で取得中(進捗は /api/lora の state)"})).into_response()
}

/// .safetensors のアップロード(棚へのドラッグ&ドロップ)
async fn api_lora_upload(State(app): S, mut mp: axum::extract::Multipart) -> impl IntoResponse {
    let mut done = vec![];
    let mut errs = vec![];
    while let Ok(Some(field)) = mp.next_field().await {
        let fname = field.file_name().unwrap_or("lora.safetensors").to_string();
        if !fname.to_lowercase().ends_with(".safetensors") { errs.push(format!("{fname}: .safetensors だけ")); continue; }
        let Ok(data) = field.bytes().await else { errs.push(format!("{fname}: 読めない")); continue };
        match lora::import_bytes(&app.root, &fname, &data) { Ok(s) => done.push(s), Err(e) => errs.push(format!("{fname}: {e}")) }
    }
    Json(json!({"ok": errs.is_empty(), "added": done, "errors": errs}))
}

async fn api_lora_delete(State(app): S, AxPath(name): AxPath<String>) -> impl IntoResponse {
    let stem = lora::safe_stem(&name);
    if lora::delete(&app.root, &stem) { Json(json!({"ok": true})).into_response() } else { StatusCode::NOT_FOUND.into_response() }
}

/// 試し描き: 内蔵エンジンで 2 題描いてカードの顔にする(生成中は 409)
async fn api_lora_probe(State(app): S, AxPath(name): AxPath<String>) -> impl IntoResponse {
    let stem = lora::safe_stem(&name);
    if !lora::file_path(&app.root, &stem).exists() { return StatusCode::NOT_FOUND.into_response(); }
    if app.gen.alive.swap(true, Relaxed) {
        return (StatusCode::CONFLICT, Json(json!({"detail": "生成中です(終わってから試し描きしてください)"}))).into_response();
    }
    let m = lora::load_meta(&app.root, &stem);
    let base = m["base"].as_str().map(String::from).unwrap_or_else(|| lora::base_from_text(&stem).to_string());
    let Some(model_id) = lora::model_for_base(&base) else {
        app.gen.alive.store(false, Relaxed);
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": format!("親モデル「{base}」は内蔵モデルに載りません")}))).into_response();
    };
    let triggers: Vec<String> = m["triggers"].as_array().map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect()).unwrap_or_default();
    app.gen.stop.store(false, Relaxed);
    *app.gen.album.lock().unwrap() = format!("lora_probe:{stem}");
    *app.gen.last.lock().unwrap() = format!("LoRA 試し描き: {stem}");
    *app.lora.probing.lock().unwrap() = stem.clone();
    let stem2 = stem.clone();
    tokio::spawn(async move {
        let s = gen::spec(model_id);
        for (i, p) in lora::probe_prompts(&triggers).iter().enumerate() {
            if app.gen.stop.load(Relaxed) { break; }
            *app.gen.prompt.lock().unwrap() = p.clone();
            let job = gen::GenJob { prompt: p.clone(), w: 768, h: 768, steps: 0, seed: 7 + i as u64, lora: vec![(stem2.clone(), 1.0)] };
            let job = gen::GenJob { steps: s.steps, ..job };
            match gen::generate_one(&app.root, &app.http, &app.gen, s, &job, &[]).await {
                Ok(png) => lora::save_preview(&app.root, &stem2, i, &png),
                Err(e) => { *app.gen.last.lock().unwrap() = format!("試し描き失敗: {e}"); break; }
            }
        }
        *app.lora.probing.lock().unwrap() = String::new();
        *app.gen.prompt.lock().unwrap() = String::new();
        let _ = std::fs::remove_file(gen::preview_path(&app.root));
        app.gen.alive.store(false, Relaxed);
        println!("🧬 LoRA 試し描き完了: {stem2}");
    });
    Json(json!({"ok": true, "note": "2 枚描きます(1024²換算で約1分)。進捗は /api/gen/status"})).into_response()
}

async fn lora_preview_img(State(app): S, AxPath((name, i)): AxPath<(String, String)>) -> impl IntoResponse {
    let stem = lora::safe_stem(&name);
    let i = if i == "art" { "art".to_string() } else { i.chars().filter(|c| c.is_ascii_digit()).take(1).collect() };
    match std::fs::read(lora::previews_dir(&app.root).join(format!("{stem}_{i}.jpg"))) {
        Ok(b) => ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "no-cache")], b).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------- 設定画面(docs/gen-design.md §8.1): 正本 store/config.json、行ごとに自動保存 ----------

async fn api_settings_get(State(app): S) -> Json<Value> {
    let home = std::env::var("HOME").unwrap_or_default();
    Json(json!({
        "config": config::masked(), "defaults": config::defaults(),
        "path": std::fs::canonicalize(config::path()).unwrap_or_else(|_| config::path()).display().to_string(),
        "root": std::fs::canonicalize(&app.root).unwrap_or(app.root.clone()).display().to_string(),
        "env": config::env_overrides(),
        "legacy_file": std::path::Path::new(&home).join("ml-hub/config/settings.json").exists(),
        "log": app.root.join("fluent_gallery.log").display().to_string(),
        "version": env!("CARGO_PKG_VERSION"),
        "features": {"faceid": cfg!(feature = "faceid"), "store": cfg!(feature = "store"), "metal": cfg!(feature = "metal"), "cuda": cfg!(feature = "cuda")},
        "port": BIND_PORT.load(Relaxed),
        "ai": ai_status(app).await,
        "system": sys_stats(),
    }))
}

#[derive(Deserialize)]
struct SettingIn { path: String, value: Value }

async fn api_settings_patch(Json(s): Json<SettingIn>) -> impl IntoResponse {
    match config::set(&s.path, s.value) {
        Ok(_) => Json(json!({"ok": true, "config": config::masked()})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"detail": e}))).into_response(),
    }
}

#[derive(Deserialize)]
struct SettingTestIn { what: String }

/// 疎通確認(キーは短い一覧取得、接続先はヘルス)。結果は文字列 1 行
async fn api_settings_test(State(app): S, Json(t): Json<SettingTestIn>) -> Json<Value> {
    let c = &app.http;
    let to = std::time::Duration::from_secs(10);
    async fn probe(r: reqwest::RequestBuilder, to: std::time::Duration) -> Result<String, String> {
        let resp = r.timeout(to).send().await.map_err(|e| format!("接続できません: {e}"))?;
        let st = resp.status();
        if st.is_success() { Ok("OK".into()) } else {
            let body = resp.text().await.unwrap_or_default();
            Err(format!("HTTP {} {}", st.as_u16(), body.chars().take(120).collect::<String>()))
        }
    }
    let need = |name: &str| config::key(name).ok_or_else(|| "キー未設定".to_string());
    let r: Result<String, String> = match t.what.as_str() {
        "anthropic" => match need("anthropic") { Ok(k) => probe(c.get("https://api.anthropic.com/v1/models").header("x-api-key", k).header("anthropic-version", "2023-06-01"), to).await, Err(e) => Err(e) },
        "openai" => match need("openai") { Ok(k) => probe(c.get("https://api.openai.com/v1/models").bearer_auth(k), to).await, Err(e) => Err(e) },
        "openrouter" => match need("openrouter") { Ok(k) => probe(c.get("https://openrouter.ai/api/v1/auth/key").bearer_auth(k), to).await, Err(e) => Err(e) },
        "xai" => match need("xai") { Ok(k) => probe(c.get("https://api.x.ai/v1/models").bearer_auth(k), to).await, Err(e) => Err(e) },
        // Pexels は無効キーでも 200 を返す(正誤を判定できない)ので、接続確認だけと明記する
        "pexels" => match need("pexels") { Ok(k) => probe(c.get("https://api.pexels.com/v1/search?query=cat&per_page=1").header("Authorization", k), to).await.map(|_| "接続OK(Pexels はキーの正誤を返さないため、収集で使って確かめてください)".into()), Err(e) => Err(e) },
        "civitai" => match need("civitai") { Ok(k) => probe(c.get("https://civitai.com/api/v1/models?limit=1").bearer_auth(k), to).await, Err(e) => Err(e) },
        "pixabay" => match need("pixabay") { Ok(k) => probe(c.get(format!("https://pixabay.com/api/?key={k}&q=cat&per_page=3")), to).await, Err(e) => Err(e) },
        "gen" => match gen::external_base() {
            Some(b) => if gen::health(c, &b).await { Ok(format!("外部 sd-server OK ({b})")) } else { Err(format!("外部 sd-server に繋がりません ({b})")) },
            None => if gen::cli_bin(&app.root).is_some() { Ok(format!("内蔵 sd-cli(途中経過あり・常駐なし){}", if gen::models_present(&app.root) { "" } else { "・モデル未取得" })) }
                    else if gen::health(c, &gen::base_url()).await { Ok("内蔵 sd-server 稼働中".into()) }
                    else if gen::server_bin(&app.root).is_some() { Ok(format!("内蔵 sd-server(停止中・▶で起動){}", if gen::models_present(&app.root) { "" } else { "・モデル未取得" })) }
                    else { Err("sd-cli / sd-server が見つかりません".into()) },
        },
        "vlm" => match config::env_or("FG_VLM_BASE", "vlm.base") {
            Some(b) => if enrich::local_vlm_ok(c).await { Ok(format!("外部 VLM OK ({b})")) } else { Err(format!("外部 VLM に繋がりません ({b})")) },
            None => if vlm::health(c).await { Ok("内蔵 VLM 稼働中".into()) }
                    else if enrich::any_vlm_available(c).await { Ok("ollama の VLM が使えます".into()) }
                    else if vlm::server_bin(&app.root).is_some() { Ok(format!("内蔵(停止中){}", if vlm::models_present(&app.root) { "" } else { "・モデル未取得" })) }
                    else { Err("llama-server が見つかりません".into()) },
        },
        _ => Err(format!("知らない確認対象: {}", t.what)),
    };
    Json(json!({"ok": r.is_ok(), "detail": match r { Ok(s) => s, Err(e) => e }}))
}

// ---------- 自然言語検索(魔法④): 日本語の要望→内蔵LLMが属性フィルタ+英語FTS語に翻訳 ----------

#[derive(Deserialize)]
struct NlqIn {
    text: String,
}

async fn api_nlq(State(app): S, Json(n): Json<NlqIn>) -> impl IntoResponse {
    let user = format!(
        "画像ライブラリの検索要望を検索条件JSONに翻訳する。要望:「{}」\n\
         キャプションは英語なのでqは英語キーワード(スペース区切り・無ければ空)。\n\
         使えるフィルタ(該当しない物は空文字):\n\
         origin: real|synthetic / animal: dog|cat|bird|fish|horse|rabbit|reptile|insect|farm|wild|other\n\
         gender: male|female|mixed / age_group: child|teen|adult|senior\n\
         framing: closeup|upper_body|full_body|wide / scene: indoor|outdoor|studio|street|nature|abstract\n\
         subject: person|face|animal|food|vehicle|building|object|landscape|text\n\
         style: photo|illustration|anime|3dcg|painting|sketch / min_quality: 数値(高品質なら8、指定なければ0)\n\
         JSONのみ返す: {{\"q\":\"\",\"origin\":\"\",\"animal\":\"\",\"gender\":\"\",\"age_group\":\"\",\"framing\":\"\",\"scene\":\"\",\"subject\":\"\",\"style\":\"\",\"min_quality\":0}}",
        n.text
    );
    match llm::chat(&app.root, &app.http, &app.llm, "Reply with ONLY the requested JSON.", &user, 300).await {
        Ok(text) => {
            let parsed = text
                .find('{')
                .and_then(|a| text.rfind('}').map(|b| (a, b)))
                .and_then(|(a, b)| serde_json::from_str::<Value>(&text[a..=b]).ok());
            match parsed {
                Some(p) => Json(p).into_response(),
                None => (StatusCode::BAD_GATEWAY, Json(json!({"detail": "翻訳結果が壊れています"}))).into_response(),
            }
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"detail": e}))).into_response(),
    }
}

// ---------- 原本の世代整理(prune): 機械が集めた古い低品質データを安全に間引く ----------

#[derive(Deserialize)]
struct PruneIn {
    source: String,     // 必須: 消す対象のsourceプレフィクス(例 "crawl:", "atelier_gen:")
    older_days: f64,    // 必須: これより古いものだけ
    #[serde(default = "d_keepq")] keep_quality: i64, // これ以上のVLM品質は残す(既定7)
    #[serde(default = "d_true")] dry_run: bool,      // 既定は下見
}
fn d_keepq() -> i64 { 7 }

async fn api_prune(State(app): S, Json(p): Json<PruneIn>) -> impl IntoResponse {
    if p.source.trim().is_empty() || p.older_days <= 0.0 {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": "source と older_days は必須です(全消し防止)"})))
            .into_response();
    }
    // 動的アルバムの現メンバーを保護集合に(⭐keep/品質/データセットはprune内で保護)
    let mut protected = std::collections::HashSet::new();
    for a in load_albums(&app.root) {
        if let Ok(q) = serde_json::from_value::<Q>(a["criteria"].clone()) {
            protected.extend(query_shas(app, &q));
        }
    }
    let db = app.db.lock().unwrap();
    let r = store::prune(&app.root, &db, &p.source, p.older_days, p.keep_quality, &protected, p.dry_run);
    Json(r).into_response()
}

// ---------- 書き(全部ノンブロッキング) ----------

#[derive(Deserialize)]
struct IngestIn {
    path: String,
    #[serde(default = "d_import")] source: String,
    #[serde(default)] origin: String,
    #[serde(default)] r#move: bool,
}
fn d_import() -> String { "import".into() }

fn spawn_ingest(app: &'static App, path: PathBuf, source: String, origin: String, mv: bool) -> Result<(), String> {
    if app.ingest.alive.load(Relaxed) {
        return Err("収蔵ジョブが実行中です".into());
    }
    let p = app.ingest.clone();
    p.alive.store(true, Relaxed);
    for a in [&p.total, &p.done, &p.added, &p.dup, &p.bad] {
        a.store(0, Relaxed);
    }
    *app.ingest_label.lock().unwrap() = source.clone();
    tokio::task::spawn_blocking(move || {
        let db = Connection::open(app.root.join("store/index.sqlite")).unwrap();
        store::ensure_schema(&db);
        store::ingest(&app.root, &db, &path, &source, &origin, mv, &p);
        p.alive.store(false, Relaxed);
    });
    Ok(())
}

async fn api_ingest(State(app): S, Json(i): Json<IngestIn>) -> impl IntoResponse {
    let path = PathBuf::from(shellexpand(&i.path));
    if !path.exists() {
        return (StatusCode::NOT_FOUND, Json(json!({"detail": i.path}))).into_response();
    }
    match spawn_ingest(app, path, i.source, i.origin, i.r#move) {
        Ok(()) => Json(json!({"ok": true, "job": "ingest"})).into_response(),
        Err(e) => (StatusCode::CONFLICT, Json(json!({"detail": e}))).into_response(),
    }
}

fn shellexpand(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        format!("{}/{}", std::env::var("HOME").unwrap_or_default(), rest)
    } else {
        p.to_string()
    }
}

async fn api_ingest_status(State(app): S) -> Json<Value> {
    let p = &app.ingest;
    Json(json!({
        "alive": p.alive.load(Relaxed), "total": p.total.load(Relaxed), "done": p.done.load(Relaxed),
        "added": p.added.load(Relaxed), "dup": p.dup.load(Relaxed), "bad": p.bad.load(Relaxed),
        "label": app.ingest_label.lock().unwrap().clone(),
    }))
}

/// 取り込み(サンプルのまとめ取り含む)を途中で止める。走っている分だけ収蔵して終わる
async fn api_ingest_stop(State(app): S) -> impl IntoResponse {
    app.ingest.stop.store(true, Relaxed);
    Json(json!({"ok": true}))
}

// URL取り込み: 利用者が指定した1ページ(または画像URL)に載っている画像を取り込む(検索クローラとは別物)
#[derive(Deserialize)]
struct IngestUrlIn {
    url: String,
    #[serde(default)] source: String,
    #[serde(default = "d_url_max")] max: usize,
}
fn d_url_max() -> usize { 100 }

async fn api_ingest_url(State(app): S, Json(i): Json<IngestUrlIn>) -> impl IntoResponse {
    let bad = |m: String| (StatusCode::BAD_REQUEST, Json(json!({"detail": m}))).into_response();
    let Ok(page) = reqwest::Url::parse(i.url.trim()) else { return bad("URLの形式が不正です".into()) };
    if !matches!(page.scheme(), "http" | "https") { return bad("http/https のURLだけ取り込めます".into()); }
    let host = page.host_str().unwrap_or("").to_string();
    if let Some(b) = urlimport::blocked_host(&host) {
        return bad(format!("{b} は規約でダウンロードが禁止されているため取り込めません"));
    }
    if !crawl::is_safe_url(page.as_str()).await { return bad("内部ネットワーク宛てのURLは取り込めません".into()); }
    if app.ingest.alive.load(Relaxed) {
        return (StatusCode::CONFLICT, Json(json!({"detail": "収蔵ジョブが実行中です"}))).into_response();
    }
    // 動画/SNS媒体(YouTube/X等)は yt-dlp でメディアを落として 0.5fps でフレーム化(フル機能版のみ・ストア版は上で拒否済み)
    if urlimport::media_host(&host) {
        for t in ["yt-dlp", "ffmpeg"] {
            if !tool_present(t) {
                return bad(format!("{t} が見つかりません。動画/SNSの取り込みには yt-dlp と ffmpeg が必要です(Mac: brew install yt-dlp ffmpeg)"));
            }
        }
        let source = if i.source.trim().is_empty() { format!("url:{host}") } else { i.source.trim().to_string() };
        let p = app.ingest.clone();
        p.alive.store(true, Relaxed);
        for a in [&p.total, &p.done, &p.added, &p.dup, &p.bad] { a.store(0, Relaxed); }
        *app.ingest_label.lock().unwrap() = format!("URL(動画): {host} — yt-dlpで取得中");
        let page_s = page.to_string();
        let host_resp = host.clone();
        tokio::spawn(async move {
            let scratch = app.root.join("store/.upload_tmp");
            let _ = std::fs::create_dir_all(&scratch);
            let (sc, pu) = (scratch.clone(), page_s.clone());
            let frames = tokio::task::spawn_blocking(move || crawl::media_frames_from_urls(&sc, &[pu])).await.unwrap_or_default();
            p.total.store(frames.len(), Relaxed);
            *app.ingest_label.lock().unwrap() = format!("URL(動画): {host} — {}コマを収蔵中", frames.len());
            for (data, u, title) in frames {
                let extra = json!({"rights": "unknown", "origin": "real",
                    "crawl": {"url": u, "landing": page_s, "title": title, "engine": "url:media", "query": "", "album": "", "tags": [format!("url:{host}")]}});
                let (root, src) = (app.root.clone(), source.clone());
                let res = tokio::task::spawn_blocking(move || {
                    let db = app.db.lock().unwrap();
                    store::ingest_bytes(&root, &db, &data, "jpg", &src, &extra).map(|_| ())
                }).await.unwrap_or(Err("bad"));
                match res { Ok(()) => p.added.fetch_add(1, Relaxed), Err("dup") => p.dup.fetch_add(1, Relaxed), Err(_) => p.bad.fetch_add(1, Relaxed) };
                p.done.fetch_add(1, Relaxed);
            }
            println!("🔗 URL(動画) {page_s}: +{} (重複{} 不採用{})", p.added.load(Relaxed), p.dup.load(Relaxed), p.bad.load(Relaxed));
            p.alive.store(false, Relaxed);
        });
        return Json(json!({"ok": true, "job": "ingest", "candidates": 0, "media": true, "title": host_resp, "source": ""})).into_response();
    }
    const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15 fluent_gallery/0.2";
    let resp = match app.http.get(page.clone()).header("User-Agent", UA).timeout(std::time::Duration::from_secs(30)).send().await {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, Json(json!({"detail": format!("取得失敗: {e}")}))).into_response(),
    };
    let ctype = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let final_url = resp.url().clone();
    let source = if i.source.trim().is_empty() { format!("url:{host}") } else { i.source.trim().to_string() };
    // 画像URLそのものなら1枚だけ、HTMLならページ内の画像を候補にする
    let (title, cands): (String, Vec<String>) = if ctype.starts_with("image/") {
        (final_url.path().rsplit('/').next().unwrap_or("").to_string(), vec![final_url.to_string()])
    } else {
        let html = resp.text().await.unwrap_or_default();
        let (t, mut u) = urlimport::extract(&final_url, &html);
        u.truncate(i.max.clamp(1, 500));
        (t, u)
    };
    if cands.is_empty() { return bad("画像が見つかりませんでした".into()); }
    let p = app.ingest.clone();
    p.alive.store(true, Relaxed);
    for a in [&p.total, &p.done, &p.added, &p.dup, &p.bad] { a.store(0, Relaxed); }
    p.total.store(cands.len(), Relaxed);
    *app.ingest_label.lock().unwrap() = format!("URL: {}", if title.is_empty() { host.clone() } else { title.clone() });
    let n = cands.len();
    let landing = final_url.to_string();
    let (resp_title, resp_source) = (title.clone(), source.clone());
    tokio::spawn(async move {
        let sem = Arc::new(tokio::sync::Semaphore::new(6));
        let mut js = tokio::task::JoinSet::new();
        for u in cands {
            let (sem, client, root, source, title, landing, host, p) =
                (sem.clone(), app.http.clone(), app.root.clone(), source.clone(), title.clone(), landing.clone(), host.clone(), p.clone());
            js.spawn(async move {
                let _g = sem.acquire().await;
                let fail = || { p.bad.fetch_add(1, Relaxed); p.done.fetch_add(1, Relaxed); };
                if !crawl::is_safe_url(&u).await { return fail(); }
                let r = client.get(&u).header("User-Agent", UA).header("Referer", landing.clone())
                    .header("Accept", "image/avif,image/webp,image/png,image/jpeg,image/*;q=0.8,*/*;q=0.5")
                    .timeout(std::time::Duration::from_secs(60)).send().await.and_then(|r| r.error_for_status());
                let data = match r { Ok(r) => r.bytes().await.ok(), Err(_) => None };
                let Some(data) = data.filter(|d| d.len() > 8 * 1024) else { return fail() };
                // 短辺200未満(アイコン/トラッカー)は捨てる。寸法だけヘッダから読む(軽い)
                let dims = image::ImageReader::new(std::io::Cursor::new(&data[..])).with_guessed_format().ok().and_then(|r| r.into_dimensions().ok());
                let Some((w, h)) = dims else { return fail() };
                if w.min(h) < 200 { return fail(); }
                let ext = if data.starts_with(b"\x89PNG") { "png" } else if data.starts_with(b"RIFF") { "webp" } else if data.starts_with(b"GIF8") { "gif" } else { "jpg" };
                let extra = json!({
                    "rights": "unknown", "origin": "real",
                    "crawl": {"url": u, "landing": landing, "title": title, "engine": "url", "query": "", "album": "", "tags": [format!("url:{host}")]},
                });
                let res = tokio::task::spawn_blocking(move || {
                    let db = app.db.lock().unwrap();
                    store::ingest_bytes(&root, &db, &data, ext, &source, &extra).map(|_| ())
                }).await.unwrap_or(Err("bad"));
                match res { Ok(()) => p.added.fetch_add(1, Relaxed), Err("dup") => p.dup.fetch_add(1, Relaxed), Err(_) => p.bad.fetch_add(1, Relaxed) };
                p.done.fetch_add(1, Relaxed);
            });
        }
        while js.join_next().await.is_some() {}
        println!("🔗 URL取り込み {landing}: +{} (重複{} 不採用{})", p.added.load(Relaxed), p.dup.load(Relaxed), p.bad.load(Relaxed));
        p.alive.store(false, Relaxed);
    });
    Json(json!({"ok": true, "job": "ingest", "candidates": n, "title": resp_title, "source": resp_source})).into_response()
}

// サンプルデータ(権利クリアな公開コレクションからの取得。旧プリセットの置き換え)
async fn api_samples(State(app): S) -> Json<Value> {
    let have: std::collections::HashMap<String, i64> = {
        let db = app.db.lock().unwrap();
        db.prepare("SELECT source, COUNT(*) FROM images WHERE source LIKE 'sample:%' GROUP BY source")
            .and_then(|mut st| st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).map(|rs| rs.flatten().collect()))
            .unwrap_or_default()
    };
    let out: Vec<Value> = samples::sets().iter().map(|x| json!({
        "id": x.id, "label": x.label, "license": x.license, "license_label": x.license_label, "note": x.note,
        "have": have.get(&format!("sample:{}", x.id)).copied().unwrap_or(0),
        "seen": samples::load_seen(&app.root, x.id).len(), // 取りに行った累計(次はこの続きから)
    })).collect();
    Json(json!(out))
}

#[derive(Deserialize)]
struct SamplesQ { #[serde(default = "d_sample_n")] n: usize }
fn d_sample_n() -> usize { 100 }

/// サンプル取得ジョブ: 候補一覧→6並列DL→収蔵。進捗は ingest の器(UIの「取込」行)に出す
async fn api_sample_fetch(State(app): S, AxPath(id): AxPath<String>, axum::extract::Query(q): axum::extract::Query<SamplesQ>) -> impl IntoResponse {
    let Some(set) = samples::sets().into_iter().find(|x| x.id == id) else {
        return (StatusCode::NOT_FOUND, Json(json!({"detail": id}))).into_response();
    };
    if app.ingest.alive.load(Relaxed) {
        return (StatusCode::CONFLICT, Json(json!({"detail": "収蔵ジョブが実行中です"}))).into_response();
    }
    let n = if q.n == 0 { 0 } else { q.n.min(20000) }; // 0=全部(注釈一覧を持つ COCO / Open Images 向け)。API源は2万上限
    let p = app.ingest.clone();
    p.alive.store(true, Relaxed);
    p.stop.store(false, Relaxed);
    for a in [&p.total, &p.done, &p.added, &p.dup, &p.bad] {
        a.store(0, Relaxed);
    }
    let label_short = set.label.split_once(' ').map(|x| x.1).unwrap_or(set.label).to_string();
    *app.ingest_label.lock().unwrap() = format!("サンプル: {label_short}(一覧を取得中)");
    let (sid, license, origin) = (set.id.to_string(), set.license.to_string(), set.origin.to_string());
    tokio::spawn(async move {
        let seen0 = samples::load_seen(&app.root, &sid);
        let list = match samples::fetch_list(&app.http, &app.root, &p, &app.ingest_label, &sid, n, &seen0).await {
            Ok(l) => l,
            Err(e) => { println!("📥 サンプル一覧取得失敗({sid}): {e}"); p.alive.store(false, Relaxed); return; }
        };
        p.total.store(list.len(), Relaxed);
        p.done.store(0, Relaxed);
        *app.ingest_label.lock().unwrap() = format!("サンプル: {}", label_short);
        let got: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![])); // 取れた物のURL(既読台帳に足す)
        let sem = Arc::new(tokio::sync::Semaphore::new(6));
        let mut js = tokio::task::JoinSet::new();
        for it in list {
            let got = got.clone();
            let (sem, client, root, sid, license, origin, p) = (sem.clone(), app.http.clone(), app.root.clone(), sid.clone(), license.clone(), origin.clone(), p.clone());
            js.spawn(async move {
                let _g = sem.acquire().await;
                if p.stop.load(Relaxed) { return; } // 「止める」を押されたら残りは取りに行かない
                // 画像配信CDNはボット扱いで403にすることがある(シカゴ美術館のIIIFが実例)→ブラウザ相当のヘッダで取る
                let r = client.get(&it.url)
                    .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15 fluent_gallery/0.2")
                    .header("Accept", "image/avif,image/webp,image/png,image/jpeg,image/*;q=0.8,*/*;q=0.5")
                    .header("Referer", it.landing.clone())
                    .timeout(std::time::Duration::from_secs(60)).send().await
                    .and_then(|r| r.error_for_status());
                let data = match r { Ok(r) => r.bytes().await.ok(), Err(_) => None };
                let Some(data) = data.filter(|d| d.len() > 4096) else { p.bad.fetch_add(1, Relaxed); p.done.fetch_add(1, Relaxed); return; };
                let ext = if data.starts_with(b"\x89PNG") { "png" } else { "jpg" };
                let extra = json!({
                    "rights": it.rights.clone().unwrap_or(license), "origin": origin, "credit": it.credit,
                    "crawl": {"url": it.url, "landing": it.landing, "title": it.title, "engine": format!("sample:{sid}"),
                              "query": "", "album": "", "tags": [format!("sample:{sid}")]},
                });
                let res = tokio::task::spawn_blocking(move || {
                    let db = app.db.lock().unwrap();
                    store::ingest_bytes(&root, &db, &data, ext, &format!("sample:{sid}"), &extra).map(|_| ())
                }).await.unwrap_or(Err("bad"));
                match res {
                    Ok(()) => { got.lock().unwrap().push(it.url.clone()); p.added.fetch_add(1, Relaxed) }
                    Err("dup") => { got.lock().unwrap().push(it.url.clone()); p.dup.fetch_add(1, Relaxed) }
                    Err(_) => p.bad.fetch_add(1, Relaxed), // 取れなかった物は覚えない(次回また試す)
                };
                p.done.fetch_add(1, Relaxed);
            });
        }
        while js.join_next().await.is_some() {}
        let mut seen = seen0;
        seen.extend(got.lock().unwrap().drain(..));
        samples::save_seen(&app.root, &sid, &seen);
        println!("📥 サンプル {sid}: +{} (重複{} 失敗{}) 既読累計{}", p.added.load(Relaxed), p.dup.load(Relaxed),
                 p.bad.load(Relaxed), seen.len());
        p.stop.store(false, Relaxed);
        p.alive.store(false, Relaxed);
    });
    Json(json!({"ok": true, "job": "ingest", "n": n})).into_response()
}

// データセット払い出し
#[derive(Deserialize)]
struct DatasetIn {
    name: String,
    #[serde(default)] shas: Vec<String>,
    #[serde(default)] folder: String,
    #[serde(flatten)] q: Q,
}

async fn api_dataset_make(State(app): S, Json(d): Json<DatasetIn>) -> impl IntoResponse {
    let shas = if d.shas.is_empty() { query_shas(app, &d.q) } else { d.shas };
    if shas.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": "該当画像がありません"}))).into_response();
    }
    let root = app.root.clone();
    let name = d.name.clone();
    let folder = folder_norm(&d.folder);
    let r = tokio::task::spawn_blocking(move || store::materialize(&root, &name, &shas, &folder)).await.unwrap();
    Json(r).into_response()
}

async fn api_datasets(State(app): S) -> Json<Value> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(app.root.join("store/datasets")) {
        for e in rd.flatten() {
            if let Ok(t) = std::fs::read_to_string(e.path().join("manifest.json")) {
                if let Ok(mut m) = serde_json::from_str::<Value>(&t) {
                    m["dir"] = json!(e.path().to_string_lossy());
                    out.push(m);
                }
            }
        }
    }
    out.sort_by(|a, b| b["created"].as_f64().partial_cmp(&a["created"].as_f64()).unwrap());
    Json(json!(out))
}

async fn api_dataset_del(State(app): S, AxPath(name): AxPath<String>) -> impl IntoResponse {
    if name.contains('/') {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let d = app.root.join("store/datasets").join(&name);
    if !d.exists() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let _ = std::fs::remove_dir_all(d); // symlinkなので本体は消えない
    Json(json!({"ok": true})).into_response()
}

// データセット(出荷)の棚整理。実体はディレクトリ名+manifest。中身(symlink)は触らない
fn dataset_slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .take(48)
        .collect();
    if s.is_empty() { "dataset".into() } else { s }
}

async fn api_dataset_rename(State(app): S, AxPath(name): AxPath<String>, Json(p): Json<AlbumRenameIn>) -> impl IntoResponse {
    if name.contains('/') || p.to.trim().is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "新しい名前をください");
    }
    let dir = app.root.join("store/datasets");
    let (old, new) = (dir.join(&name), dir.join(dataset_slug(p.to.trim())));
    if !old.exists() {
        return err_json(StatusCode::NOT_FOUND, "データセットが見つかりません");
    }
    if old == new {
        return Json(json!({"ok": true, "name": name})).into_response();
    }
    if new.exists() {
        return err_json(StatusCode::CONFLICT, "その名前のデータセットはもうあります");
    }
    if std::fs::rename(&old, &new).is_err() {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, "名前を変えられませんでした");
    }
    let slug = new.file_name().unwrap().to_string_lossy().to_string();
    let mf = new.join("manifest.json");
    if let Some(mut m) = std::fs::read_to_string(&mf).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        m["name"] = json!(slug);
        let _ = std::fs::write(&mf, serde_json::to_string_pretty(&m).unwrap());
    }
    Json(json!({"ok": true, "name": slug})).into_response()
}

async fn api_dataset_move(State(app): S, AxPath(name): AxPath<String>, Json(p): Json<AlbumMoveIn>) -> impl IntoResponse {
    if name.contains('/') {
        return err_json(StatusCode::BAD_REQUEST, "名前が不正です");
    }
    let mf = app.root.join("store/datasets").join(&name).join("manifest.json");
    let mut m = match std::fs::read_to_string(&mf).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        Some(m) => m,
        None => return err_json(StatusCode::NOT_FOUND, "データセットが見つかりません"),
    };
    let folder = folder_norm(&p.folder);
    m["folder"] = json!(folder);
    if std::fs::write(&mf, serde_json::to_string_pretty(&m).unwrap()).is_err() {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, "保存に失敗しました");
    }
    Json(json!({"ok": true, "name": name, "folder": folder})).into_response()
}

async fn api_dataset_shas(State(app): S, AxPath(name): AxPath<String>) -> impl IntoResponse {
    let d = app.root.join("store/datasets").join(&name);
    if name.contains('/') || !d.exists() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let shas: Vec<String> = std::fs::read_dir(d)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) != Some("json"))
                .filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().into_owned()))
                .collect()
        })
        .unwrap_or_default();
    Json(json!({"shas": shas})).into_response()
}

// enrich(VLM属性付け)
#[derive(Deserialize)]
struct EnrichIn {
    #[serde(default = "d_builtin")] backend: String,
    #[serde(default)] n: usize,
    #[serde(default = "d_true")] only_missing: bool,
    #[serde(flatten)] q: Q,
}
fn d_builtin() -> String { "builtin".into() }
fn d_true() -> bool { true }

async fn api_enrich(State(app): S, Json(e): Json<EnrichIn>) -> impl IntoResponse {
    if app.enrich.alive.load(Relaxed) {
        return (StatusCode::CONFLICT, Json(json!({"detail": "enrichが実行中です"}))).into_response();
    }
    let mut q = e.q;
    q.vlm_ = if e.only_missing { "none".into() } else { "stale".into() };
    let mut shas = query_shas(app, &q);
    if e.n > 0 {
        shas.truncate(e.n);
    }
    let st = app.enrich.clone();
    st.alive.store(true, Relaxed);
    st.stop.store(false, Relaxed);
    st.done.store(0, Relaxed);
    st.errors.store(0, Relaxed);
    st.total.store(shas.len(), Relaxed);
    *st.backend.lock().unwrap() = e.backend.clone();
    let backend = e.backend;
    tokio::spawn(async move {
        let client = app.http.clone();
        let mut backend = backend;
        if backend == "builtin" {
            vlm_wake(app).await;
            if let Err(err) = enrich::ensure_builtin(&client).await {
                // 最低保証: ローカルVLMが動かない環境ではGPT APIに自動フォールバック
                if enrich::mlhub_key("openai_api_key").is_some() {
                    *st.last.lock().unwrap() = format!("内蔵VLM不可({err}) → GPTにフォールバック");
                    *st.backend.lock().unwrap() = "gpt(fallback)".into();
                    backend = "gpt".into();
                } else {
                    *st.last.lock().unwrap() =
                        format!("{err} — openai_api_keyを設定すればGPTで代替できます(最低保証)");
                    st.alive.store(false, Relaxed);
                    return;
                }
            }
        }
        for sha1 in shas {
            if st.stop.load(Relaxed) {
                break;
            }
            st.wait_if_yielding().await; // ユーザーが待ってる仕事(開いた画像の判定等)に道を譲る
            let Some(mut m) = store::load_meta(&app.root, &sha1) else { continue };
            let path = store::image_path(&app.root, &sha1, m["ext"].as_str().unwrap_or("png"));
            match enrich::describe(&client, &path, &backend).await {
                Ok(v) => {
                    m["vlm"] = json!({
                        "model": if backend == "builtin" { format!("builtin/{}", enrich::BUILTIN_MODEL) } else { backend.clone() },
                        "ts": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64(),
                        "caption": v["caption"], "tags": v["tags"], "attrs": v["attrs"],
                    });
                    if store::save_meta(&app.root, &m).is_ok() {
                        store::index_meta(&app.db.lock().unwrap(), &m);
                    }
                    *st.last.lock().unwrap() = v["caption"].as_str().unwrap_or("").chars().take(80).collect();
                }
                Err(err) => {
                    st.errors.fetch_add(1, Relaxed);
                    *st.last.lock().unwrap() = err;
                }
            }
            st.done.fetch_add(1, Relaxed);
        }
        st.alive.store(false, Relaxed);
    });
    Json(json!({"ok": true})).into_response()
}

/// 遅延エンリッチ: 開いた1枚に属性が無ければその場で見る(ライトボックスから叩かれる)。
/// バッチjobのロックとは独立(ollamaが並びを捌く)。済みなら即返し。
#[derive(Deserialize)]
struct EnrichOneIn {
    sha1: String,
    #[serde(default = "d_builtin")] backend: String,
}

async fn api_enrich_one(State(app): S, Json(e): Json<EnrichOneIn>) -> impl IntoResponse {
    let Some(mut m) = store::load_meta(&app.root, &e.sha1) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if m["vlm"].is_object() {
        return Json(m).into_response(); // もう見てある
    }
    let mut backend = e.backend;
    if backend == "builtin" { vlm_wake(app).await; }
    if backend == "builtin" && enrich::ensure_builtin(&app.http).await.is_err() {
        if enrich::mlhub_key("openai_api_key").is_some() {
            backend = "gpt".into(); // 最低保証
        } else {
            return (StatusCode::BAD_GATEWAY, Json(json!({"detail": "VLM不可(内蔵なし・キーなし)"}))).into_response();
        }
    }
    app.enrich.user_priority(8); // 人が画面で待ってる — バックフィルは8秒どいて
    let path = store::image_path(&app.root, &e.sha1, m["ext"].as_str().unwrap_or("png"));
    match enrich::describe(&app.http, &path, &backend).await {
        Ok(v) => {
            m["vlm"] = json!({
                "model": if backend == "builtin" { format!("builtin/{}", enrich::BUILTIN_MODEL) } else { backend },
                "ts": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64(),
                "caption": v["caption"], "tags": v["tags"], "attrs": v["attrs"],
            });
            if store::save_meta(&app.root, &m).is_ok() {
                store::index_meta(&app.db.lock().unwrap(), &m);
            }
            Json(m).into_response()
        }
        Err(err) => (StatusCode::BAD_GATEWAY, Json(json!({"detail": err}))).into_response(),
    }
}

/// 属性の手動修正(AIの間違いを人が上書き — 人の判定が常に勝つ)
#[derive(Deserialize)]
struct MetaPatchIn {
    sha1: String,
    #[serde(default)] style: String, // photo|illustration|anime|3dcg|painting|sketch
    #[serde(default)] clear_seg: bool, // 間違いマスクを消す
    #[serde(default)] add_tags: Vec<String>, // 手動タグ追加
    #[serde(default)] del_tags: Vec<String>, // 手動タグ削除
    #[serde(default)] del_seg_at: Option<Vec<f64>>, // [x,y]正規化 — その点を含むマスクだけ消す(右クリック)
}

/// 偶奇判定の点-in-多角形(pointsは正規化flat [x,y,...])
fn point_in_poly(px: f64, py: f64, pts: &[f64]) -> bool {
    let n = pts.len() / 2;
    if n < 3 {
        return false;
    }
    let (mut inside, mut j) = (false, n - 1);
    for i in 0..n {
        let (xi, yi) = (pts[2 * i], pts[2 * i + 1]);
        let (xj, yj) = (pts[2 * j], pts[2 * j + 1]);
        if (yi > py) != (yj > py) && px < (xj - xi) * (py - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

async fn api_meta_patch(State(app): S, Json(p): Json<MetaPatchIn>) -> impl IntoResponse {
    let Some(mut m) = store::load_meta(&app.root, &p.sha1) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !p.style.is_empty() {
        if !["photo", "illustration", "anime", "3dcg", "painting", "sketch"].contains(&p.style.as_str()) {
            return (StatusCode::BAD_REQUEST, Json(json!({"detail": "styleが不正"}))).into_response();
        }
        if !m["vlm"].is_object() {
            m["vlm"] = json!({"model": "human", "attrs": {}});
        }
        m["vlm"]["attrs"]["style"] = json!(p.style);
        m["vlm"]["human_edited"] = json!(true); // 再エンリッチでも人の修正は尊重したい(将来のマージ用の印)
    }
    if !p.add_tags.is_empty() || !p.del_tags.is_empty() {
        if !m["vlm"].is_object() {
            m["vlm"] = json!({"model": "human", "attrs": {}, "tags": []});
        }
        let mut tags: Vec<String> = m["vlm"]["tags"]
            .as_array()
            .map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect())
            .unwrap_or_default();
        for t in &p.add_tags {
            let t = t.trim();
            if !t.is_empty() && !tags.iter().any(|x| x == t) {
                tags.push(t.to_string());
            }
        }
        tags.retain(|t| !p.del_tags.iter().any(|d| d == t));
        m["vlm"]["tags"] = json!(tags);
        m["vlm"]["human_edited"] = json!(true);
    }
    if let Some(at) = &p.del_seg_at {
        if at.len() == 2 {
            if let Some(shapes) = m["seg"]["shapes"].as_array() {
                let kept: Vec<Value> = shapes
                    .iter()
                    .filter(|s| {
                        let pts: Vec<f64> = s["points"].as_array().map(|a| a.iter().filter_map(|v| v.as_f64()).collect()).unwrap_or_default();
                        !point_in_poly(at[0], at[1], &pts)
                    })
                    .cloned()
                    .collect();
                m["seg"]["shapes"] = json!(kept);
                edits::clear_renders(&app.root, &p.sha1);
            }
        }
    }
    if p.clear_seg {
        if let Some(o) = m.as_object_mut() {
            o.remove("seg");
        }
        edits::clear_renders(&app.root, &p.sha1);
    }
    if store::save_meta(&app.root, &m).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    store::index_meta(&app.db.lock().unwrap(), &m);
    Json(m).into_response()
}

async fn api_enrich_status(State(app): S) -> Json<Value> {
    Json(app.enrich.status())
}

async fn api_enrich_stop(State(app): S) -> Json<Value> {
    app.enrich.stop.store(true, Relaxed);
    Json(json!({"ok": true}))
}

// genvar(工房のGPUキューへ依頼。M-nextでengine内製化)
#[derive(Deserialize)]
struct GenVarIn {
    shas: Vec<String>,
    instruction: String,
    #[serde(default = "d_per")] per_ref: i64,
    #[serde(default)] name: String,
}
fn d_per() -> i64 { 4 }

async fn api_genvar(State(app): S, Json(g): Json<GenVarIn>) -> impl IntoResponse {
    // 旧 genvar(工房 :8772 へ依頼)は廃止。参照画像つきの生成フォルダを作って内蔵エンジンで始める(docs/gen-design.md G2)
    if g.shas.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": "参考画像を選んでください"}))).into_response();
    }
    if g.instruction.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": "どう作り変えるか書いてください"}))).into_response();
    }
    let base = if g.name.trim().is_empty() { format!("変種_{}", &g.shas[0][..6.min(g.shas[0].len())]) } else { g.name.trim().to_string() };
    let slug = album_slug(&base);
    let refs: Vec<Value> = g.shas.iter().take(8).map(|s| json!({"kind": "image", "sha": s})).collect();
    let n = (g.shas.len().min(8) * g.per_ref.clamp(1, 32) as usize).clamp(1, 500);
    let rec = json!({"name": slug, "criteria": {"source": format!("gen:{slug}")}, "folder": "", "goal": g.instruction.trim(),
                     "kind": "gen", "recipe": {"refs": refs}, "agent": {"auto": false, "target": n, "batch": n},
                     "keywords": [], "engines": [],
                     "created": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()});
    if !save_album(&app.root, &rec) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"detail": "フォルダを作れませんでした"}))).into_response();
    }
    match start_gen(app, &slug, n, 240) {
        Ok(_) => Json(json!({"ok": true, "name": slug, "n": n, "est_minutes": (n as f64 * 0.5).ceil(), "note": "生成フォルダを作って始めました"})).into_response(),
        Err((code, msg)) => (code, Json(json!({"detail": msg, "name": slug}))).into_response(),
    }
}

// 保守
async fn api_rebuild(State(app): S) -> Json<Value> {
    let root = app.root.clone();
    let n = tokio::task::spawn_blocking(move || {
        let db = Connection::open(root.join("store/index.sqlite")).unwrap();
        store::ensure_schema(&db);
        store::rebuild(&root, &db)
    })
    .await
    .unwrap();
    Json(json!({"indexed": n}))
}

fn cache_cap_mb() -> u64 {
    std::env::var("FG_CACHE_MB").ok().and_then(|v| v.parse().ok()).unwrap_or_else(|| config::get_u64("storage.cache_mb", 20 * 1024)) // 既定20GB
}

/// ストアがあるFSの空きバイト(dfで取る — 外部クレート不要)
fn disk_free_bytes(root: &std::path::Path) -> Option<u64> {
    let out = std::process::Command::new("df").arg("-Pk").arg(root).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let avail_kb: u64 = s.lines().nth(1)?.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail_kb * 1024)
}

async fn api_cache_stats(State(app): S) -> Json<Value> {
    let du = |p: PathBuf, suffix: Option<&str>| -> u64 {
        fn walk(p: &std::path::Path, suffix: Option<&str>) -> u64 {
            std::fs::read_dir(p)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| {
                            let p = e.path();
                            if p.is_dir() {
                                walk(&p, suffix)
                            } else if suffix.map(|s| p.to_string_lossy().ends_with(s)).unwrap_or(true) {
                                e.metadata().map(|m| m.len()).unwrap_or(0)
                            } else {
                                0
                            }
                        })
                        .sum()
                })
                .unwrap_or(0)
        }
        walk(&p, suffix)
    };
    let thumbs = du(app.root.join("store/thumbs"), None);
    let previews = du(app.root.join("store/thumbs"), Some(".p.jpg"));
    Json(json!({
        "images_mb": du(app.root.join("store/images"), None) >> 20,
        "thumbs_mb": (thumbs - previews) >> 20,
        "previews_mb": previews >> 20,
        "renders_mb": du(app.root.join("store/renders"), None) >> 20,
        "cache_cap_mb": cache_cap_mb(), // preview+renders合算の上限。超過は古い順に自動間引き(ジャニター)
        "disk_free_gb": disk_free_bytes(&app.root).map(|b| b >> 30),
    }))
}

async fn api_cache_clean(State(app): S) -> Json<Value> {
    for d in ["store/thumbs", "store/renders"] {
        let _ = std::fs::remove_dir_all(app.root.join(d));
    }
    // 一時_refsも掃除(古いものだけ)
    if let Ok(rd) = std::fs::read_dir(app.root.join("store/datasets")) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with("_refs_") {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    Json(json!({"ok": true, "note": "サムネは表示時/次回ingest時に再生成されます"}))
}

/// ビルド時機能(Cargo feature)。UIは無効な機能の操作を隠す(販売ビルドで顔ID等を外すため)
async fn api_caps() -> impl IntoResponse {
    Json(json!({"faceid": cfg!(feature = "faceid"), "store": cfg!(feature = "store"), "version": env!("CARGO_PKG_VERSION")}))
}

async fn index_page(State(app): S) -> impl IntoResponse {
    match std::fs::read_to_string(app.root.join("web/index.html")) {
        // no-store: UI更新が普通のリロードで必ず届くように(古いUIがキャッシュから出続ける事故防止)
        Ok(t) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], t).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[tokio::main]
async fn main() {
    let root = std::env::current_exe()
        .ok()
        .and_then(|p| p.ancestors().find(|a| a.join("store").exists()).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    config::init(&root); // 設定の正本(store/config.json)を読む。以後 config::get_* で参照
    let db = Connection::open(root.join("store/index.sqlite")).expect("index.sqlite");
    store::ensure_schema(&db);
    let app: &'static App = Box::leak(Box::new(App {
        db: Mutex::new(db),
        root,
        ingest: Arc::new(store::Progress::default()),
        ingest_label: Mutex::new(String::new()),
        enrich: Arc::new(enrich::EnrichState::default()),
        crawl: Arc::new(crawl::CrawlState::default()),
        crawl_queue: Mutex::new(Vec::new()),
        gen: Arc::new(gen::GenState::default()),
        lora: Arc::new(lora::LoraState::default()),
        llm: Arc::new(llm::LlmState::default()),
        vlm: Arc::new(vlm::VlmState::default()),
        seg: Arc::new(seg::SegState::default()),
        http: reqwest::Client::new(),
        ui_hot: std::sync::atomic::AtomicU64::new(0),
        micro_inflight: Mutex::new(std::collections::HashSet::new()),
        atlas_inflight: Mutex::new(std::collections::HashSet::new()),
        workers: Mutex::new(serde_json::Map::new()),
    }));
    #[cfg(feature = "faceid")]
    faceid::set_root(&app.root);
    let router = Router::new()
        .route("/", get(index_page))
        .route("/api/images", get(api_images))
        .route("/api/facets", get(api_facets))
        .route("/api/meta/{sha1}", get(api_meta))
        .route("/img/{sha1}", get(img))
        .route("/dl/{sha1}/{fname}", get(dl_img))
        .route("/api/export", post(api_export))
        .route("/api/export/{id}", get(api_export_status))
        .route("/export/{id}/{fname}", get(export_zip))
        .route("/api/images/shas", get(api_images_shas))
        .route("/thumb/{sha1}", get(thumb))
        .route("/preview/{sha1}", get(preview))
        .route("/render/{sha1}", get(render_img))
        .route("/api/edits/{sha1}", get(api_edits_get).put(api_edits_put))
        .route("/api/keep", post(api_keep))
        .route("/api/trash", post(api_trash).get(api_trash_list))
        .route("/api/trash/restore", post(api_trash_restore))
        .route("/api/source/trash", post(api_source_trash))
        .route("/api/move", post(api_move))
        .route("/trash/img/{sha1}", get(trash_img))
        .route("/api/albums", post(api_album_make).get(api_albums))
        .route("/api/albums/{name}", delete(api_album_del))
        .route("/api/albums/{name}/rename", post(api_album_rename))
        .route("/api/albums/{name}/move", post(api_album_move))
        .route("/api/albums/merge", post(api_album_merge))
        .route("/api/folders/rename", post(api_folder_rename))
        .route("/api/prune", post(api_prune))
        .route("/api/crawl", post(api_crawl))
        .route("/api/crawl/status", get(api_crawl_status))
        .route("/api/crawl/stop", post(api_crawl_stop))
        .route("/api/crawl/ledger/clear", post(api_ledger_clear))
        .route("/api/gen", post(api_gen))
        .route("/api/gen/status", get(api_gen_status))
        .route("/api/gen/stop", post(api_gen_stop))
        .route("/api/gen/plan", post(api_gen_plan))
        .route("/api/gen/engine", get(api_gen_engine))
        .route("/api/gen/engine/stop", post(api_gen_engine_stop))
        .route("/api/gen/pull", post(api_gen_pull))
        .route("/api/gen/preview", get(api_gen_preview))
        .route("/api/lora", get(api_lora_list))
        .route("/api/lora/import", post(api_lora_import))
        .route("/api/lora/upload", post(api_lora_upload).layer(axum::extract::DefaultBodyLimit::max(3 << 30)))
        .route("/api/lora/{name}", delete(api_lora_delete))
        .route("/api/lora/{name}/probe", post(api_lora_probe))
        .route("/lora/preview/{name}/{i}", get(lora_preview_img))
        .route("/api/settings", get(api_settings_get).patch(api_settings_patch))
        .route("/api/settings/test", post(api_settings_test))
        .route("/crawl/reject/{uk}", get(crawl_reject_thumb))
        .route("/api/activity", get(api_activity))
        .route("/api/nlq", post(api_nlq))
        .route("/api/llm/status", get(api_llm_status))
        .route("/api/llm/pull", post(api_llm_pull))
        .route("/api/llm/test", post(api_llm_test))
        .route("/api/upload", post(api_upload).layer(axum::extract::DefaultBodyLimit::max(2 << 30)))
        .route("/api/ingest", post(api_ingest))
        .route("/api/ingest/status", get(api_ingest_status))
        .route("/api/ingest/stop", post(api_ingest_stop))
        .route("/api/ingest/url", post(api_ingest_url))
        .route("/api/samples", get(api_samples))
        .route("/api/samples/{id}", post(api_sample_fetch))
        .route("/api/datasets", post(api_dataset_make).get(api_datasets))
        .route("/api/datasets/{name}", delete(api_dataset_del))
        .route("/api/datasets/{name}/shas", get(api_dataset_shas))
        .route("/api/datasets/{name}/rename", post(api_dataset_rename))
        .route("/api/datasets/{name}/move", post(api_dataset_move))
        .route("/api/enrich", post(api_enrich))
        .route("/api/enrich/one", post(api_enrich_one))
        .route("/api/meta/patch", post(api_meta_patch))
        .route("/api/seg", post(api_seg))
        .route("/api/seg/one", post(api_seg_one))
        .route("/api/seg/refine", post(api_seg_refine))
        .route("/micro/{sha1}", get(micro))
        .route("/atlas/{key}", get(atlas))
        .route("/cutout/{sha1}", get(cutout))
        .route("/api/seg/stop", post(api_seg_stop))
        .route("/api/enrich/status", get(api_enrich_status))
        .route("/api/enrich/stop", post(api_enrich_stop))
        .route("/api/genvar", post(api_genvar))
        .route("/api/rebuild", post(api_rebuild))
        .route("/api/cache/stats", get(api_cache_stats))
        .route("/api/cache/clean", post(api_cache_clean))
        .route("/api/caps", get(api_caps))
        .route("/api/ai/status", get(api_ai_status))
        .route("/api/vlm/status", get(api_vlm_status))
        .route("/api/vlm/pull", post(api_vlm_pull))
        .route("/api/vlm/stop", post(api_vlm_stop))
        .route("/api/clip/pull", post(api_clip_pull));
    // 顔ID(feature "faceid")。無効ビルドではルート自体が無い=404。UIは/api/capsを見て操作を隠す
    #[cfg(feature = "faceid")]
    let router = router
        .route("/api/faces/pull", post(api_faces_pull))
        .route("/api/faces", get(api_faces_list).delete(api_faces_delete))
        .route("/api/faces/enroll", post(api_faces_enroll))
        .route("/api/faces/detect", post(api_faces_detect))
        .route("/api/faces/scan", post(api_faces_scan));
    let router = router
        .layer(axum::middleware::from_fn(log_client_errors))
        .with_state(app);
    // キャッシュジャニター常駐: preview+renders(全て再生成可能)をFG_CACHE_MB以下に保つ。
    // 原本とgrid360サムネは対象外。1000万枚時代の正解はM7(S3/R2階層)、これはローカルの安全弁。
    tokio::spawn(async {
        let root = std::env::current_exe()
            .ok()
            .and_then(|p| p.ancestors().find(|a| a.join("store").exists()).map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        loop {
            let r = root.clone();
            let (freed, n, trash) = tokio::task::spawn_blocking(move || {
                let (f, n) = store::cache_janitor(&r, cache_cap_mb());
                let t = store::empty_old_trash(&r); // prune退避分は30日でゴミ箱から消える
                (f, n, t)
            })
            .await
            .unwrap_or((0, 0, 0));
            if n > 0 || trash > 0 {
                println!("🧹 janitor: キャッシュ{}MB/{n}件 + ゴミ箱{}MB", freed >> 20, trash >> 20);
            }
            tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)).await;
        }
    });
    // 収集キューの番人: 走ってる収集が終わったら順番待ちの次を自動で始める
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            if app.crawl.alive.load(Relaxed) {
                continue;
            }
            let next = {
                let mut q = app.crawl_queue.lock().unwrap();
                if q.is_empty() { None } else { Some(q.remove(0)) } // 先入れ先出し
            };
            if let Some(c) = next {
                if let Ok(slug) = start_crawl(app, &c.album, c.n, c.minutes, c.min_quality) {
                    println!("⏭ 順番待ちから収集開始: {slug}");
                }
            }
        }
    });
    // micro(120px)バックフィル常駐: 既存分を低優先で焼き切る(俯瞰グリッドのmiss生成を0にする)。
    // 新規ingestはwrite_thumbsで両tier同時焼きなので、これは既存分の一度きり+安全弁。
    // UIが直近10秒に触られていたら遠慮して待つ(閲覧中のCPU/IO競合を作らない)。
    {
        let root = app.root.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(120)).await;
            let mut total = 0usize;
            loop {
                if app.ui_recent(10) {
                    if total > 0 {
                        app.set_worker("micro", false, "閲覧中は遠慮".into());
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
                let r = root.clone();
                let n = tokio::task::spawn_blocking(move || store::micro_backfill(&r, 2000)).await.unwrap_or(0);
                total += n;
                if n == 0 {
                    if total > 0 {
                        println!("🔬 microバックフィル完了 +{total}");
                        total = 0;
                    }
                    app.set_worker("micro", false, "全数あり".into());
                    tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)).await; // 以後は安全弁の見回り
                } else {
                    app.set_worker("micro", true, format!("小サムネ焼き +{total}"));
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await; // 平準化
                }
            }
        });
    }
    // CLIP埋め込みバックフィル常駐: 空いた時間に少しずつ(似た画像の索引)。CPUのみ=GPUの邪魔をしない
    {
        let root = app.root.clone();
        tokio::spawn(async move {
            // 起動直後はUI立ち上がり優先(バックフィルが即全開だと起動が重い)
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let mut total = 0usize;
            loop {
                let r = root.clone();
                let n = tokio::task::spawn_blocking(move || embed_backfill(&r, 2000)).await.unwrap_or(0);
                total += n;
                if n > 0 {
                    println!("🧭 CLIP埋め込み +{n}");
                    app.set_worker("clip", true, format!("似た画像の索引 +{total}"));
                } else {
                    app.set_worker("clip", false, "全数あり".into());
                    total = 0;
                }
                tokio::time::sleep(std::time::Duration::from_secs(if n == 0 { 300 } else { 2 })).await;
            }
        });
    }
    // 自動お手入れ常駐: 取り込まれた画像へ (1)VLM情報(enrich) (2)マスク(gdino2seg) を人手なしで付ける
    // (2026-09-03指示「取り込んだら、マスクと情報取得は自動で」)。収集中は内蔵VLMを取り合うので待つ。
    // 1tick=1仕事(enrich優先→次tickでマスク)・マスクは15分に1回まで(ml-hub側サービス停止時の連打防止)
    tokio::spawn(async move {
        let mut last_seg = std::time::Instant::now() - std::time::Duration::from_secs(3600);
        tokio::time::sleep(std::time::Duration::from_secs(90)).await;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(180)).await;
            if !config::get_bool("autopilot.groom", true) {
                app.set_worker("groom", false, "OFF(設定)".into());
                continue;
            }
            if app.crawl.alive.load(Relaxed) || app.enrich.alive.load(Relaxed) || app.seg.alive.load(Relaxed) {
                continue;
            }
            let missing: i64 = {
                let db = app.db.lock().unwrap();
                db.query_row("SELECT COUNT(*) FROM images WHERE vlm_model IS NULL", [], |r| r.get(0)).unwrap_or(0)
            };
            if missing > 0 {
                // VLM が一つも無い環境(Mac で内蔵VLM未取得・ollama無し・キー無し)では 3 分ごとに空振りしない
                if !enrich::any_vlm_available(&app.http).await {
                    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                    if !SAID.swap(true, Relaxed) { println!("🤖 自動エンリッチ待機: VLM が無い(AI配役で内蔵VLMを取得すると始まります) 未取得{missing}"); }
                    app.set_worker("groom", false, format!("待機: VLM無し(未取得{missing})"));
                    continue;
                }
                println!("🤖 自動エンリッチ開始(未取得{missing})");
                app.set_worker("groom", true, format!("属性の穴埋め依頼(残{missing})"));
                let _ = app.http.post(format!("http://127.0.0.1:{}/api/enrich", BIND_PORT.load(Relaxed)))
                    .json(&json!({"backend": "builtin", "n": 300}))
                    .send()
                    .await;
                continue;
            }
            app.set_worker("groom", false, "見回り済".into());
            if last_seg.elapsed().as_secs() < 900 {
                continue;
            }
            // マスク: AIフォルダ(goal持ち)のsourceで未マスクを探し、1フォルダだけ依頼(seg::runは同お題済みをスキップ)
            for a in load_albums(&app.root) {
                if a["goal"].as_str().unwrap_or("").is_empty() {
                    continue;
                }
                let src = a["criteria"]["source"].as_str().unwrap_or("").to_string();
                if src.is_empty() {
                    continue;
                }
                let n: i64 = {
                    let db = app.db.lock().unwrap();
                    db.query_row("SELECT COUNT(*) FROM images WHERE source=?1 AND (seg IS NULL OR seg=0)",
                                 [&src], |r| r.get(0))
                        .unwrap_or(0)
                };
                if n > 0 {
                    let name = a["name"].as_str().unwrap_or("").to_string();
                    println!("🤖 自動マスク開始: {name} (未マスク{n})");
                    last_seg = std::time::Instant::now();
                    let _ = app.http.post(format!("http://127.0.0.1:{}/api/seg", BIND_PORT.load(Relaxed)))
                        .json(&json!({"album": name}))
                        .send()
                        .await;
                    break;
                }
            }
        }
    });
    // オートパイロット: ♻自動ONのAIフォルダを30分毎に見回り、目標枚数に足りなければ補充クロール。
    // 「置いとくと増える・キュレーションで消してもまた埋まる」の実体。一度に走るのは1フォルダ(直列)。
    tokio::spawn(async move {
        loop {
            // 次回の見回り時刻を黒板に出す(UI の「自動収集: 30分ごと・次回 N 分後」用)
            let secs = autopilot_secs();
            let next = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + secs;
            AUTOPILOT_NEXT.store(next, Relaxed);
            tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
            // 生成フォルダ(kind=gen)の補充: 収集とは別の資源(sd-server)なので独立に 1 本。VLM が無くても走る
            if !app.gen.alive.load(Relaxed) && (gen::models_present(&app.root) && (gen::cli_bin(&app.root).is_some() || gen::server_bin(&app.root).is_some()) || gen::external_base().is_some()) {
                for a in load_albums(&app.root) {
                    if a["kind"].as_str() != Some("gen") || !a["agent"]["auto"].as_bool().unwrap_or(false) {
                        continue;
                    }
                    let name = a["name"].as_str().unwrap_or("").to_string();
                    if name.is_empty() || a["goal"].as_str().unwrap_or("").is_empty() {
                        continue;
                    }
                    let target = a["agent"]["target"].as_i64().unwrap_or(200).max(1) as usize;
                    let count = serde_json::from_value::<Q>(a["criteria"].clone()).map(|q| query_shas(app, &q).len()).unwrap_or(0);
                    if count >= target {
                        continue;
                    }
                    let per_run = a["agent"]["batch"].as_i64().unwrap_or(30).clamp(1, 500) as usize;
                    let batch = (target - count).min(per_run);
                    if let Ok(slug) = start_gen(app, &name, batch, 180) {
                        println!("♻ autopilot: {slug} を補充生成({count}/{target} → +{batch}目標)");
                        app.set_worker("autopilot", true, format!("{slug} 生成+{batch}"));
                        break;
                    }
                }
            }
            if app.crawl.alive.load(Relaxed) {
                continue;
            }
            if !enrich::any_vlm_available(&app.http).await {
                app.set_worker("autopilot", false, "休止: VLM無し(内蔵VLMを取得すると再開)".into());
                continue;
            }
            app.set_worker("autopilot", false, "見回り済".into());
            for a in load_albums(&app.root) {
                if !a["agent"]["auto"].as_bool().unwrap_or(false) {
                    continue;
                }
                let name = a["name"].as_str().unwrap_or("").to_string();
                let goal = a["goal"].as_str().unwrap_or("");
                if name.is_empty() || goal.is_empty() {
                    continue;
                }
                let target = a["agent"]["target"].as_i64().unwrap_or(200).max(1) as usize;
                let count = serde_json::from_value::<Q>(a["criteria"].clone())
                    .map(|q| query_shas(app, &q).len())
                    .unwrap_or(0);
                if count >= target {
                    continue;
                }
                let per_run = a["agent"]["batch"].as_i64().unwrap_or(30).clamp(1, 500) as usize;
                let batch = (target - count).min(per_run); // 1回の補充量はフォルダ設定(既定30)
                if let Ok(slug) = start_crawl(app, &name, batch, 15, 5) {
                    println!("♻ autopilot: {slug} を補充クロール({count}/{target} → +{batch}目標)");
                    app.set_worker("autopilot", true, format!("{slug} 補充+{batch}"));
                    break; // 直列: 次のtickで次のフォルダ
                }
            }
        }
    });
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8790);
    BIND_PORT.store(port, Relaxed);
    // 内蔵VLM: モデルと llama-server が揃っていれば裏で起動しておく(初回の属性付けを待たせない)。無ければ何もしない
    if vlm::models_present(&app.root) && vlm::server_bin(&app.root).is_some() {
        tokio::spawn(async move { vlm_wake(app).await; });
    } else if vlm::server_bin(&app.root).is_none() {
        println!("👁 内蔵VLM: llama-server が見つからないため無効(brew install llama.cpp か .app 同梱)");
    }
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await.unwrap();
    println!("🖼 fluent_gallery (rust) on :{port}");
    axum::serve(listener, router).await.unwrap();
}

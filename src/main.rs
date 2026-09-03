//! fluent_gallery — 画像ライブラリ本体(Rust)。
//! 原則: 人を待たせない(重い処理は全てバックグラウンドジョブ+進捗)、UIをロックしない、
//!       正本はサイドカー・SQLiteは使い捨て索引、AI 1st(全操作がAPI=MCP化可能)。

mod crawl;
mod edits;
mod enrich;
mod llm;
mod media;
mod faceid;
mod onnx;
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
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
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
    llm: Arc<llm::LlmState>,
    seg: Arc<seg::SegState>,
    http: reqwest::Client,
    ui_hot: std::sync::atomic::AtomicU64, // 最後にUIが画像/一覧を要求したunix秒(backfillの遠慮判断)
    micro_inflight: Mutex<std::collections::HashSet<String>>, // /micro miss生成のsingle-flight
    workers: Mutex<serde_json::Map<String, Value>>, // 裏方常駐の自己申告黒板(AI稼働ボードに出す)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl App {
    fn touch_ui(&self) {
        self.ui_hot.store(now_secs(), Relaxed);
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

// ---------- 読み ----------

#[derive(Deserialize, Default)]
struct Q {
    #[serde(default)] q: String,
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
    #[serde(default = "d_limit")] limit: i64,
    #[serde(default)] offset: i64,
}
fn d_limit() -> i64 { 200 }

const COLS: &str = "sha1, ext, w, h, bytes, phash, source, origin, ingested, tint, vlm_model, caption, quality, nsfw, scene, subject, lighting, style, keep, cost, rights, gender, people_count, age_group, framing, watermark, animal, erev";

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
    db.prepare(&format!("SELECT sha1 FROM images WHERE {cond} ORDER BY ingested DESC"))
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
        .map(|(s, e)| (e.iter().zip(&qe).map(|(a, b)| a * b).sum::<f32>(), s))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().take(top).map(|(_, s)| s.clone()).collect()
}

async fn api_images(State(app): S, Query(q): Query<Q>) -> Json<Value> {
    app.touch_ui();
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
                db.query_row(&format!("SELECT {COLS} FROM images WHERE sha1=?1"), [s], row_to_json).ok()
            })
            .collect();
        return Json(json!({"total": ranked.len(), "items": items}));
    }
    let (cond, args) = build_where(&q);
    let db = app.db.lock().unwrap();
    let params: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let total: i64 = db
        .query_row(&format!("SELECT COUNT(*) FROM images WHERE {cond}"), params.as_slice(), |r| r.get(0))
        .unwrap_or(0);
    // 並び順はホワイトリスト(SQL注入防止)。NULLは常に後ろへ
    let order = match q.sort.as_str() {
        "old" => "ingested ASC",
        "quality" => "quality IS NULL, quality DESC, ingested DESC",
        "big" => "bytes DESC",
        "cost" => "cost IS NULL, cost DESC, ingested DESC",
        _ => "ingested DESC",
    };
    let items: Vec<Value> = db
        .prepare(&format!(
            "SELECT {COLS} FROM images WHERE {cond} ORDER BY {order} LIMIT {} OFFSET {}",
            q.limit.clamp(1, 500),
            q.offset.max(0)
        ))
        .and_then(|mut st| {
            st.query_map(params.as_slice(), row_to_json)
                .map(|rows| rows.filter_map(Result::ok).collect())
        })
        .unwrap_or_default();
    Json(json!({"total": total, "items": items}))
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
                    let th = img.thumbnail(360, 360).into_rgb8();
                    let mut buf = std::io::Cursor::new(Vec::new());
                    if image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 82).encode_image(&th).is_ok() {
                        let _ = std::fs::write(store::thumb_path(&root, &sha), buf.get_ref());
                    }
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
#[derive(Deserialize)]
struct FaceEnrollIn {
    album: String,
    person: String,
    shas: Vec<String>,
    #[serde(default)] point: Option<[f32; 2]>, // 正規化座標(0-1)。指定=その点に一番近い顔を登録(2ショット対応)
}

#[derive(Deserialize)]
struct FaceDetectIn {
    sha1: String,
}

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

#[derive(Deserialize)]
struct FacesQ {
    #[serde(default)] album: String,
}

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

#[derive(Deserialize)]
struct FaceDelIn {
    album: String,
    person: String,
    #[serde(default)] sha1: Option<String>, // 指定=その参照顔1枚だけ削除、無指定=人物ごと削除
}

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

#[derive(Deserialize)]
struct FaceScanIn {
    album: String,
}

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
}

async fn api_album_make(State(app): S, Json(a): Json<AlbumIn>) -> impl IntoResponse {
    let slug = album_slug(&a.name);
    let rec = json!({"name": slug, "criteria": a.criteria, "folder": folder_norm(&a.folder), "goal": a.goal,
        "agent": if a.agent.is_object() { a.agent } else { json!({}) },
        "keywords": a.keywords, "engines": a.engines,
        "created": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()});
    let dir = album_dir(&app.root);
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
        a["running"] = json!(running && a["name"] == json!(running_album.clone()));
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
    *c = (Instant::now(), json!({"gpu": gpu, "ram": ram, "disk": disk}));
    c.1.clone()
}

/// AI稼働状況の一枚板 — どのAIが今なにをしてるかを1回で返す(UIサイドバー常設パネル用)
async fn api_activity(State(app): S) -> Json<Value> {
    let p = &app.ingest;
    let mut crawl = app.crawl.status();
    crawl["queue"] = json!(app.crawl_queue.lock().unwrap().iter().map(|c| album_slug(&c.album)).collect::<Vec<_>>());
    Json(json!({
        "crawl": crawl,
        "enrich": app.enrich.status(),
        "llm": app.llm.status(&app.root),
        "seg": app.seg.status(),
        "ingest": {
            "alive": p.alive.load(Relaxed), "done": p.done.load(Relaxed), "total": p.total.load(Relaxed),
            "label": app.ingest_label.lock().unwrap().clone(),
        },
        "workers": Value::Object(app.workers.lock().unwrap().clone()),
        "system": sys_stats(),
    }))
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

// プリセット(定番データの一括収蔵)
fn presets() -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
    vec![
        ("coco_val2017", "~/qwen-anime/data/val2017", "real", "🌍 COCO val2017 (実写5,000枚)"),
        ("coco_train2017", "~/qwen-anime/data/train2017", "real", "🌍 COCO train2017 (実写118,287枚·重い)"),
        ("places_indoor", "~/qwen-anime/data/places_indoor", "real", "🛋️ Places365 室内 (実写8,300枚)"),
        ("collected", "~/qwen-anime/data/collected", "real", "🌐 Web収集 (実写6,006枚)"),
        ("faces_synth", "~/qwen-anime/data/faces_synth", "synthetic", "🧑 合成顔 (生成3,000枚)"),
        ("scenes_synth", "~/qwen-anime/data/scenes_synth", "synthetic", "🏠 合成室内 (生成3,000枚)"),
        ("webcam_captured", "~/qwen-anime/data/webcam_captured", "real", "📷 部屋キャプチャ (実写)"),
    ]
}

async fn api_presets() -> Json<Value> {
    let out: Vec<Value> = presets()
        .iter()
        .map(|(id, path, origin, label)| {
            let d = PathBuf::from(shellexpand(path));
            json!({"id": id, "label": label, "origin": origin, "available": d.exists(),
                   "n": std::fs::read_dir(&d).map(|r| r.count()).unwrap_or(0)})
        })
        .collect();
    Json(json!(out))
}

async fn api_preset_ingest(State(app): S, AxPath(pid): AxPath<String>) -> impl IntoResponse {
    let Some((id, path, origin, _)) = presets().into_iter().find(|(id, ..)| *id == pid) else {
        return (StatusCode::NOT_FOUND, Json(json!({"detail": pid}))).into_response();
    };
    let d = PathBuf::from(shellexpand(path));
    if !d.exists() {
        return (StatusCode::NOT_FOUND, Json(json!({"detail": format!("{path} がまだありません")}))).into_response();
    }
    match spawn_ingest(app, d, format!("preset:{id}"), origin.into(), false) {
        Ok(()) => Json(json!({"ok": true, "job": "ingest"})).into_response(),
        Err(e) => (StatusCode::CONFLICT, Json(json!({"detail": e}))).into_response(),
    }
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
    if g.shas.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": "参考画像を選んでください"}))).into_response();
    }
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let root = app.root.clone();
    let shas = g.shas.clone();
    let refs = tokio::task::spawn_blocking(move || store::materialize(&root, &format!("_refs_{ts}"), &shas, ""))
        .await
        .unwrap();
    let r = app
        .http
        .post("http://127.0.0.1:8772/api/genvar")
        .json(&json!({"refs_path": refs["dir"], "instruction": g.instruction,
                      "per_ref": g.per_ref, "name": g.name}))
        .timeout(std::time::Duration::from_secs(90))
        .send()
        .await;
    match r {
        Ok(resp) => {
            let code = resp.status();
            let v: Value = resp.json().await.unwrap_or(json!({"detail": "応答壊れ"}));
            (StatusCode::from_u16(code.as_u16()).unwrap(), Json(v)).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"detail": format!("工房(:8772)に繋がりません: {e}")}))).into_response(),
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
    std::env::var("FG_CACHE_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(20 * 1024) // 既定20GB
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
        llm: Arc::new(llm::LlmState::default()),
        seg: Arc::new(seg::SegState::default()),
        http: reqwest::Client::new(),
        ui_hot: std::sync::atomic::AtomicU64::new(0),
        micro_inflight: Mutex::new(std::collections::HashSet::new()),
        workers: Mutex::new(serde_json::Map::new()),
    }));
    let router = Router::new()
        .route("/", get(index_page))
        .route("/api/images", get(api_images))
        .route("/api/facets", get(api_facets))
        .route("/api/meta/{sha1}", get(api_meta))
        .route("/img/{sha1}", get(img))
        .route("/thumb/{sha1}", get(thumb))
        .route("/preview/{sha1}", get(preview))
        .route("/render/{sha1}", get(render_img))
        .route("/api/edits/{sha1}", get(api_edits_get).put(api_edits_put))
        .route("/api/keep", post(api_keep))
        .route("/api/trash", post(api_trash).get(api_trash_list))
        .route("/api/trash/restore", post(api_trash_restore))
        .route("/api/source/trash", post(api_source_trash))
        .route("/api/move", post(api_move))
        .route("/api/faces", get(api_faces_list).delete(api_faces_delete))
        .route("/api/faces/enroll", post(api_faces_enroll))
        .route("/api/faces/detect", post(api_faces_detect))
        .route("/api/faces/scan", post(api_faces_scan))
        .route("/trash/img/{sha1}", get(trash_img))
        .route("/api/albums", post(api_album_make).get(api_albums))
        .route("/api/albums/{name}", delete(api_album_del))
        .route("/api/prune", post(api_prune))
        .route("/api/crawl", post(api_crawl))
        .route("/api/crawl/status", get(api_crawl_status))
        .route("/api/crawl/stop", post(api_crawl_stop))
        .route("/api/crawl/ledger/clear", post(api_ledger_clear))
        .route("/crawl/reject/{uk}", get(crawl_reject_thumb))
        .route("/api/activity", get(api_activity))
        .route("/api/nlq", post(api_nlq))
        .route("/api/llm/status", get(api_llm_status))
        .route("/api/llm/pull", post(api_llm_pull))
        .route("/api/llm/test", post(api_llm_test))
        .route("/api/upload", post(api_upload).layer(axum::extract::DefaultBodyLimit::max(2 << 30)))
        .route("/api/ingest", post(api_ingest))
        .route("/api/ingest/status", get(api_ingest_status))
        .route("/api/presets", get(api_presets))
        .route("/api/presets/{pid}", post(api_preset_ingest))
        .route("/api/datasets", post(api_dataset_make).get(api_datasets))
        .route("/api/datasets/{name}", delete(api_dataset_del))
        .route("/api/datasets/{name}/shas", get(api_dataset_shas))
        .route("/api/enrich", post(api_enrich))
        .route("/api/enrich/one", post(api_enrich_one))
        .route("/api/meta/patch", post(api_meta_patch))
        .route("/api/seg", post(api_seg))
        .route("/api/seg/one", post(api_seg_one))
        .route("/api/seg/refine", post(api_seg_refine))
        .route("/micro/{sha1}", get(micro))
        .route("/cutout/{sha1}", get(cutout))
        .route("/api/seg/stop", post(api_seg_stop))
        .route("/api/enrich/status", get(api_enrich_status))
        .route("/api/enrich/stop", post(api_enrich_stop))
        .route("/api/genvar", post(api_genvar))
        .route("/api/rebuild", post(api_rebuild))
        .route("/api/cache/stats", get(api_cache_stats))
        .route("/api/cache/clean", post(api_cache_clean))
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
            if app.crawl.alive.load(Relaxed) || app.enrich.alive.load(Relaxed) || app.seg.alive.load(Relaxed) {
                continue;
            }
            let missing: i64 = {
                let db = app.db.lock().unwrap();
                db.query_row("SELECT COUNT(*) FROM images WHERE vlm_model IS NULL", [], |r| r.get(0)).unwrap_or(0)
            };
            if missing > 0 {
                println!("🤖 自動エンリッチ開始(未取得{missing})");
                app.set_worker("groom", true, format!("属性の穴埋め依頼(残{missing})"));
                let _ = app.http.post("http://localhost:8790/api/enrich")
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
                    let _ = app.http.post("http://localhost:8790/api/seg")
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
            tokio::time::sleep(std::time::Duration::from_secs(1800)).await;
            if app.crawl.alive.load(Relaxed) {
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
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await.unwrap();
    println!("🖼 fluent_gallery (rust) on :{port}");
    axum::serve(listener, router).await.unwrap();
}

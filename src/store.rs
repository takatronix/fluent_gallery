//! ストア — 収蔵(ingest)・pHash・サムネ焼き・索引・払い出し。
//! 正本はサイドカーJSON。SQLiteは「WHERE句に使う値だけ」の使い捨て索引。

use rayon::prelude::*;
use rusqlite::Connection;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};

pub const IMG_EXTS: [&str; 5] = ["jpg", "jpeg", "png", "webp", "bmp"];

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS images(
  sha1 TEXT PRIMARY KEY, ext TEXT, w INT, h INT, bytes INT, phash TEXT,
  source TEXT, origin TEXT, ingested REAL, tint TEXT,
  vlm_model TEXT, caption TEXT, quality INT, nsfw INT,
  scene TEXT, subject TEXT, lighting TEXT, style TEXT);
CREATE TABLE IF NOT EXISTS tags(sha1 TEXT, tag TEXT, PRIMARY KEY(sha1, tag));
CREATE INDEX IF NOT EXISTS idx_tags_tag ON tags(tag);
CREATE INDEX IF NOT EXISTS idx_images_source ON images(source);
-- 13万件の無限scrollをOFFSET後半でも全表sortさせない。sha1は同時刻の決定的tie-breaker。
CREATE INDEX IF NOT EXISTS idx_images_ingested_sha1 ON images(ingested DESC, sha1 DESC);
CREATE TABLE IF NOT EXISTS faces(
  person TEXT, album TEXT, sha1 TEXT, emb BLOB, bbox TEXT,
  PRIMARY KEY(person, sha1)
); -- 顔ID参照(docs/face-id-design.md 2026-09-03)
CREATE INDEX IF NOT EXISTS idx_images_origin ON images(origin);
CREATE VIRTUAL TABLE IF NOT EXISTS captions USING fts5(sha1 UNINDEXED, caption);
PRAGMA journal_mode=WAL;
";

/// スキーマ適用+移行(後付け列はALTERを空振り覚悟で)
pub fn ensure_schema(db: &Connection) {
    let _ = db.execute_batch(SCHEMA);
    let _ = db.execute("ALTER TABLE images ADD COLUMN keep INT", []);
    let _ = db.execute("ALTER TABLE images ADD COLUMN cost REAL", []); // 獲得コストUSD(生成/VLM/クロールの実費。金かけて集めた事を見える化)
    // 画像内の顔(位置+ArcFace埋め込み)の永続キャッシュ。開くたび再計算で遅い問題の根治(2026-09-03)。
    // 台帳(faces)と独立な生データなので、人物の登録/削除では無効化不要。idx=-1空行=顔なしの印
    let _ = db.execute(
        "CREATE TABLE IF NOT EXISTS img_faces(sha1 TEXT, idx INT, bbox TEXT, emb BLOB, PRIMARY KEY(sha1, idx))",
        [],
    );
    // CLIP埋め込み512×f32LE(似た画像)。空BLOB=読めない画像の印。
    // imagesに焼くと1行2KB×12万で全表スキャン(一覧/facet/COUNT)が30倍遅くなった実害(2026-09-03)→別テーブル分離
    let _ = db.execute("CREATE TABLE IF NOT EXISTS embs(sha1 TEXT PRIMARY KEY, emb BLOB)", []);
    // 旧: images.emb列からの一回きり移行(列が残っていれば吸い上げて落とす)
    let has_emb_col = db
        .prepare("SELECT emb FROM images LIMIT 0")
        .is_ok();
    if has_emb_col {
        let n = db
            .execute("INSERT OR IGNORE INTO embs(sha1, emb) SELECT sha1, emb FROM images WHERE emb IS NOT NULL", [])
            .unwrap_or(0);
        let dropped = db.execute("ALTER TABLE images DROP COLUMN emb", []).is_ok();
        if dropped {
            let _ = db.execute_batch("VACUUM"); // 太った行の残骸ページを返して全表スキャンを元の速さに戻す(一回きり)
        }
        println!("🧭 CLIP埋め込みを別テーブルへ移行: {n}件 (emb列削除={dropped})");
    }
    let _ = db.execute("ALTER TABLE images ADD COLUMN rights TEXT", []); // 権利状態(cc-by等/unknown)。クリーンだけでデータセットを組めるように
    let _ = db.execute("ALTER TABLE images ADD COLUMN gender TEXT", []); // 人物の性別(male/female/mixed/none)。人物データセットの選別用
    let _ = db.execute("ALTER TABLE images ADD COLUMN people_count TEXT", []); // 0/1/2/group
    let _ = db.execute("ALTER TABLE images ADD COLUMN age_group TEXT", []); // child/teen/adult/senior/none(安全フィルタ兼用)
    let _ = db.execute("ALTER TABLE images ADD COLUMN framing TEXT", []); // closeup/upper_body/full_body/wide(LoRA選別)
    let _ = db.execute("ALTER TABLE images ADD COLUMN watermark INT", []); // 透かし/ロゴ/焼き込み文字(クロールゴミの主犯)
    let _ = db.execute("ALTER TABLE images ADD COLUMN animal TEXT", []); // 動物種(dog/cat/bird…/none)。品種はtagsへ(shiba inu等)
    let _ = db.execute("ALTER TABLE images ADD COLUMN seg INT", []); // マスク有無(gdino2seg、shapes>0)
    let _ = db.execute("ALTER TABLE images ADD COLUMN erev TEXT", []); // 編集rev(サムネURLのキャッシュバスタ。編集無し=NULL)
}

pub fn shard(sha1: &str) -> &str {
    &sha1[..2]
}

pub fn meta_path(root: &Path, sha1: &str) -> PathBuf {
    root.join("store/meta").join(shard(sha1)).join(format!("{sha1}.json"))
}

pub fn image_path(root: &Path, sha1: &str, ext: &str) -> PathBuf {
    root.join("store/images").join(shard(sha1)).join(format!("{sha1}.{ext}"))
}

pub fn thumb_path(root: &Path, sha1: &str) -> PathBuf {
    root.join("store/thumbs").join(shard(sha1)).join(format!("{sha1}.jpg"))
}

/// preview段(長辺1080)。thumbsと同居(拡張子で区別)— キャッシュ掃除が一括で効く。
pub fn preview_path(root: &Path, sha1: &str) -> PathBuf {
    root.join("store/thumbs").join(shard(sha1)).join(format!("{sha1}.p.jpg"))
}

/// micro段(120px) — 俯瞰グリッド用。janitorは*.p.jpgのみ対象なのでmicroは掃除されない。
pub fn micro_path(root: &Path, sha1: &str) -> PathBuf {
    root.join("store/thumbs").join(shard(sha1)).join(format!("{sha1}.m.jpg"))
}

/// decode済み画像から360サムネと120microを同時に焼く(decode1回で両tier)。
/// microは360中間から縮める — 原寸から2回resizeするより軽い。
pub fn write_thumbs(root: &Path, sha1: &str, img: &image::DynamicImage) {
    let tp = thumb_path(root, sha1);
    if let Some(dir) = tp.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let th = img.thumbnail(360, 360);
    let mut buf = std::io::Cursor::new(Vec::new());
    if image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 82)
        .encode_image(&th.to_rgb8())
        .is_ok()
    {
        let _ = std::fs::write(&tp, buf.get_ref());
    }
    let mi = th.thumbnail(120, 120).into_rgb8();
    let mut mb = std::io::Cursor::new(Vec::new());
    if image::codecs::jpeg::JpegEncoder::new_with_quality(&mut mb, 72).encode_image(&mi).is_ok() {
        let _ = std::fs::write(micro_path(root, sha1), mb.get_ref());
    }
}

/// micro未生成の360サムネからbatch件だけ焼く。返り値=生成数(0=完了)。
/// 360jpg→120は1件2ms級なので、呼び側が小batch+休止で平準化する。
pub fn micro_backfill(root: &Path, batch: usize) -> usize {
    let mut done = 0usize;
    for p in walk_all(&root.join("store/thumbs")) {
        if done >= batch {
            break;
        }
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        // 360サムネ(<sha1>.jpg)のみ対象。preview(.p.jpg)/micro(.m.jpg)自身は飛ばす
        let Some(sha1) = name.strip_suffix(".jpg") else { continue };
        if sha1.len() != 40 || sha1.contains('.') {
            continue;
        }
        let mp = micro_path(root, sha1);
        if mp.exists() {
            continue;
        }
        let Ok(im) = image::open(&p) else {
            // 壊れた360は原本から焼き直しを試みる(それも駄目なら諦めて次へ)
            if let Some(m) = load_meta(root, sha1) {
                if let Ok(orig) = image::open(image_path(root, sha1, m["ext"].as_str().unwrap_or("png"))) {
                    write_thumbs(root, sha1, &orig);
                    done += 1;
                }
            }
            continue;
        };
        let mi = im.thumbnail(120, 120).into_rgb8();
        let mut buf = std::io::Cursor::new(Vec::new());
        if image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 72).encode_image(&mi).is_ok() {
            let _ = std::fs::write(&mp, buf.get_ref());
            done += 1;
        }
    }
    done
}

pub fn load_meta(root: &Path, sha1: &str) -> Option<Value> {
    if sha1.len() < 3 || !sha1.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    serde_json::from_str(&std::fs::read_to_string(meta_path(root, sha1)).ok()?).ok()
}

pub fn save_meta(root: &Path, m: &Value) -> std::io::Result<()> {
    let sha1 = m["sha1"].as_str().unwrap();
    // 削除競合ガード: 原本が消えた個体のサイドカーは書かない。裏方(enrich/seg)が削除直後に
    // 書き戻して「消したのに灰色タイルで復活」する事故の根治(2026-09-03、IVEで8件実害)
    let ext = m["ext"].as_str().unwrap_or("jpg");
    if !image_path(root, sha1, ext).exists() {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "original missing (deleted?)"));
    }
    let p = meta_path(root, sha1);
    std::fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string(m)?)?;
    std::fs::rename(tmp, p)
}

/// DCTベースpHash(64bit)。Python版(numpy)と同じ手順: 32x32グレー→DCT→左上8x8をメディアン閾値。
pub fn phash64(img: &image::DynamicImage) -> String {
    let g = img.grayscale().resize_exact(32, 32, image::imageops::FilterType::Lanczos3);
    let g = g.to_luma8();
    let n = 32usize;
    // DCT-II 基底行列 d[k][x]
    let mut d = vec![[0f32; 32]; 32];
    for k in 0..n {
        for x in 0..n {
            d[k][x] = (2.0 / n as f32).sqrt()
                * (std::f32::consts::PI * (2 * x + 1) as f32 * k as f32 / (2 * n) as f32).cos();
        }
    }
    for x in 0..n {
        d[0][x] /= 2f32.sqrt();
    }
    // dct = D * g * D^T の左上8x8だけ計算
    let px = |x: usize, y: usize| g.get_pixel(x as u32, y as u32).0[0] as f32;
    let mut low = [0f32; 64];
    for u in 0..8 {
        for v in 0..8 {
            let mut s = 0f32;
            for y in 0..n {
                let mut row = 0f32;
                for x in 0..n {
                    row += d[v][x] * px(x, y);
                }
                s += d[u][y] * row;
            }
            low[u * 8 + v] = s;
        }
    }
    let mut sorted: Vec<f32> = low[1..].to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = sorted[sorted.len() / 2];
    let mut bits: u64 = 0;
    for (i, v) in low.iter().enumerate() {
        if *v > median {
            bits |= 1 << (63 - i);
        }
    }
    format!("{bits:016x}")
}

/// 平均色(即時表示プレースホルダ用の魔法の種)
fn tint(img: &image::DynamicImage) -> String {
    let t = img.thumbnail(8, 8).to_rgb8();
    let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
    for p in t.pixels() {
        r += p.0[0] as u32;
        g += p.0[1] as u32;
        b += p.0[2] as u32;
        n += 1;
    }
    if n == 0 {
        return "#222".into();
    }
    format!("#{:02x}{:02x}{:02x}", r / n, g / n, b / n)
}

pub fn index_meta(db: &Connection, m: &Value) {
    let v = &m["vlm"];
    let a = &v["attrs"];
    let s = |x: &Value| x.as_str().map(str::to_string);
    let _ = db.execute(
        "INSERT OR REPLACE INTO images(sha1,ext,w,h,bytes,phash,source,origin,ingested,tint,
         vlm_model,caption,quality,nsfw,scene,subject,lighting,style,keep,cost,rights,gender,
         people_count,age_group,framing,watermark,animal,seg,erev)
         VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        rusqlite::params![
            m["sha1"].as_str(), m["ext"].as_str(), m["w"].as_i64(), m["h"].as_i64(),
            m["bytes"].as_i64(), s(&m["phash"]), s(&m["source"]), s(&m["origin"]),
            m["ingested"].as_f64(), s(&m["tint"]),
            s(&v["model"]), s(&v["caption"]),
            a["quality"].as_i64(),
            if v.is_object() { Some(a["nsfw"].as_bool().unwrap_or(false) as i64) } else { None },
            s(&a["scene"]), s(&a["subject"]), s(&a["lighting"]), s(&a["style"]),
            m["keep"].as_bool().unwrap_or(false) as i64,
            m["cost"]["usd"].as_f64(), // サイドカー正本: {"cost":{"usd":0.012,"by":"gen:gpt-image-1"}}
            s(&m["rights"]),
            s(&a["gender"]),
            a["people_count"].as_str().map(str::to_string).or_else(|| a["people_count"].as_i64().map(|n| n.to_string())),
            s(&a["age_group"]),
            s(&a["framing"]),
            if v.is_object() { a["watermark"].as_bool().map(|b| b as i64) } else { None },
            s(&a["animal"]),
            m["seg"]["shapes"].as_array().map(|a| (!a.is_empty()) as i64),
            m["edits"].as_array().filter(|a| !a.is_empty()).map(|_| crate::edits::rev(&m["edits"])),
        ],
    );
    let sha1 = m["sha1"].as_str().unwrap();
    let _ = db.execute("DELETE FROM tags WHERE sha1=?", [sha1]);
    if let Some(tags) = v["tags"].as_array() {
        for t in tags {
            if let Some(t) = t.as_str() {
                let t: String = t.chars().take(48).collect::<String>().to_lowercase();
                let _ = db.execute("INSERT OR IGNORE INTO tags VALUES(?,?)", [sha1, &t]);
            }
        }
    }
    // クロール由来の永続タグ(フォルダ名/クエリ固有名詞)。enrichがvlmを書き換えても残る(2026-09-03)
    if let Some(tags) = m["crawl"]["tags"].as_array() {
        for t in tags {
            if let Some(t) = t.as_str() {
                let t: String = t.chars().take(48).collect::<String>().to_lowercase();
                let _ = db.execute("INSERT OR IGNORE INTO tags VALUES(?,?)", [sha1, &t]);
            }
        }
    }
    let _ = db.execute("DELETE FROM captions WHERE sha1=?", [sha1]);
    if let Some(c) = v["caption"].as_str() {
        let _ = db.execute("INSERT INTO captions VALUES(?,?)", [sha1, c]);
    }
}

/// 顔ID: フォルダの登録メンバー(person→参照埋め込み群)。album=''のグローバル登録も合流
pub fn face_refs(db: &Connection, album: &str) -> Vec<(String, Vec<Vec<f32>>)> {
    let mut map: std::collections::HashMap<String, Vec<Vec<f32>>> = Default::default();
    if let Ok(mut st) = db.prepare("SELECT person, emb FROM faces WHERE album=?1 OR album=''") {
        if let Ok(rows) = st.query_map([album], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))) {
            for (p, b) in rows.flatten() {
                let e: Vec<f32> = b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
                map.entry(p).or_default().push(e);
            }
        }
    }
    map.into_iter().collect()
}

pub fn infer_origin(source: &str) -> &'static str {
    let s = source.to_lowercase();
    if ["gen", "var", "synth", "t2i", "ai_"].iter().any(|k| s.contains(k)) {
        "synthetic"
    } else {
        "real"
    }
}

/// 進捗(UIを待たせない・ロックしないための心臓部)。ジョブはこれを共有して随時更新する。
#[derive(Default)]
pub struct Progress {
    pub total: std::sync::atomic::AtomicUsize,
    pub done: std::sync::atomic::AtomicUsize,
    pub added: std::sync::atomic::AtomicUsize,
    pub dup: std::sync::atomic::AtomicUsize,
    pub bad: std::sync::atomic::AtomicUsize,
    pub alive: std::sync::atomic::AtomicBool,
    /// 中断要求(長い取り込みを人が止められるように。ジョブ側が1件ごとに見る)
    pub stop: std::sync::atomic::AtomicBool,
}

#[allow(dead_code)] // 呼び手はProgress経由で読む。戻り値はテスト/将来のログ用
pub struct IngestStats {
    pub added: usize,
    pub dup: usize,
    pub bad: usize,
    pub scanned: usize,
}

/// 収蔵。並列でsha1/pHash/サムネ焼き→索引書き込み。進捗はProgressにリアルタイム反映。
/// 同FSはハードリンク(容量ゼロ)、別FSはコピー。サムネはこの場で焼く(表示時のもたつきゼロ)。
pub fn ingest(
    root: &Path,
    db: &Connection,
    path: &Path,
    source: &str,
    origin: &str,
    mv: bool,
    prog: &Progress,
) -> IngestStats {
    use std::sync::atomic::Ordering::Relaxed;
    let files: Vec<PathBuf> = if path.is_file() {
        vec![path.to_path_buf()]
    } else {
        let mut v: Vec<PathBuf> = walk(path);
        v.sort();
        v
    };
    let scanned = files.len();
    prog.total.store(scanned, Relaxed);
    let origin = if origin.is_empty() { infer_origin(source) } else { origin };
    // 索引書き込みは1000枚ごとのチャンクで逐次コミット=途中経過が検索にすぐ現れる
    for chunk in files.chunks(1000) {
        let results: Vec<Option<Value>> = chunk
            .par_iter()
            .map(|f| {
                let r = process_one(root, f, source, origin, mv);
                prog.done.fetch_add(1, Relaxed);
                r
            })
            .collect();
        for r in &results {
            match r {
                Some(m) if m["dup"].as_bool() == Some(true) => {
                    prog.dup.fetch_add(1, Relaxed);
                }
                Some(m) => {
                    index_meta(db, m);
                    prog.added.fetch_add(1, Relaxed);
                }
                None => {
                    prog.bad.fetch_add(1, Relaxed);
                }
            }
        }
    }
    IngestStats {
        added: prog.added.load(Relaxed),
        dup: prog.dup.load(Relaxed),
        bad: prog.bad.load(Relaxed),
        scanned,
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else if p
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| IMG_EXTS.contains(&e.to_lowercase().as_str()))
                .unwrap_or(false)
            {
                out.push(p);
            }
        }
    }
    out
}

fn process_one(root: &Path, f: &Path, source: &str, origin: &str, mv: bool) -> Option<Value> {
    let data = std::fs::read(f).ok()?;
    let sha1 = hex::encode(Sha1::digest(&data));
    if meta_path(root, &sha1).exists() {
        return Some(json!({"dup": true}));
    }
    let img = image::load_from_memory(&data).ok()?;
    let (w, h) = (img.width(), img.height());
    let ph = phash64(&img);
    let tint = tint(&img);
    let ext = f
        .extension()?
        .to_str()?
        .to_lowercase()
        .replace("jpeg", "jpg");
    let dst = image_path(root, &sha1, &ext);
    std::fs::create_dir_all(dst.parent()?).ok()?;
    if !dst.exists() {
        if std::fs::hard_link(f, &dst).is_err() {
            std::fs::write(&dst, &data).ok()?;
        }
    }
    if mv {
        let _ = std::fs::remove_file(f);
    }
    // grid360サムネ+120microだけこの場で焼く(合計30KB級で軽い・グリッドの速さの生命線)。
    // preview1080は初回表示時に生成+LRU間引き — 全数焼くと100万枚で百GB級に膨らむため(ディスク設計)。
    write_thumbs(root, &sha1, &img);
    let m = json!({
        "sha1": sha1, "ext": ext, "w": w, "h": h, "bytes": data.len(),
        "phash": ph, "tint": tint, "source": source, "origin": origin,
        "ingested": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs_f64(),
    });
    save_meta(root, &m).ok()?;
    Some(m)
}

/// 検査済みバイト列の収蔵(クローラ等の入り口)。extraはサイドカーに合流(rights/crawl来歴/cost等)。
/// 返り値: Ok(sha1) / Err("dup") / Err("bad")
pub fn ingest_bytes(
    root: &Path,
    db: &Connection,
    data: &[u8],
    ext: &str,
    source: &str,
    extra: &Value,
) -> Result<String, &'static str> {
    let sha1 = hex::encode(Sha1::digest(data));
    if meta_path(root, &sha1).exists() {
        return Err("dup");
    }
    let img = image::load_from_memory(data).map_err(|_| "bad")?;
    let (w, h) = (img.width(), img.height());
    let ph = phash64(&img);
    let tint = tint(&img);
    let ext = if IMG_EXTS.contains(&ext) { ext } else { "jpg" };
    let dst = image_path(root, &sha1, ext);
    std::fs::create_dir_all(dst.parent().unwrap()).map_err(|_| "bad")?;
    std::fs::write(&dst, data).map_err(|_| "bad")?;
    write_thumbs(root, &sha1, &img);
    let mut m = json!({
        "sha1": sha1, "ext": ext, "w": w, "h": h, "bytes": data.len(),
        "phash": ph, "tint": tint, "source": source, "origin": infer_origin(source),
        "ingested": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64(),
    });
    if let (Some(base), Some(ex)) = (m.as_object_mut(), extra.as_object()) {
        for (k, v) in ex {
            base.insert(k.clone(), v.clone());
        }
    }
    save_meta(root, &m).map_err(|_| "bad")?;
    index_meta(db, &m);
    Ok(sha1)
}

/// サイドカー正本からSQLiteを作り直す(壊れても怖くない)。
pub fn rebuild(root: &Path, db: &Connection) -> usize {
    let _ = db.execute_batch(
        "DELETE FROM images; DELETE FROM tags; DELETE FROM captions;",
    );
    let mut n = 0;
    for p in walk_json(&root.join("store/meta")) {
        if let Ok(t) = std::fs::read_to_string(&p) {
            if let Ok(m) = serde_json::from_str::<Value>(&t) {
                index_meta(db, &m);
                n += 1;
            }
        }
    }
    n
}

fn walk_json(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk_json(&p));
            } else if p.extension().and_then(|e| e.to_str()) == Some("json") {
                out.push(p);
            }
        }
    }
    out
}

/// 原本の世代整理(prune): 機械が集めた/作った古いデータを .trash へ退避する。
/// 安全弁: ①dry_run既定 ②データセット参照中は絶対保護 ③即消しせずゴミ箱経由(30日後にジャニターが空にする)。
/// 将来の常駐キュレーターはこれをMCP経由で叩く(AI 1st)。
pub fn prune(
    root: &Path,
    db: &Connection,
    source_prefix: &str,
    older_days: f64,
    keep_quality: i64,
    extra_protected: &std::collections::HashSet<String>,
    dry_run: bool,
) -> Value {
    use std::collections::HashSet;
    // アルバム/データセットが参照しているshaは保護(symlink/焼き込みどちらも file stem = sha1)。
    // extra_protected=動的アルバムの現メンバー(呼び出し側で条件を評価して渡す)
    let mut protected: HashSet<String> = extra_protected.clone();
    if let Ok(rd) = std::fs::read_dir(root.join("store/datasets")) {
        for d in rd.flatten() {
            if let Ok(fs) = std::fs::read_dir(d.path()) {
                for f in fs.flatten() {
                    if let Some(stem) = f.path().file_stem().map(|s| s.to_string_lossy().into_owned()) {
                        if stem.len() == 40 {
                            protected.insert(stem);
                        }
                    }
                }
            }
        }
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
    let cutoff = now - older_days * 86400.0;
    // 残す条件: ⭐keep / VLM品質が閾値以上(「これは！」は機械判定でも残る)。未評価(quality NULL)は候補。
    let rows: Vec<(String, String, u64)> = db
        .prepare(
            "SELECT sha1, ext, COALESCE(bytes,0) FROM images
             WHERE source LIKE ? AND ingested < ?
               AND (keep IS NULL OR keep=0)
               AND (quality IS NULL OR quality < ?)",
        )
        .and_then(|mut st| {
            st.query_map(rusqlite::params![format!("{source_prefix}%"), cutoff, keep_quality], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? as u64))
            })
            .map(|rs| rs.filter_map(Result::ok).collect())
        })
        .unwrap_or_default();
    let day = (now / 86400.0) as u64;
    let trash = root.join("store/.trash").join(format!("d{day}"));
    let (mut moved, mut skipped, mut bytes) = (0usize, 0usize, 0u64);
    for (sha1, ext, n) in &rows {
        if protected.contains(sha1) {
            skipped += 1;
            continue;
        }
        bytes += n;
        if dry_run {
            moved += 1;
            continue;
        }
        let _ = std::fs::create_dir_all(&trash);
        let _ = std::fs::rename(image_path(root, sha1, ext), trash.join(format!("{sha1}.{ext}")));
        let _ = std::fs::rename(meta_path(root, sha1), trash.join(format!("{sha1}.json")));
        for p in [thumb_path(root, sha1), preview_path(root, sha1), micro_path(root, sha1)] {
            let _ = std::fs::remove_file(p);
        }
        crate::edits::clear_renders(root, sha1);
        let _ = db.execute("DELETE FROM images WHERE sha1=?", [sha1.as_str()]);
        let _ = db.execute("DELETE FROM embs WHERE sha1=?", [sha1.as_str()]);
        let _ = db.execute("DELETE FROM img_faces WHERE sha1=?", [sha1.as_str()]);
        let _ = db.execute("DELETE FROM tags WHERE sha1=?", [sha1.as_str()]);
        let _ = db.execute("DELETE FROM captions WHERE sha1=?", [sha1.as_str()]);
        moved += 1;
    }
    json!({"dry_run": dry_run, "source": source_prefix, "older_days": older_days,
           "candidates": rows.len(), "protected_skipped": skipped,
           "moved": moved, "mb": bytes >> 20,
           "note": if dry_run { "dry_run=false で .trash へ退避(30日後に自動で空になります)" }
                   else { ".trash へ退避しました。戻すには d*/ から images/ へ戻して /api/rebuild" }})
}

/// 「捨てた」は意思表示: 削除画像のpHashを永久拒否リストに刻み、クローラが同じ/似た絵を二度と拾わないようにする
pub fn never_again_add(root: &Path, entries: &[(String, String)]) {
    if entries.is_empty() {
        return;
    }
    let p = root.join("store/crawl/never_again.json");
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let mut v: Vec<Value> = std::fs::read_to_string(&p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    for (sha1, ph) in entries {
        v.push(json!({"sha1": sha1, "phash": ph}));
    }
    let _ = std::fs::write(&p, serde_json::to_string(&v).unwrap());
}

/// undo(復元)されたら拒否リストから外す(「やっぱり要る」も意思表示)
pub fn never_again_remove(root: &Path, shas: &[String]) {
    let p = root.join("store/crawl/never_again.json");
    let Some(mut v) = std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str::<Vec<Value>>(&t).ok())
    else { return };
    let before = v.len();
    v.retain(|e| e["sha1"].as_str().map(|s| !shas.iter().any(|x| x == s)).unwrap_or(true));
    if v.len() != before {
        let _ = std::fs::write(&p, serde_json::to_string(&v).unwrap());
    }
}

pub fn never_again_phashes(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join("store/crawl/never_again.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<Value>>(&t).ok())
        .map(|v| v.iter().filter_map(|e| e["phash"].as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// 手動/AI削除: .trash/d<day>/ へ退避(一発で消せるのはundoがあるから)。返り値=退避できた数
pub fn trash_shas(root: &Path, db: &Connection, shas: &[String]) -> usize {
    let day = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86400)
        .unwrap_or(0);
    let trash = root.join("store/.trash").join(format!("d{day}"));
    let _ = std::fs::create_dir_all(&trash);
    let mut n = 0;
    let mut never: Vec<(String, String)> = vec![];
    for sha1 in shas {
        // 冪等に消し切る: サイドカーが無い「幽霊」(索引だけ残った個体)でもDB行とファイルを必ず始末する
        let mut removed = false;
        if let Some(m) = load_meta(root, sha1) {
            if let Some(ph) = m["phash"].as_str() {
                never.push((sha1.clone(), ph.to_string()));
            }
        } else if let Ok(Some(ph)) = db.query_row("SELECT phash FROM images WHERE sha1=?", [sha1.as_str()], |r| {
            r.get::<_, Option<String>>(0)
        }) {
            never.push((sha1.clone(), ph)); // 幽霊でも拒否リストには載せる
        }
        for ext in IMG_EXTS {
            let src = image_path(root, sha1, ext);
            if src.exists() && std::fs::rename(&src, trash.join(format!("{sha1}.{ext}"))).is_ok() {
                removed = true;
            }
        }
        if meta_path(root, sha1).exists()
            && std::fs::rename(meta_path(root, sha1), trash.join(format!("{sha1}.json"))).is_ok()
        {
            removed = true;
        }
        for p in [thumb_path(root, sha1), preview_path(root, sha1), micro_path(root, sha1)] {
            let _ = std::fs::remove_file(p);
        }
        crate::edits::clear_renders(root, sha1);
        let rows = db.execute("DELETE FROM images WHERE sha1=?", [sha1.as_str()]).unwrap_or(0);
        let _ = db.execute("DELETE FROM embs WHERE sha1=?", [sha1.as_str()]);
        let _ = db.execute("DELETE FROM img_faces WHERE sha1=?", [sha1.as_str()]);
        let _ = db.execute("DELETE FROM tags WHERE sha1=?", [sha1.as_str()]);
        let _ = db.execute("DELETE FROM captions WHERE sha1=?", [sha1.as_str()]);
        if removed || rows > 0 {
            n += 1;
        }
    }
    never_again_add(root, &never); // 二度と拾わない(クローラの再収蔵防止)
    n
}

/// undo: .trash から原本+サイドカーを戻して再索引。日付フォルダを横断して探す
pub fn restore_shas(root: &Path, db: &Connection, shas: &[String]) -> usize {
    never_again_remove(root, shas);
    let mut n = 0;
    let trash_root = root.join("store/.trash");
    let dirs: Vec<PathBuf> = std::fs::read_dir(&trash_root)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default();
    for sha1 in shas {
        for d in &dirs {
            let mj = d.join(format!("{sha1}.json"));
            if !mj.exists() {
                continue;
            }
            let Ok(t) = std::fs::read_to_string(&mj) else { break };
            let Ok(m) = serde_json::from_str::<Value>(&t) else { break };
            let ext = m["ext"].as_str().unwrap_or("png");
            let dst = image_path(root, sha1, ext);
            let _ = std::fs::create_dir_all(dst.parent().unwrap());
            if std::fs::rename(d.join(format!("{sha1}.{ext}")), &dst).is_ok() {
                let _ = std::fs::create_dir_all(meta_path(root, sha1).parent().unwrap());
                let _ = std::fs::rename(&mj, meta_path(root, sha1));
                index_meta(db, &m);
                n += 1;
            }
            break;
        }
    }
    n
}

/// .trash の日付フォルダ(d<unix_day>)を30日で空にする(ジャニターから呼ばれる)
pub fn empty_old_trash(root: &Path) -> u64 {
    let now_day = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86400)
        .unwrap_or(0);
    let mut freed = 0u64;
    if let Ok(rd) = std::fs::read_dir(root.join("store/.trash")) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(day) = name.strip_prefix('d').and_then(|s| s.parse::<u64>().ok()) {
                if now_day.saturating_sub(day) > 30 {
                    freed += walk_all(&e.path()).iter().filter_map(|p| p.metadata().ok()).map(|m| m.len()).sum::<u64>();
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
    }
    freed
}

/// ディスク設計の芯: 原本以外(preview/レンダ)は全て「上限つき再生成可能キャッシュ」。
/// preview(*.p.jpg)とrendersを合算し、キャップ超過分を古い方(mtime)から捨てる。
/// grid360サムネ(*.jpg)は小さくグリッド速度の生命線なので対象外。返り値=(解放バイト, 削除数)。
pub fn cache_janitor(root: &Path, cap_mb: u64) -> (u64, usize) {
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = vec![];
    let mut scan = |dir: &Path, previews_only: bool| {
        for p in walk_all(dir) {
            if previews_only && !p.file_name().map(|n| n.to_string_lossy().ends_with(".p.jpg")).unwrap_or(false) {
                continue;
            }
            if let Ok(md) = p.metadata() {
                files.push((md.modified().unwrap_or(std::time::UNIX_EPOCH), md.len(), p));
            }
        }
    };
    scan(&root.join("store/renders"), false);
    scan(&root.join("store/thumbs"), true);
    let total: u64 = files.iter().map(|(_, n, _)| n).sum();
    let cap = cap_mb.saturating_mul(1 << 20);
    if total <= cap {
        return (0, 0);
    }
    files.sort_by_key(|(t, ..)| *t); // 古い順
    let target = (total - cap) + cap / 10; // 1割余分に空けて毎回発火しないように
    let (mut freed, mut deleted) = (0u64, 0usize);
    for (_, n, p) in files {
        if freed >= target {
            break;
        }
        if std::fs::remove_file(&p).is_ok() {
            freed += n;
            deleted += 1;
        }
    }
    (freed, deleted)
}

fn walk_all(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk_all(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// 払い出し: symlinkディレクトリ+manifest。folderはUI上の棚分け(ネスト可、実体はフラット)
pub fn materialize(root: &Path, name: &str, shas: &[String], folder: &str) -> Value {
    let slug: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .take(48)
        .collect();
    let slug = if slug.is_empty() { "dataset".into() } else { slug };
    let out = root.join("store/datasets").join(&slug);
    let _ = std::fs::create_dir_all(&out);
    let mut linked = 0;
    for sha1 in shas {
        if let Some(m) = load_meta(root, sha1) {
            let ext = m["ext"].as_str().unwrap_or("png");
            let src = image_path(root, sha1, ext);
            // 非破壊調整あり → ここで焼き込み(原本は不変のまま、渡す物は見た目通り)
            let ed = m.get("edits").cloned().unwrap_or_else(|| json!([]));
            if ed.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
                let link = out.join(format!("{sha1}.jpg"));
                if !link.exists() {
                    if let Some(b) = crate::edits::render(root, sha1, ext, &ed, 0, None) {
                        if std::fs::write(&link, b).is_ok() {
                            linked += 1;
                        }
                    }
                }
                continue;
            }
            let link = out.join(format!("{sha1}.{ext}"));
            if !link.exists() && src.exists() {
                #[cfg(unix)]
                if std::os::unix::fs::symlink(src.canonicalize().unwrap_or(src), &link).is_ok() {
                    linked += 1;
                }
            }
        }
    }
    let count = std::fs::read_dir(&out)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) != Some("json"))
                .count()
        })
        .unwrap_or(0);
    let manifest = json!({"name": slug, "folder": folder, "created": std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64(),
        "count": count, "criteria": "api"});
    let _ = std::fs::write(out.join("manifest.json"), serde_json::to_string_pretty(&manifest).unwrap());
    json!({"name": slug, "dir": out.to_string_lossy(), "count": count, "linked": linked})
}

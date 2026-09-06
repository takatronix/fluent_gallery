//! フォルダの言語フィルタを一度だけ原寸画像へ焼く。閲覧時は通常の画像と同じ経路。
//! 原本と既存の編集履歴は不変。集合のメンバーと再利用キャッシュはファイルにも保存する。

use crate::{edits, store};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Default)]
pub struct FolderFilterState {
    job: Mutex<Value>,
    stop_requested: AtomicBool,
}

impl FolderFilterState {
    pub fn status(&self) -> Value {
        let mut job = self.job.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if !job.is_object() {
            job = json!({"running": false, "alive": false, "done": 0, "total": 0,
                "errors": 0, "cached": 0, "last": "", "set_id": "", "completed": false});
        }
        job["stop_requested"] = json!(self.stop_requested.load(Relaxed));
        job
    }

    /// 中断は現在の1枚を保存した後。共有DBや描画スレッドは待たせない。
    pub fn stop(&self) {
        self.stop_requested.store(true, Relaxed);
    }

    pub fn start(
        self: &Arc<Self>,
        root: PathBuf,
        shas: Vec<String>,
        edit: Value,
        set_id: String,
        output_source: String,
    ) -> Result<(), String> {
        let operations = recipe_operations(&edit)?;
        if !valid_set_id(&set_id) || output_source.is_empty() || output_source.len() > 256 {
            return Err("invalid filter set".into());
        }
        if shas.is_empty() || shas.len() > 100_000 || shas.iter().any(|s| !valid_sha(s)) {
            return Err("filter requires 1–100000 valid images".into());
        }
        if manifest_path(&root, &set_id).exists() {
            return Err("filter set already exists".into());
        }
        let mut seen = HashSet::new();
        let shas: Vec<_> = shas
            .into_iter()
            .filter(|s| seen.insert(s.clone()))
            .collect();
        let mut job = self.job.lock().unwrap_or_else(|p| p.into_inner());
        if job["running"] == true {
            return Err("a folder filter is already running".into());
        }
        self.stop_requested.store(false, Relaxed);
        *job = json!({"running": true, "alive": true, "done": 0, "total": shas.len(),
            "errors": 0, "cached": 0, "last": "", "set_id": set_id,
            "source": output_source, "completed": false});
        let state = self.clone();
        let spawned = std::thread::Builder::new()
            .name("folder-filter".into())
            .spawn(move || {
                lower_thread_priority();
                // Keep running=false even when an image decoder or an unexpected input panics.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(
                        &state,
                        &root,
                        &shas,
                        &edit,
                        &operations,
                        &set_id,
                        &output_source,
                    )
                }));
                let mut job = state.job.lock().unwrap_or_else(|p| p.into_inner());
                let failure = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(e),
                    Err(_) => Some("folder filter worker failed".into()),
                };
                if let Some(message) = failure {
                    job["errors"] = json!(job["errors"].as_u64().unwrap_or(0) + 1);
                    job["last"] = json!(message);
                }
                job["completed"] = json!(
                    job["done"] == job["total"]
                        && job["errors"] == 0
                        && !state.stop_requested.load(Relaxed)
                );
                job["running"] = json!(false);
                job["alive"] = json!(false);
            });
        if let Err(error) = spawned {
            job["running"] = json!(false);
            job["alive"] = json!(false);
            job["errors"] = json!(1);
            job["last"] = json!(error.to_string());
            return Err(error.to_string());
        }
        Ok(())
    }
}

pub fn ensure_schema(db: &Connection) -> Result<(), String> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS filter_members(
            set_id TEXT NOT NULL, original_sha TEXT NOT NULL, sha1 TEXT NOT NULL,
            PRIMARY KEY(set_id, original_sha));
         CREATE INDEX IF NOT EXISTS idx_filter_members_set_sha ON filter_members(set_id, sha1);
         CREATE TABLE IF NOT EXISTS filter_sets(set_id TEXT PRIMARY KEY, manifest_bytes INTEGER NOT NULL);",
    ).map_err(|e| e.to_string())
}

fn valid_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|c| c.is_ascii_hexdigit())
}

fn valid_set_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 160
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

fn recipe_operations(edit: &Value) -> Result<Value, String> {
    let operations = edit["params"]["edits"]
        .as_array()
        .filter(|ops| !ops.is_empty() && ops.len() <= 16)
        .ok_or("filter pipeline must contain 1–16 operations")?;
    if edit["op"] != "pipeline"
        || operations
            .iter()
            .any(|op| !matches!(op["op"].as_str(), Some("filter" | "adjust" | "auto")))
    {
        return Err("invalid filter pipeline".into());
    }
    // The API validates filter names and parameters. Presentation text never changes the cache.
    Ok(json!(operations
        .iter()
        .map(|op| json!({"op": op["op"], "params": op["params"]}))
        .collect::<Vec<_>>()))
}

fn cache_key(sha: &str, previous_edits: &Value, operations: &Value) -> String {
    let identity = json!([
        "folder-filter-v1",
        sha,
        edits::rev(previous_edits),
        operations
    ]);
    hex::encode(Sha1::digest(serde_json::to_vec(&identity).unwrap()))
}

fn manifest_path(root: &Path, set_id: &str) -> PathBuf {
    root.join("store/filter_sets")
        .join(format!("{set_id}.jsonl"))
}

fn cache_path(root: &Path, key: &str) -> PathBuf {
    root.join("store/filter_cache")
        .join(&key[..2])
        .join(format!("{key}.json"))
}

fn run(
    state: &FolderFilterState,
    root: &Path,
    shas: &[String],
    edit: &Value,
    operations: &Value,
    set_id: &str,
    output_source: &str,
) -> Result<(), String> {
    let db = Connection::open(root.join("store/index.sqlite")).map_err(|e| e.to_string())?;
    db.busy_timeout(Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    ensure_schema(&db)?;
    let manifest = manifest_path(root, set_id);
    std::fs::create_dir_all(manifest.parent().unwrap()).map_err(|e| e.to_string())?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&manifest)
        .map_err(|e| e.to_string())?;
    for sha in shas {
        if state.stop_requested.load(Relaxed) {
            break;
        }
        let result = materialize_one(root, &db, sha, edit, operations, output_source).and_then(
            |(output, cached)| {
                let entry = json!({"original_sha": sha, "sha1": output});
                writeln!(file, "{entry}").map_err(|e| e.to_string())?;
                let bytes = file.metadata().map_err(|e| e.to_string())?.len();
                let tx = db.unchecked_transaction().map_err(|e| e.to_string())?;
                tx.execute(
                    "INSERT OR REPLACE INTO filter_members(set_id,original_sha,sha1) VALUES(?,?,?)",
                    params![set_id, sha, output],
                )
                .map_err(|e| e.to_string())?;
                tx.execute(
                    "INSERT OR REPLACE INTO filter_sets(set_id,manifest_bytes) VALUES(?,?)",
                    params![set_id, bytes],
                )
                .map_err(|e| e.to_string())?;
                tx.commit().map_err(|e| e.to_string())?;
                Ok(cached)
            },
        );
        {
            let mut job = state.job.lock().unwrap_or_else(|p| p.into_inner());
            job["done"] = json!(job["done"].as_u64().unwrap_or(0) + 1);
            match result {
                Ok(cached) => {
                    if cached {
                        job["cached"] = json!(job["cached"].as_u64().unwrap_or(0) + 1);
                    }
                }
                Err(e) => {
                    job["errors"] = json!(job["errors"].as_u64().unwrap_or(0) + 1);
                    job["last"] = json!(format!("{sha}: {e}"));
                }
            }
        }
        // One decode at a time, no Rayon fan-out, with space for interactive work between images.
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn ordinary_output(root: &Path, sha: &str) -> Option<Value> {
    if !valid_sha(sha) {
        return None;
    }
    let meta = store::load_meta(root, sha)?;
    if meta["edits"].as_array().is_some_and(|a| !a.is_empty()) {
        return None;
    }
    let ext = meta["ext"].as_str()?;
    store::image_path(root, sha, ext).is_file().then_some(meta)
}

fn write_preview(root: &Path, sha: &str, img: &image::DynamicImage) -> Result<(), String> {
    let path = store::preview_path(root, sha);
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut data, 88)
        .encode_image(&img.thumbnail(1080, 1080).to_rgb8())
        .map_err(|e| e.to_string())?;
    std::fs::write(path, data).map_err(|e| e.to_string())
}

fn ensure_display_files(root: &Path, sha: &str, meta: &Value) -> Result<(), String> {
    let thumbs_missing =
        !store::thumb_path(root, sha).is_file() || !store::micro_path(root, sha).is_file();
    let preview_missing = !store::preview_path(root, sha).is_file();
    if thumbs_missing || preview_missing {
        // A cleaned-up preview is recovered from the finished image; filters never run on a hit.
        let img = image::open(store::image_path(
            root,
            sha,
            meta["ext"].as_str().unwrap_or("png"),
        ))
        .map_err(|e| e.to_string())?;
        if thumbs_missing {
            store::write_thumbs(root, sha, &img);
        }
        if preview_missing {
            write_preview(root, sha, &img)?;
        }
    }
    if !store::thumb_path(root, sha).is_file() || !store::micro_path(root, sha).is_file() {
        return Err("could not save image thumbnails".into());
    }
    Ok(())
}

fn ensure_indexed(db: &Connection, meta: &Value) -> Result<(), String> {
    let sha = meta["sha1"].as_str().ok_or("saved image SHA missing")?;
    let indexed = || -> Result<bool, String> {
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM images WHERE sha1=? AND (erev IS NULL OR erev=''))",
            [sha],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())
    };
    if !indexed()? {
        // Only metadata/index writes are inside this short transaction, never image processing.
        let tx = db.unchecked_transaction().map_err(|e| e.to_string())?;
        store::index_meta(&tx, meta);
        tx.commit().map_err(|e| e.to_string())?;
        if !indexed()? {
            return Err("could not index filtered image".into());
        }
    }
    Ok(())
}

fn materialize_one(
    root: &Path,
    db: &Connection,
    sha: &str,
    edit: &Value,
    operations: &Value,
    output_source: &str,
) -> Result<(String, bool), String> {
    let original = store::load_meta(root, sha).ok_or("source metadata missing")?;
    let path = store::image_path(
        root,
        sha,
        original["ext"].as_str().ok_or("source extension missing")?,
    );
    if !path.is_file() {
        return Err("source image missing".into());
    }
    let previous = original["edits"]
        .as_array()
        .map(|v| json!(v))
        .unwrap_or_else(|| json!([]));
    let key = cache_key(sha, &previous, operations);
    let cache_file = cache_path(root, &key);
    if let Some(cached_sha) = std::fs::read(&cache_file)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|cache| cache["sha1"].as_str().map(str::to_owned))
    {
        if let Some(meta) = ordinary_output(root, &cached_sha) {
            ensure_display_files(root, &cached_sha, &meta)?;
            // SQLite is disposable; restoring this one row requires no image decode or processing.
            ensure_indexed(db, &meta)?;
            return Ok((cached_sha, true));
        }
    }
    let image = edits::load(root, sha, original["ext"].as_str().unwrap(), &previous)
        .ok_or("source image or saved filter result missing")?;
    let image = edits::apply(image, &json!([edit]));
    // Keep color PNGs consistent for consumers of baked Canny/grayscale results; retain alpha.
    let image = if image.color().has_alpha() {
        image
    } else {
        image::DynamicImage::ImageRgb8(image.into_rgb8())
    };
    let mut png = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    let mut data = png_identity(png.into_inner(), &key);
    let mut output_sha = hex::encode(Sha1::digest(&data));
    // Someone may have edited an earlier baked output. Keep that history and bake a separate image.
    if store::meta_path(root, &output_sha).exists() && ordinary_output(root, &output_sha).is_none()
    {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        data = png_identity(data, &format!("{key}:{nonce}"));
        output_sha = hex::encode(Sha1::digest(&data));
    }
    write_preview(root, &output_sha, &image)?;
    drop(image); // Ingest performs its own decode; don't retain two full-resolution images.
    let mut extra = original.clone();
    if let Some(extra) = extra.as_object_mut() {
        for field in [
            "sha1",
            "ext",
            "w",
            "h",
            "bytes",
            "phash",
            "tint",
            "source",
            "ingested",
            "edits",
            "edits_rev",
            "erev",
            "redo",
            "seg",
            "faces",
            "emb",
            "embs",
        ] {
            extra.remove(field);
        }
    }
    extra["filter_source_sha"] = json!(sha);
    extra["filter_source"] = original["source"].clone();
    extra["filter_source_edits"] = previous;
    extra["filter_recipe"] = edit.clone();
    extra["filter_cache_key"] = json!(key);
    match store::ingest_bytes(root, db, &data, "png", output_source, &extra) {
        Ok(_) => {}
        Err("dup") if ordinary_output(root, &output_sha).is_some() => {}
        Err(error) => return Err(format!("could not save filtered image: {error}")),
    }
    let meta = ordinary_output(root, &output_sha).ok_or("saved image is unavailable")?;
    ensure_display_files(root, &output_sha, &meta)?;
    // index_meta intentionally ignores SQL failures elsewhere; ensure this result is actually indexed.
    ensure_indexed(db, &meta)?;
    std::fs::create_dir_all(cache_file.parent().unwrap()).map_err(|e| e.to_string())?;
    let tmp = cache_file.with_extension("tmp");
    std::fs::write(
        &tmp,
        json!({"sha1": output_sha, "original_sha": sha, "recipe": operations}).to_string(),
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(tmp, cache_file).map_err(|e| e.to_string())?;
    Ok((output_sha, false))
}

/// A standard PNG text chunk binds identical pixels to their own provenance/cache identity.
/// This prevents an identity operation from aliasing the original's live edit stack or rights.
fn png_identity(mut png: Vec<u8>, identity: &str) -> Vec<u8> {
    let text = format!("fluent_filter\0{identity}");
    let mut chunk = Vec::with_capacity(text.len() + 12);
    chunk.extend_from_slice(&(text.len() as u32).to_be_bytes());
    chunk.extend_from_slice(b"tEXt");
    chunk.extend_from_slice(text.as_bytes());
    let mut crc = !0u32;
    for byte in &chunk[4..] {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    chunk.extend_from_slice(&(!crc).to_be_bytes());
    // image's PNG encoder always ends with the twelve-byte IEND chunk.
    png.splice(png.len() - 12..png.len() - 12, chunk);
    png
}

/// Rebuild memberships from append-only manifests, skipping sets already indexed at this size.
/// Call after opening/rebuilding the disposable index; no images are decoded.
pub fn restore_members(root: &Path, db: &Connection) -> Result<usize, String> {
    ensure_schema(db)?;
    let dir = root.join("store/filter_sets");
    if !dir.exists() {
        return Ok(0);
    }
    let mut restored = 0;
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(set_id) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| valid_set_id(s))
        else {
            continue;
        };
        let bytes = entry.metadata().map_err(|e| e.to_string())?.len();
        let indexed_bytes: Option<u64> = db
            .query_row(
                "SELECT manifest_bytes FROM filter_sets WHERE set_id=?",
                [set_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if indexed_bytes == Some(bytes) {
            continue;
        }
        let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
        let tx = db.unchecked_transaction().map_err(|e| e.to_string())?;
        for line in std::io::BufReader::new(file).lines() {
            let line = line.map_err(|e| e.to_string())?;
            let Ok(record) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let (Some(original), Some(sha)) =
                (record["original_sha"].as_str(), record["sha1"].as_str())
            else {
                continue;
            };
            if !valid_sha(original) || !valid_sha(sha) {
                continue;
            }
            tx.execute(
                "INSERT OR REPLACE INTO filter_members(set_id,original_sha,sha1) VALUES(?,?,?)",
                params![set_id, original, sha],
            )
            .map_err(|e| e.to_string())?;
            restored += 1;
        }
        tx.execute(
            "INSERT OR REPLACE INTO filter_sets(set_id,manifest_bytes) VALUES(?,?)",
            params![set_id, bytes],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
    }
    Ok(restored)
}

fn lower_thread_priority() {
    // Linux niceness is per thread. Other platforms retain the sequential worker + short yields.
    #[cfg(target_os = "linux")]
    unsafe {
        unsafe extern "C" {
            fn nice(increment: std::ffi::c_int) -> std::ffi::c_int;
        }
        let _ = nice(10);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(label: &str) -> Value {
        json!({"op":"pipeline", "params":{"label":label,"prompt":label,
            "edits":[{"op":"filter","params":{"name":"invert"}}]}})
    }

    struct TempRoot(PathBuf);
    impl TempRoot {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "fluent-folder-filter-{}-{stamp}",
                std::process::id()
            ));
            std::fs::create_dir_all(root.join("store")).unwrap();
            Self(root)
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn cache_ignores_language_but_tracks_original_edits() {
        let one = recipe_operations(&filter("invert")).unwrap();
        let two = recipe_operations(&filter("色を反転して")).unwrap();
        assert_eq!(
            cache_key("source", &json!([]), &one),
            cache_key("source", &json!([]), &two)
        );
        assert_ne!(
            cache_key("source", &json!([]), &one),
            cache_key(
                "source",
                &json!([{"op":"rotate","params":{"deg":90}}]),
                &one
            )
        );
    }

    #[test]
    fn baked_image_keeps_provenance_and_cache_never_decodes_source() {
        let root = TempRoot::new();
        let db = Connection::open(root.0.join("store/index.sqlite")).unwrap();
        store::ensure_schema(&db);
        let original = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            20,
            12,
            image::Rgb([20, 40, 60]),
        ));
        let mut data = std::io::Cursor::new(Vec::new());
        original
            .write_to(&mut data, image::ImageFormat::Png)
            .unwrap();
        let sha = store::ingest_bytes(&root.0, &db, data.get_ref(), "png", "original", &json!({
            "rights":"cc-by", "credit":"photographer", "origin":"real", "crawl":{"url":"https://example.org/a"},
            "edits":[{"op":"rotate","params":{"deg":90}}], "seg":{"shapes":[{}]}
        })).unwrap();
        let original_meta = store::load_meta(&root.0, &sha).unwrap();
        let edit = filter("invert");
        let operations = recipe_operations(&edit).unwrap();
        let (output, cached) =
            materialize_one(&root.0, &db, &sha, &edit, &operations, "filter:test").unwrap();
        assert!(!cached);
        let meta = store::load_meta(&root.0, &output).unwrap();
        assert_eq!(meta["rights"], "cc-by");
        assert_eq!(meta["credit"], "photographer");
        assert_eq!(meta["crawl"], original_meta["crawl"]);
        assert_eq!(meta["filter_source_sha"], sha);
        assert!(meta["edits"].is_null() && meta["seg"].is_null());
        assert_eq!(
            (meta["w"].as_u64(), meta["h"].as_u64()),
            (Some(12), Some(20))
        );
        let result = image::open(store::image_path(&root.0, &output, "png"))
            .unwrap()
            .to_rgb8();
        assert_eq!(result.get_pixel(0, 0).0, [235, 215, 195]);
        let erev: Option<String> = db
            .query_row("SELECT erev FROM images WHERE sha1=?", [&output], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(erev, None);
        assert!(store::preview_path(&root.0, &output).is_file());
        assert!(store::thumb_path(&root.0, &output).is_file());
        assert!(store::micro_path(&root.0, &output).is_file());
        assert_eq!(store::load_meta(&root.0, &sha).unwrap(), original_meta);
        assert_eq!(
            std::fs::read(store::image_path(&root.0, &sha, "png")).unwrap(),
            *data.get_ref()
        );
        // A cache hit succeeds despite deliberately undecodable source bytes, proving no decode.
        std::fs::write(store::image_path(&root.0, &sha, "png"), b"not an image").unwrap();
        let hit = materialize_one(
            &root.0,
            &db,
            &sha,
            &filter("色を反転して"),
            &operations,
            "filter:another",
        )
        .unwrap();
        assert_eq!(hit, (output.clone(), true));
        std::fs::write(store::image_path(&root.0, &sha, "png"), data.get_ref()).unwrap();
        let mut edited_output = meta;
        edited_output["edits"] = json!([{"op":"rotate","params":{"deg":90}}]);
        store::save_meta(&root.0, &edited_output).unwrap();
        let (fresh, cached) =
            materialize_one(&root.0, &db, &sha, &edit, &operations, "filter:new").unwrap();
        assert!(!cached);
        assert_ne!(fresh, output);
        assert_eq!(store::load_meta(&root.0, &output).unwrap(), edited_output);
    }

    #[test]
    fn worker_indexes_members_and_manifest_restores_a_disposable_index() {
        let root = TempRoot::new();
        let db = Connection::open(root.0.join("store/index.sqlite")).unwrap();
        store::ensure_schema(&db);
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(8, 8)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let sha = store::ingest_bytes(&root.0, &db, bytes.get_ref(), "png", "source", &json!({}))
            .unwrap();
        let state = Arc::new(FolderFilterState::default());
        state
            .start(
                root.0.clone(),
                vec![sha.clone()],
                filter("invert"),
                "test_set".into(),
                "filter:test".into(),
            )
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while state.status()["running"] == true && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let status = state.status();
        assert_eq!(status["completed"], true, "{status}");
        assert_eq!(status["done"], 1);
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM filter_members WHERE set_id='test_set'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            1
        );
        let rebuilt = Connection::open_in_memory().unwrap();
        assert_eq!(restore_members(&root.0, &rebuilt).unwrap(), 1);
        assert_eq!(restore_members(&root.0, &rebuilt).unwrap(), 0);
        assert_eq!(
            rebuilt
                .query_row("SELECT original_sha FROM filter_members", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            sha
        );
    }
}

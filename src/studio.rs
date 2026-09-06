//! fluent_scene saves are private baked edit snapshots on the selected logical image.
//! Original bytes stay immutable; browsing reads finished pixels, never executes the graph.

use axum::{
    extract::{Multipart, Path as AxPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use base64::Engine;
use image::{DynamicImage, ImageFormat, ImageReader};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::{
    io::Cursor,
    path::{Path, PathBuf},
    sync::Mutex,
};

use crate::{edits, store, App, S};

const MAX_IMAGE_BYTES: usize = 64 << 20;
const MAX_RECIPE_BYTES: usize = 16 << 20;
const MAX_EDGE: u32 = 8192;
const MAX_PIXELS: u64 = 32_000_000;
type Failure = (StatusCode, String);

fn valid_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|c| c.is_ascii_hexdigit())
}

fn fail(status: StatusCode, message: &str) -> Failure {
    (status, message.into())
}
fn bad(message: &str) -> Failure {
    fail(StatusCode::BAD_REQUEST, message)
}
fn io_error(error: impl std::fmt::Display) -> Failure {
    fail(
        StatusCode::INTERNAL_SERVER_ERROR,
        &format!("加工画像を保存できませんでした: {error}"),
    )
}

fn validate_recipe(recipe: &Value) -> Result<(), Failure> {
    if recipe["save_mode"] != "in_place" {
        return Err(bad(
            "保存方式が更新されました。ページを再読み込みしてから適用してください",
        ));
    }
    if !recipe.is_object() || !recipe["graph"].is_object() || !recipe["sceneYaml"].is_string() {
        return Err(bad("recipe には graph と sceneYaml が必要です"));
    }
    if !recipe["graph"]["n"]
        .as_array()
        .is_some_and(|nodes| !nodes.is_empty() && nodes.len() <= 512)
        || !recipe["graph"]["e"]
            .as_array()
            .is_some_and(|edges| edges.len() <= 2048)
    {
        return Err(bad("フィルタグラフのノードまたは接続が不正です"));
    }
    for name in ["source_edits_rev", "target_edits_rev"] {
        if let Some(rev) = recipe.get(name) {
            let Some(rev) = rev.as_str() else {
                return Err(bad(&format!("{name} が不正です")));
            };
            if rev.len() != 12 || !rev.bytes().all(|c| c.is_ascii_hexdigit()) {
                return Err(bad(&format!("{name} が不正です")));
            }
        }
    }
    if recipe
        .get("input_sha")
        .is_some_and(|sha| !sha.as_str().is_some_and(valid_sha))
    {
        return Err(bad("input_sha が不正です"));
    }
    if let Some(edits) = recipe.get("photo_edits") {
        validate_photo_edits(edits)?;
    }
    Ok(())
}

fn allowed_params(params: &serde_json::Map<String, Value>, names: &[&str]) -> Result<(), Failure> {
    if params.keys().any(|name| !names.contains(&name.as_str())) {
        return Err(bad("未対応の写真調整パラメータです"));
    }
    Ok(())
}

fn bounded(
    params: &serde_json::Map<String, Value>,
    name: &str,
    min: f64,
    max: f64,
) -> Result<(), Failure> {
    if let Some(value) = params.get(name) {
        if !value
            .as_f64()
            .is_some_and(|value| value.is_finite() && value >= min && value <= max)
        {
            return Err(bad(&format!(
                "{name} は {min}〜{max} の数値で指定してください"
            )));
        }
    }
    Ok(())
}

fn validate_photo_edit(edit: &Value) -> Result<(), Failure> {
    let params = edit["params"]
        .as_object()
        .ok_or_else(|| bad("各写真調整に params が必要です"))?;
    match edit["op"].as_str() {
        Some("adjust") => {
            let names = ["exposure", "contrast", "saturation", "temperature"];
            allowed_params(params, &names)?;
            for name in names {
                bounded(params, name, -1.0, 1.0)?;
            }
        }
        Some("filter") => {
            crate::filter_commands::validate(
                &json!({"op": "pipeline", "params": {"edits": [edit]}}),
            )
            .map_err(|message| bad(&message))?;
            for (name, min, max) in [
                ("levels", 2.0, 32.0),
                ("amount", 0.1, 3.0),
                ("sigma", 0.3, 5.0),
                ("low", 0.0, 1020.0),
                ("high", 0.0, 1020.0),
            ] {
                bounded(params, name, min, max)?;
            }
        }
        Some("auto") => {
            allowed_params(params, &["version"])?;
            if params
                .get("version")
                .is_some_and(|version| version != 2 && version != edits::AUTO_VERSION)
            {
                return Err(bad("未対応の自動補正バージョンです"));
            }
        }
        Some("studio") => {
            allowed_params(params, &["render_sha"])?;
            if !params
                .get("render_sha")
                .and_then(Value::as_str)
                .is_some_and(valid_sha)
            {
                return Err(bad("保存済みフィルター画像の参照が不正です"));
            }
        }
        Some("rotate") => {
            allowed_params(params, &["deg"])?;
            if !params
                .get("deg")
                .and_then(Value::as_i64)
                .is_some_and(|deg| (-360..=360).contains(&deg) && deg % 90 == 0)
            {
                return Err(bad("回転は -360〜360 度の 90 度単位で指定してください"));
            }
        }
        Some("flip") => {
            allowed_params(params, &["dir"])?;
            if !matches!(params.get("dir").and_then(Value::as_str), Some("h" | "v")) {
                return Err(bad("反転の方向は h または v です"));
            }
        }
        Some("crop") => {
            let fractional = params.keys().any(|key| key.starts_with('f'));
            let names = if fractional {
                ["fx", "fy", "fw", "fh"]
            } else {
                ["x", "y", "w", "h"]
            };
            allowed_params(params, &names)?;
            if names.iter().any(|name| !params.contains_key(*name)) {
                return Err(bad("切り抜きの位置と幅・高さが必要です"));
            }
            for name in names {
                bounded(
                    params,
                    name,
                    0.0,
                    if fractional { 1.0 } else { f64::from(MAX_EDGE) },
                )?;
            }
            if params[names[2]].as_f64().unwrap() <= 0.0
                || params[names[3]].as_f64().unwrap() <= 0.0
            {
                return Err(bad("切り抜きの幅・高さは正の値にしてください"));
            }
            if fractional {
                if params["fx"].as_f64().unwrap() + params["fw"].as_f64().unwrap() > 1.000001
                    || params["fy"].as_f64().unwrap() + params["fh"].as_f64().unwrap() > 1.000001
                {
                    return Err(bad("切り抜きが画像の範囲を超えています"));
                }
            } else if names.iter().any(|name| params[*name].as_u64().is_none()) {
                return Err(bad("ピクセルの切り抜き位置・寸法は整数で指定してください"));
            }
        }
        _ => return Err(bad("未対応の写真調整です")),
    }
    Ok(())
}

fn validate_photo_edits(edits: &Value) -> Result<(), Failure> {
    let list = edits
        .as_array()
        .filter(|list| list.len() <= 64)
        .ok_or_else(|| bad("写真調整は 64 操作以下の配列で指定してください"))?;
    let mut operations = list.len();
    for edit in list {
        if edit["op"] == "pipeline" {
            crate::filter_commands::validate(edit).map_err(|message| bad(&message))?;
            let children = edit["params"]["edits"].as_array().unwrap();
            operations += children.len();
            for child in children {
                validate_photo_edit(child)?;
            }
        } else {
            validate_photo_edit(edit)?;
        }
        if operations > 64 {
            return Err(bad("写真調整は 64 操作以下にしてください"));
        }
    }
    Ok(())
}

fn validate_photo_references(root: &Path, edits: &Value) -> Result<(), Failure> {
    for edit in edits.as_array().into_iter().flatten() {
        if edit["op"] == "studio"
            && edit["params"]["render_sha"]
                .as_str()
                .and_then(|sha| asset_path(root, sha))
                .is_none()
        {
            return Err(bad("保存済みフィルター画像が見つかりません"));
        }
    }
    Ok(())
}

/// Render draft photo controls from the source file, without changing its saved edit stack.
pub async fn preview(
    State(app): S,
    AxPath(sha): AxPath<String>,
    Json(request): Json<Value>,
) -> Response {
    if sha.len() != 40 || !sha.bytes().all(|c| c.is_ascii_hexdigit()) {
        return crate::err_json(StatusCode::BAD_REQUEST, "元画像の ID が不正です");
    }
    if let Err((status, message)) = validate_photo_edits(&request["edits"]) {
        return crate::err_json(status, &message);
    }
    let revision = match request.get("source_edits_rev") {
        Some(Value::String(value))
            if value.len() == 12 && value.bytes().all(|c| c.is_ascii_hexdigit()) =>
        {
            Some(value.clone())
        }
        Some(_) => return crate::err_json(StatusCode::BAD_REQUEST, "source_edits_rev が不正です"),
        None => None,
    };
    app.touch_ui();
    // Slider bursts cannot run unbounded full-size decodes in parallel.
    static PREVIEWS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
    let Ok(permit) = PREVIEWS.acquire().await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        render_photo_preview(app, &sha, &request["edits"], revision.as_deref())
    })
    .await
    {
        Ok(Ok(bytes)) => (
            [
                (axum::http::header::CONTENT_TYPE, "image/png"),
                (axum::http::header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Ok(Err((status, message))) => crate::err_json(status, &message),
        Err(_) => crate::err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "写真調整のプレビューを作成できませんでした",
        ),
    }
}

fn render_photo_preview(
    app: &'static App,
    sha: &str,
    draft: &Value,
    requested_rev: Option<&str>,
) -> Result<Vec<u8>, Failure> {
    let (original, revision) = {
        let _db = app.db.lock().unwrap_or_else(|p| p.into_inner());
        let original = source_meta(&app.root, sha)?;
        let revision = edits::rev(&history(&original));
        if requested_rev.is_some_and(|requested| requested != revision) {
            return Err(fail(
                StatusCode::CONFLICT,
                "元画像の編集内容が変わりました。開き直してください",
            ));
        }
        (original, revision)
    };
    validate_photo_references(&app.root, draft)?;
    let path = store::image_path(&app.root, sha, original["ext"].as_str().unwrap());
    let (w, h) = ImageReader::open(&path)
        .map_err(|_| bad("元画像を読み取れませんでした"))?
        .into_dimensions()
        .map_err(|_| bad("元画像を読み取れませんでした"))?;
    if w == 0 || h == 0 || w > MAX_EDGE || h > MAX_EDGE || u64::from(w) * u64::from(h) > MAX_PIXELS
    {
        return Err(bad(
            "画像は各辺 8192 px 以下、3200 万画素以下にしてください",
        ));
    }
    let image = edits::load(&app.root, sha, original["ext"].as_str().unwrap(), draft)
        .ok_or_else(|| bad("元画像または保存済みフィルター画像を読み取れませんでした"))?;
    let mut png = Cursor::new(Vec::new());
    image
        .write_to(&mut png, ImageFormat::Png)
        .map_err(io_error)?;
    let _db = app.db.lock().unwrap_or_else(|p| p.into_inner());
    check_snapshot(&app.root, sha, &revision)?;
    Ok(png.into_inner())
}

/// Serve the actual scene editor bundle, with URL traversal handled by ServeDir.
pub fn assets(root: &Path) -> axum::Router<&'static App> {
    use tower_http::services::{ServeDir, ServeFile};
    axum::Router::new()
        .nest_service(
            "/fluent-scene",
            ServeDir::new(root.join("web/fluent-scene")),
        )
        .nest_service(
            "/studio-assets",
            ServeDir::new(root.join("store/studio_assets")),
        )
        .route_service(
            "/studio-gallery.js",
            ServeFile::new(root.join("web/studio-gallery.js")),
        )
        .route_service(
            "/studio-gallery.css",
            ServeFile::new(root.join("web/studio-gallery.css")),
        )
        .layer(axum::middleware::map_response(
            |mut response: Response| async {
                response.headers_mut().insert(
                    axum::http::header::CACHE_CONTROL,
                    axum::http::HeaderValue::from_static("no-cache"),
                );
                response
            },
        ))
}

pub async fn save(
    State(app): S,
    AxPath(sha): AxPath<String>,
    mut multipart: Multipart,
) -> Response {
    if sha.len() != 40 || !sha.bytes().all(|c| c.is_ascii_hexdigit()) {
        return crate::err_json(StatusCode::BAD_REQUEST, "元画像の ID が不正です");
    }
    let mut image = None;
    let mut recipe = None;
    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(_) => {
                return crate::err_json(StatusCode::BAD_REQUEST, "画像の送信を読み取れませんでした")
            }
        };
        let name = field.name().unwrap_or("").to_owned();
        let limit = match name.as_str() {
            "image" if image.is_none() => MAX_IMAGE_BYTES,
            "recipe" if recipe.is_none() => MAX_RECIPE_BYTES,
            _ => {
                return crate::err_json(
                    StatusCode::BAD_REQUEST,
                    "image と recipe を一つずつ送信してください",
                )
            }
        };
        let mut bytes = Vec::new();
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) if bytes.len().saturating_add(chunk.len()) <= limit => {
                    bytes.extend_from_slice(&chunk)
                }
                Ok(Some(_)) => {
                    return crate::err_json(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "画像または編集レシピが大きすぎます",
                    )
                }
                Ok(None) => break,
                Err(_) => {
                    return crate::err_json(
                        StatusCode::BAD_REQUEST,
                        "画像の送信が完了しませんでした",
                    )
                }
            }
        }
        if name == "image" {
            image = Some(bytes);
        } else {
            recipe = Some(bytes);
        }
    }
    let (Some(image), Some(recipe)) = (image, recipe) else {
        return crate::err_json(StatusCode::BAD_REQUEST, "image と recipe が必要です");
    };
    let recipe = match serde_json::from_slice::<Value>(&recipe) {
        Ok(recipe) => recipe,
        Err(_) => return crate::err_json(StatusCode::BAD_REQUEST, "編集レシピの JSON が不正です"),
    };
    if let Err((status, message)) = validate_recipe(&recipe) {
        return crate::err_json(status, &message);
    }
    app.touch_ui();
    match tokio::task::spawn_blocking(move || materialize(app, &sha, &image, &recipe)).await {
        Ok(Ok((meta, reused))) => {
            Json(json!({"ok": true, "sha1": meta["sha1"], "meta": meta, "reused": reused}))
                .into_response()
        }
        Ok(Err((status, message))) => crate::err_json(status, &message),
        Err(_) => crate::err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "加工画像の保存処理に失敗しました",
        ),
    }
}

fn source_meta(root: &Path, sha: &str) -> Result<Value, Failure> {
    let meta = store::load_meta(root, sha)
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "元画像が見つかりません"))?;
    let ext = meta["ext"]
        .as_str()
        .ok_or_else(|| bad("元画像の形式が不正です"))?;
    if !store::IMG_EXTS.contains(&ext) || !store::image_path(root, sha, ext).is_file() {
        return Err(fail(
            StatusCode::NOT_FOUND,
            "元画像のファイルが見つかりません",
        ));
    }
    Ok(meta)
}

/// Baked images have no live edit stack to clear. Follow provenance to the real source file.
/// This is a read-only resolution; even the root image's current editing history stays intact.
pub async fn original(State(app): S, AxPath(sha): AxPath<String>) -> Response {
    if sha.len() != 40 || !sha.bytes().all(|c| c.is_ascii_hexdigit()) {
        return crate::err_json(StatusCode::BAD_REQUEST, "画像の ID が不正です");
    }
    let _db = app.db.lock().unwrap_or_else(|p| p.into_inner());
    match resolve_original(&app.root, &sha) {
        Ok((mut meta, chain)) => {
            meta["edits_rev"] = json!(edits::rev(&history(&meta)));
            (
                [(axum::http::header::CACHE_CONTROL, "no-store")],
                Json(json!({"ok": true, "sha1": meta["sha1"], "meta": meta,
                    "derived": chain.len() > 1, "chain": chain})),
            )
                .into_response()
        }
        Err((status, message)) => crate::err_json(status, &message),
    }
}

fn resolve_original(root: &Path, sha: &str) -> Result<(Value, Vec<String>), Failure> {
    let mut seen = std::collections::HashSet::new();
    let mut chain = Vec::new();
    let mut current = sha.to_owned();
    loop {
        if !seen.insert(current.clone()) || chain.len() >= 64 {
            return Err(fail(
                StatusCode::CONFLICT,
                "原本への参照が循環しているため開けません",
            ));
        }
        chain.push(current.clone());
        let meta = source_meta(root, &current)?;
        let next = meta["studio"]
            .get("source_sha")
            .or_else(|| meta.get("filter_source_sha"));
        let Some(next) = next else {
            return Ok((meta, chain));
        };
        let Some(next) = next
            .as_str()
            .filter(|value| value.len() == 40 && value.bytes().all(|c| c.is_ascii_hexdigit()))
        else {
            return Err(fail(StatusCode::CONFLICT, "原本への参照が不正です"));
        };
        current = next.to_owned();
    }
}

fn history(meta: &Value) -> Value {
    meta["edits"]
        .as_array()
        .map(|value| json!(value))
        .unwrap_or_else(|| json!([]))
}

fn decode_png(bytes: &[u8]) -> Result<DynamicImage, Failure> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(bad("保存する画像は PNG で送信してください"));
    }
    let (w, h) = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png)
        .into_dimensions()
        .map_err(|_| bad("PNG 画像を読み取れませんでした"))?;
    if w == 0 || h == 0 || w > MAX_EDGE || h > MAX_EDGE || u64::from(w) * u64::from(h) > MAX_PIXELS
    {
        return Err(bad(
            "画像は各辺 8192 px 以下、3200 万画素以下にしてください",
        ));
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_EDGE);
    limits.max_image_height = Some(MAX_EDGE);
    limits.max_alloc = Some(512 << 20);
    reader.limits(limits);
    reader
        .decode()
        .map_err(|_| bad("PNG 画像を読み取れませんでした"))
}

/// Keep uploaded textures/source layers out of the frequently-read image sidecar.
/// Stable URLs are local to this gallery, and are fetched only when reopening the editor.
fn save_resources(root: &Path, recipe: &Value) -> Result<Value, Failure> {
    let mut recipe = recipe.clone();
    let Some(assets) = recipe.get_mut("assets") else {
        return Ok(recipe);
    };
    let assets = assets
        .as_array_mut()
        .filter(|assets| assets.len() <= 64)
        .ok_or_else(|| bad("追加画像は 64 件以下にしてください"))?;
    for asset in assets {
        if !matches!(asset["kind"].as_str(), Some("source" | "resource"))
            || !asset["name"]
                .as_str()
                .is_some_and(|name| name.len() <= 1024)
        {
            return Err(bad("追加画像の情報が不正です"));
        }
        let data = asset["data"]
            .as_str()
            .ok_or_else(|| bad("追加画像のデータがありません"))?;
        if let Some(name) = data.strip_prefix("/studio-assets/") {
            let (sha, ext) = name
                .split_once('.')
                .ok_or_else(|| bad("追加画像の URL が不正です"))?;
            if sha.len() != 40
                || !sha.bytes().all(|c| c.is_ascii_hexdigit())
                || !matches!(ext, "png" | "jpg" | "webp")
                || !root.join("store/studio_assets").join(name).is_file()
            {
                return Err(bad("保存済みの追加画像が見つかりません"));
            }
            continue;
        }
        let (encoded, format, ext) =
            if let Some(encoded) = data.strip_prefix("data:image/png;base64,") {
                (encoded, ImageFormat::Png, "png")
            } else if let Some(encoded) = data.strip_prefix("data:image/jpeg;base64,") {
                (encoded, ImageFormat::Jpeg, "jpg")
            } else if let Some(encoded) = data.strip_prefix("data:image/webp;base64,") {
                (encoded, ImageFormat::WebP, "webp")
            } else {
                return Err(bad("追加画像は PNG・JPEG・WebP で保存してください"));
            };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| bad("追加画像のデータが不正です"))?;
        let (w, h) = ImageReader::with_format(Cursor::new(&bytes), format)
            .into_dimensions()
            .map_err(|_| bad("追加画像を読み取れませんでした"))?;
        if w == 0
            || h == 0
            || w > MAX_EDGE
            || h > MAX_EDGE
            || u64::from(w) * u64::from(h) > MAX_PIXELS
        {
            return Err(bad(
                "追加画像は各辺 8192 px 以下、3200 万画素以下にしてください",
            ));
        }
        let mut reader = ImageReader::with_format(Cursor::new(&bytes), format);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_EDGE);
        limits.max_image_height = Some(MAX_EDGE);
        limits.max_alloc = Some(512 << 20);
        reader.limits(limits);
        reader
            .decode()
            .map_err(|_| bad("追加画像を読み取れませんでした"))?;
        let name = format!("{}.{ext}", hex::encode(Sha1::digest(&bytes)));
        crate::atomic_publish(&root.join("store/studio_assets").join(&name), &bytes)
            .map_err(io_error)?;
        asset["data"] = json!(format!("/studio-assets/{name}"));
    }
    Ok(recipe)
}

fn private_path(root: &Path, render_sha: &str, suffix: &str) -> PathBuf {
    root.join("store/studio_renders")
        .join(&render_sha[..2])
        .join(format!("{render_sha}.{suffix}"))
}

/// A baked edit is private storage, never an images/meta row or a gallery item.
pub fn asset_path(root: &Path, render_sha: &str) -> Option<PathBuf> {
    if !valid_sha(render_sha) {
        return None;
    }
    let path = private_path(root, render_sha, "png");
    path.is_file().then_some(path)
}

/// These tiers are prepared at save time and remain usable after ordinary render-cache cleanup.
pub fn display_path(root: &Path, render_sha: &str, width: u32) -> Option<PathBuf> {
    if !valid_sha(render_sha) || !matches!(width, 120 | 360 | 1080 | 1600) {
        return None;
    }
    let path = private_path(root, render_sha, &format!("{width}.jpg"));
    path.is_file().then_some(path)
}

fn load_snapshot(root: &Path, render_sha: &str) -> Option<Value> {
    asset_path(root, render_sha)?;
    let snapshot: Value =
        serde_json::from_slice(&std::fs::read(private_path(root, render_sha, "json")).ok()?)
            .ok()?;
    (snapshot["render_sha"] == render_sha
        && snapshot["version"] == 2
        && snapshot["recipe"].is_object())
    .then_some(snapshot)
}

/// Add an ephemeral editor view of the last baked history entry. Legacy `studio` provenance
/// stays untouched, so ordinary source resolution never follows a new self-reference.
pub fn decorate_meta(root: &Path, meta: &mut Value) {
    if let Some(object) = meta.as_object_mut() {
        object.remove("studio_edit");
    }
    let Some(list) = meta["edits"].as_array() else {
        return;
    };
    let Some(index) = list.iter().rposition(|edit| edit["op"] == "studio") else {
        return;
    };
    let Some(render_sha) = list[index]["params"]["render_sha"].as_str() else {
        return;
    };
    let Some(mut snapshot) = load_snapshot(root, render_sha) else {
        return;
    };
    snapshot["index"] = json!(index);
    snapshot["tail_edits"] = json!(&list[index + 1..]);
    meta["studio_edit"] = snapshot;
}

fn reply_meta(root: &Path, mut meta: Value) -> Value {
    meta["edits_rev"] = json!(edits::rev(&history(&meta)));
    meta["studio_save_mode"] = json!("in_place");
    decorate_meta(root, &mut meta);
    meta
}

fn materialize(
    app: &'static App,
    selected_sha: &str,
    bytes: &[u8],
    recipe: &Value,
) -> Result<(Value, bool), Failure> {
    static SAVES: Mutex<()> = Mutex::new(());
    let _save = SAVES.lock().unwrap_or_else(|p| p.into_inner());
    let input_sha = recipe["input_sha"].as_str().unwrap_or(selected_sha);
    let (target, input) = {
        let _db = app.db.lock().unwrap_or_else(|p| p.into_inner());
        (
            source_meta(&app.root, selected_sha)?,
            source_meta(&app.root, input_sha)?,
        )
    };
    let target_history = history(&target);
    let target_revision = edits::rev(&target_history);
    let input_history = history(&input);
    let input_revision = edits::rev(&input_history);
    if recipe
        .get("source_edits_rev")
        .is_some_and(|revision| revision != &json!(input_revision))
    {
        return Err(fail(
            StatusCode::CONFLICT,
            "入力画像の編集内容が変わりました。エディターを開き直してください",
        ));
    }
    let expected_target = recipe.get("target_edits_rev").or_else(|| {
        (input_sha == selected_sha)
            .then(|| recipe.get("source_edits_rev"))
            .flatten()
    });
    if expected_target.is_some_and(|revision| revision != &json!(target_revision)) {
        return Err(fail(
            StatusCode::CONFLICT,
            "この画像は別の操作で編集されました。開き直してください",
        ));
    }
    if let Some(photo_edits) = recipe.get("photo_edits") {
        validate_photo_references(&app.root, photo_edits)?;
    }
    let image = decode_png(bytes)?;
    let recipe = save_resources(&app.root, recipe)?;
    // CAS tokens describe the request, not its pixels. A subsequent unchanged save must
    // reuse its existing baked file even though the selected image's revision has advanced.
    let mut identity_recipe = recipe.clone();
    if let Some(object) = identity_recipe.as_object_mut() {
        for field in [
            "source_edits_rev",
            "target_edits_rev",
            "save_mode",
            "input_sha",
        ] {
            object.remove(field);
        }
    }
    let identity = hex::encode(Sha1::digest(
        serde_json::to_vec(&json!([
            "fluent-scene-in-place-v2",
            selected_sha,
            input_sha,
            identity_recipe
        ]))
        .map_err(io_error)?,
    ));
    let mut png = Cursor::new(Vec::new());
    image
        .write_to(&mut png, ImageFormat::Png)
        .map_err(io_error)?;
    let data = png_identity(png.into_inner(), &identity);
    let render_sha = hex::encode(Sha1::digest(&data));
    let snapshot = json!({"version": 2, "render_sha": render_sha, "identity": identity,
        "target_sha": selected_sha, "source_sha": input_sha, "source_edits": input_history,
        "source_edits_rev": input_revision, "recipe": recipe,
        "w": image.width(), "h": image.height(), "bytes": data.len(),
        "phash": store::phash64(&image), "tint": store::tint(&image)});
    write_display_files(&app.root, &render_sha, &image)?;
    crate::atomic_publish(&private_path(&app.root, &render_sha, "png"), &data).map_err(io_error)?;
    crate::atomic_publish(
        &private_path(&app.root, &render_sha, "json"),
        &serde_json::to_vec(&snapshot).map_err(io_error)?,
    )
    .map_err(io_error)?;
    if load_snapshot(&app.root, &render_sha).is_none() {
        return Err(io_error(
            "保存済みフィルター画像の情報を読み取れませんでした",
        ));
    }
    // The only logical image touched is the selected item; no INSERT of a new SHA occurs.
    // Preserve concurrently refreshed tags/attributes by reloading under the edit commit lock.
    let db = app.db.lock().unwrap_or_else(|p| p.into_inner());
    check_snapshot(&app.root, input_sha, &input_revision)?;
    check_snapshot(&app.root, selected_sha, &target_revision)?;
    let mut current = source_meta(&app.root, selected_sha)?;
    let mut list = history(&current).as_array().unwrap().clone();
    let legacy_thumbs = !list.is_empty() && current["original_thumbs"] != true;
    let reused = list
        .last()
        .is_some_and(|edit| edit["op"] == "studio" && edit["params"]["render_sha"] == render_sha);
    if !reused {
        list.push(json!({"op": "studio", "params": {"render_sha": render_sha}}));
        current["edits"] = json!(list);
        current["original_thumbs"] = json!(true);
        // A decorated API object must never become the persisted source of truth.
        if let Some(object) = current.as_object_mut() {
            object.remove("studio_edit");
            object.remove("studio_save_mode");
            object.remove("edits_rev");
        }
        store::save_meta(&app.root, &current).map_err(io_error)?;
        if legacy_thumbs {
            for path in [
                store::thumb_path(&app.root, selected_sha),
                store::micro_path(&app.root, selected_sha),
                store::preview_path(&app.root, selected_sha),
            ] {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    ensure_indexed(&db, &current)?;
    Ok((reply_meta(&app.root, current), reused))
}

fn check_snapshot(root: &Path, sha: &str, revision: &str) -> Result<(), Failure> {
    if edits::rev(&history(&source_meta(root, sha)?)) != revision {
        return Err(fail(
            StatusCode::CONFLICT,
            "元画像の編集内容が変わりました。エディターを開き直してください",
        ));
    }
    Ok(())
}

fn ensure_indexed(db: &rusqlite::Connection, meta: &Value) -> Result<(), Failure> {
    let tx = db.unchecked_transaction().map_err(io_error)?;
    store::index_meta(&tx, meta);
    let expected = edits::rev(&history(meta));
    let present: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM images WHERE sha1=? AND erev=?)",
            [meta["sha1"].as_str().unwrap(), &expected],
            |row| row.get(0),
        )
        .map_err(io_error)?;
    if !present {
        return Err(io_error("画像索引の更新に失敗しました"));
    }
    tx.commit().map_err(io_error)
}

fn write_display_files(root: &Path, render_sha: &str, image: &DynamicImage) -> Result<(), Failure> {
    for (width, quality) in [(120, 72), (360, 82), (1080, 88), (1600, 90)] {
        let path = private_path(root, render_sha, &format!("{width}.jpg"));
        if path.is_file() {
            continue;
        }
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality)
            .encode_image(&image.thumbnail(width, width).to_rgb8())
            .map_err(io_error)?;
        crate::atomic_publish(&path, &bytes).map_err(io_error)?;
    }
    Ok(())
}

/// Bind pixel-identical exports to their source and exact editor recipe.
fn png_identity(mut png: Vec<u8>, identity: &str) -> Vec<u8> {
    let text = format!("fluent_studio\0{identity}");
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
    png.splice(png.len() - 12..png.len() - 12, chunk);
    png
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_identity_preserves_rgba_and_separates_provenance() {
        let image = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            3,
            2,
            image::Rgba([27, 51, 90, 42]),
        ));
        let mut png = Cursor::new(Vec::new());
        image.write_to(&mut png, ImageFormat::Png).unwrap();
        let left = png_identity(png.get_ref().clone(), "left");
        let right = png_identity(png.into_inner(), "right");
        assert_ne!(Sha1::digest(&left), Sha1::digest(&right));
        assert_eq!(decode_png(&left).unwrap().into_rgba8(), image.into_rgba8());
    }

    #[test]
    fn rejects_non_png_and_incomplete_recipe() {
        assert!(decode_png(b"not png").is_err());
        assert!(validate_recipe(
            &json!({"graph": {}, "sceneYaml": "scene: {}", "source_edits_rev": "../"})
        )
        .is_err());
        assert!(validate_recipe(&json!({"sceneYaml": "scene: {}"})).is_err());
        assert!(validate_recipe(&json!({"save_mode": "in_place", "graph": {"n": [{"i": "n1", "t": "src"}], "e": []}, "sceneYaml": "scene: {}", "source_edits_rev": "0123456789ab"})).is_ok());
        assert!(validate_recipe(
            &json!({"graph": {"n": [{"i": "n1", "t": "src"}], "e": []}, "sceneYaml": "scene: {}"})
        )
        .is_err());
    }

    #[test]
    fn resolves_studio_and_folder_sources_without_changing_metadata() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "fluent-studio-provenance-{}-{nonce}",
            std::process::id()
        ));
        let (a, b, c) = ("a".repeat(40), "b".repeat(40), "c".repeat(40));
        let metas = [
            json!({"sha1": a, "ext": "png", "studio": {"source_sha": b}}),
            json!({"sha1": b, "ext": "png", "filter_source_sha": c, "filter_source_edits": []}),
            json!({"sha1": c, "ext": "png", "edits": [{"op": "auto"}]}),
        ];
        for meta in &metas {
            let path = store::image_path(&root, meta["sha1"].as_str().unwrap(), "png");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"source marker").unwrap();
            store::save_meta(&root, meta).unwrap();
        }
        let (meta, chain) = resolve_original(&root, &a).unwrap();
        assert_eq!(chain, vec![a.clone(), b.clone(), c.clone()]);
        assert_eq!(meta, metas[2]);
        for expected in &metas {
            assert_eq!(
                store::load_meta(&root, expected["sha1"].as_str().unwrap()).unwrap(),
                *expected
            );
        }
        let mut cycle = metas[2].clone();
        cycle["filter_source_sha"] = json!(a);
        store::save_meta(&root, &cycle).unwrap();
        assert_eq!(
            resolve_original(&root, &a).unwrap_err().0,
            StatusCode::CONFLICT
        );
        cycle["filter_source_sha"] = json!("0".repeat(40));
        store::save_meta(&root, &cycle).unwrap();
        assert_eq!(
            resolve_original(&root, &a).unwrap_err().0,
            StatusCode::NOT_FOUND
        );
        cycle["filter_source_sha"] = json!("../invalid");
        store::save_meta(&root, &cycle).unwrap();
        assert_eq!(
            resolve_original(&root, &a).unwrap_err().0,
            StatusCode::CONFLICT
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn validates_draft_photo_geometry_and_bounded_operations() {
        assert!(validate_photo_edits(&json!([])).is_ok());
        let draft = json!([
            {"op": "adjust", "params": {"exposure": 0.3}},
            {"op": "auto", "params": {}},
            {"op": "rotate", "params": {"deg": 270}},
            {"op": "flip", "params": {"dir": "h"}},
            {"op": "crop", "params": {"fx": 0.1, "fy": 0.2, "fw": 0.5, "fh": 0.6}},
            {"op": "pipeline", "params": {"edits": [{"op": "filter", "params": {"name": "canny"}}]}}
        ]);
        assert!(validate_photo_edits(&draft).is_ok());
        assert!(validate_photo_edits(
            &json!([{"op":"studio","params":{"render_sha":"a".repeat(40)}}])
        )
        .is_ok());
        for invalid in [
            json!([{"op": "adjust", "params": {"exposure": 99}}]),
            json!([{"op": "filter", "params": {"name": "blur", "amount": -1}}]),
            json!([{"op": "rotate", "params": {"deg": 45}}]),
            json!([{"op": "flip", "params": {"dir": "diagonal"}}]),
            json!([{"op": "crop", "params": {"fx": 0.5, "fy": 0.0, "fw": 1, "fh": 1}}]),
            json!([{"op": "crop", "params": {"x": 0, "y": 0, "w": 0, "h": 1}}]),
            json!([{"op": "unknown", "params": {}}]),
            json!(vec![json!({"op": "auto", "params": {}}); 65]),
        ] {
            assert!(validate_photo_edits(&invalid).is_err(), "{invalid}");
        }
        let invalid_recipe = json!({"graph": {"n": [{"i": "n1", "t": "src"}], "e": []},
            "sceneYaml": "scene: {}", "photo_edits": [{"op": "unknown", "params": {}}]});
        assert!(validate_recipe(&invalid_recipe).is_err());
    }

    #[test]
    fn active_studio_snapshot_is_private_and_follows_history_undo() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "fluent-studio-private-{}-{nonce}",
            std::process::id()
        ));
        let (selected, render) = ("a".repeat(40), "b".repeat(40));
        let png = private_path(&root, &render, "png");
        std::fs::create_dir_all(png.parent().unwrap()).unwrap();
        std::fs::write(png, b"private image marker").unwrap();
        std::fs::write(
            private_path(&root, &render, "json"),
            json!({"version": 2,
            "render_sha": render, "source_sha": selected, "recipe": {"graph": {"n": [], "e": []}}})
            .to_string(),
        )
        .unwrap();
        let mut meta = json!({"sha1": selected, "studio": {"source_sha": "c".repeat(40)}, "edits": [
            {"op": "auto", "params": {"version": 3}},
            {"op": "studio", "params": {"render_sha": render}},
            {"op": "flip", "params": {"dir": "h"}}
        ]});
        let provenance = meta["studio"].clone();
        decorate_meta(&root, &mut meta);
        assert_eq!(meta["studio_edit"]["index"], 1);
        assert_eq!(
            meta["studio_edit"]["tail_edits"],
            json!([{"op":"flip","params":{"dir":"h"}}])
        );
        assert_eq!(meta["studio"], provenance);
        assert!(!store::meta_path(&root, &render).exists());
        assert!(!store::image_path(&root, &render, "png").exists());
        meta["edits"] = json!([]);
        decorate_meta(&root, &mut meta);
        assert!(meta.get("studio_edit").is_none());
        assert!(
            asset_path(&root, &render).is_some(),
            "undo keeps the saved bytes available"
        );
        assert!(asset_path(&root, "../invalid").is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}

//! 自動セグメント — 計算はml-hubの実戦済みgdino2seg(GDINO→SAM2)に委譲(b64で渡す薄い結合)。
//! フォルダは目標=クラスを知っているので、プロンプトは goal から内蔵LLMが抽出する。
//! 結果はサイドカー seg:{prompt, shapes:[{cls,conf,points(正規化xy)}]} に保存(ウォーム後0.2秒/枚)。

use base64::Engine;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;

use crate::store;

const MLHUB: &str = "http://127.0.0.1:7000";

#[derive(Default)]
pub struct SegState {
    pub alive: AtomicBool,
    pub stop: AtomicBool,
    pub done: AtomicUsize,
    pub total: AtomicUsize,
    pub hits: AtomicUsize, // 1つ以上マスクが付いた枚数
    pub prompt: Mutex<String>,
    pub last: Mutex<String>,
}

impl SegState {
    pub fn status(&self) -> Value {
        json!({
            "alive": self.alive.load(Relaxed), "done": self.done.load(Relaxed),
            "total": self.total.load(Relaxed), "hits": self.hits.load(Relaxed),
            "prompt": self.prompt.lock().unwrap().clone(),
            "last": self.last.lock().unwrap().clone(),
        })
    }
}

/// 1枚をml-hubに投げてshapesを得る
pub async fn seg_one(client: &reqwest::Client, img_bytes: &[u8], prompt: &str) -> Result<Vec<Value>, String> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(img_bytes);
    let v: Value = client
        .post(format!("{MLHUB}/annotation/engine-on-image"))
        .json(&json!({"engine": "gdino2seg", "image_b64": b64, "prompt": prompt, "output": "seg"}))
        .timeout(std::time::Duration::from_secs(180))
        .send()
        .await
        .map_err(|e| format!("ml-hub接続失敗: {e}"))?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    if let Some(d) = v["detail"].as_str() {
        return Err(d.to_string());
    }
    let shapes: Vec<Value> = v["shapes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|s| s["kind"] == "polygon" && s["conf"].as_f64().unwrap_or(0.0) >= 0.35)
                .map(|s| json!({"cls": s["cls_name"], "conf": s["conf"], "points": s["points"]}))
                .collect()
        })
        .unwrap_or_default();
    Ok(shapes)
}

/// ジョブ: 対象shasを順に処理(既に同promptのマスク持ちはスキップ=冪等)
pub async fn run(
    root: PathBuf,
    client: reqwest::Client,
    st: std::sync::Arc<SegState>,
    shas: Vec<String>,
    prompt: String,
) {
    let db = rusqlite::Connection::open(root.join("store/index.sqlite")).unwrap();
    store::ensure_schema(&db);
    st.total.store(shas.len(), Relaxed);
    st.done.store(0, Relaxed);
    st.hits.store(0, Relaxed);
    *st.prompt.lock().unwrap() = prompt.clone();
    for sha1 in shas {
        if st.stop.load(Relaxed) {
            break;
        }
        st.done.fetch_add(1, Relaxed);
        let Some(mut m) = store::load_meta(&root, &sha1) else { continue };
        if m["seg"]["prompt"].as_str() == Some(prompt.as_str()) {
            if m["seg"]["shapes"].as_array().map(|a| !a.is_empty()).unwrap_or(false) {
                st.hits.fetch_add(1, Relaxed);
            }
            continue; // 同じお題で済み
        }
        let ext = m["ext"].as_str().unwrap_or("jpg").to_string();
        let Ok(bytes) = std::fs::read(store::image_path(&root, &sha1, &ext)) else { continue };
        match seg_one(&client, &bytes, &prompt).await {
            Ok(shapes) => {
                let n = shapes.len();
                m["seg"] = json!({"prompt": prompt, "model": "gdino2seg", "shapes": shapes,
                    "ts": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()});
                if store::save_meta(&root, &m).is_ok() {
                    store::index_meta(&db, &m);
                }
                crate::edits::clear_renders(&root, &sha1); // seg表示キャッシュを作り直させる
                if n > 0 {
                    st.hits.fetch_add(1, Relaxed);
                }
                *st.last.lock().unwrap() = format!("{}: {n}個", &sha1[..8]);
            }
            Err(e) => {
                *st.last.lock().unwrap() = e;
            }
        }
    }
    st.alive.store(false, Relaxed);
}

/// 点/箱プロンプトでSAM2切り直し(ml-hub /annotation/sam-on-image)。座標は0-1正規化
pub async fn sam_refine(
    client: &reqwest::Client,
    img_bytes: &[u8],
    points: &[Vec<f64>],
    labels: &[i64],
    box_: Option<&[f64]>,
    cls: &str,
) -> Result<Vec<Value>, String> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(img_bytes);
    let mut body = json!({"image_b64": b64, "cls": cls});
    if let Some(b) = box_ {
        body["box"] = json!(b);
    }
    if !points.is_empty() {
        body["points"] = json!(points);
        body["labels"] = json!(labels);
    }
    let v: Value = client
        .post(format!("{MLHUB}/annotation/sam-on-image"))
        .json(&body)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| format!("ml-hub接続不可: {e}"))?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    if let Some(d) = v["detail"].as_str() {
        return Err(d.to_string());
    }
    Ok(v["shapes"].as_array().cloned().unwrap_or_default())
}

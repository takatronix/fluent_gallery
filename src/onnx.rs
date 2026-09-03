//! ort(onnxruntime)常駐 — CLIP画像埋め込み(似た画像の芯)。CPUで1枚十数ms、GPU不要。
//! モデルは engine/models/clip-vision.onnx (Xenova/clip-vit-base-patch32, 512次元)。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use serde_json::{json, Value};

static CLIP: OnceLock<Option<Mutex<ort::session::Session>>> = OnceLock::new();

pub const MODEL_FILE: &str = "clip-vision.onnx";
const MODEL_URL: &str = "https://huggingface.co/Xenova/clip-vit-base-patch32/resolve/main/onnx/vision_model.onnx";
const MODEL_BYTES: u64 = 351_685_709;
static DOWNLOADING: AtomicBool = AtomicBool::new(false);
static GOT_MB: AtomicUsize = AtomicUsize::new(0);
static TOTAL_MB: AtomicUsize = AtomicUsize::new(0);

pub fn model_path(root: &Path) -> PathBuf {
    root.join("engine/models").join(MODEL_FILE)
}

/// UI向け: モデル有無とDL進捗(/api/activity の ai.clip)
pub fn status(root: &Path) -> Value {
    json!({
        "model": MODEL_FILE, "present": model_path(root).exists(), "size_mb": MODEL_BYTES >> 20,
        "downloading": DOWNLOADING.load(Relaxed),
        "got_mb": GOT_MB.load(Relaxed), "total_mb": TOTAL_MB.load(Relaxed),
    })
}

/// CLIPモデルを初回だけDL(.part→rename)。既にあれば即Ok。Mac販売版は同梱せず初回取得にする
pub async fn ensure_model(root: &Path, client: &reqwest::Client) -> Result<PathBuf, String> {
    use std::io::Write;
    let p = model_path(root);
    if p.exists() {
        return Ok(p);
    }
    if DOWNLOADING.swap(true, Relaxed) {
        return Err("CLIPモデルDL中です".into());
    }
    let r = async {
        std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
        let tmp = p.with_extension("onnx.part");
        let mut resp = client.get(MODEL_URL).send().await.map_err(|e| format!("CLIPモデルDL接続失敗: {e}"))?
            .error_for_status().map_err(|e| format!("CLIPモデルDL失敗: {e}"))?;
        let total = resp.content_length().unwrap_or(MODEL_BYTES);
        TOTAL_MB.store((total >> 20) as usize, Relaxed);
        GOT_MB.store(0, Relaxed);
        let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        let mut got: u64 = 0;
        while let Some(chunk) = resp.chunk().await.map_err(|e| format!("DL中断: {e}"))? {
            f.write_all(&chunk).map_err(|e| e.to_string())?;
            got += chunk.len() as u64;
            GOT_MB.store((got >> 20) as usize, Relaxed);
        }
        drop(f);
        if got != total {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("DLサイズ不一致({got}/{total})"));
        }
        std::fs::rename(&tmp, &p).map_err(|e| e.to_string())?;
        println!("🧭 clip-vision.onnx 取得完了({}MB)", got >> 20);
        Ok(p.clone())
    }.await;
    DOWNLOADING.store(false, Relaxed);
    r
}

// CLIP標準の正規化(ImageNetと違う専用値)
const MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_73];
const STD: [f32; 3] = [0.268_629_54, 0.261_302_58, 0.275_777_11];

fn clip(root: &Path) -> Option<&'static Mutex<ort::session::Session>> {
    // モデルが無い間は OnceLock を確定させない(後からDLした時に読み込めるように)
    if CLIP.get().is_none() && !model_path(root).exists() {
        return None;
    }
    let root = root.to_path_buf();
    CLIP.get_or_init(move || {
        let p = model_path(&root);
        // スレッド2本に制限: 既定(全コア)だと起動直後のバックフィルでUIまで重くなる(2026-09-03実害)
        let built = (|| -> Result<ort::session::Session, String> {
            let b = ort::session::Session::builder().map_err(|e| e.to_string())?;
            let mut b = b.with_intra_threads(2).map_err(|e| e.to_string())?;
            b.commit_from_file(&p).map_err(|e| e.to_string())
        })();
        match built {
            Ok(s) => {
                println!("🧭 clip-vision.onnx 読込OK");
                Some(Mutex::new(s))
            }
            Err(e) => {
                println!("⚠ clip-vision読込失敗({e}) — 似た画像は無効");
                None
            }
        }
    })
    .as_ref()
}

/// 画像→CLIP埋め込み(512次元・L2正規化済み)。モデル無し/失敗はNone。
pub fn embed(root: &Path, img: &image::DynamicImage) -> Option<Vec<f32>> {
    let s = clip(root)?;
    let im = img
        .resize_exact(224, 224, image::imageops::FilterType::Triangle)
        .into_rgb8();
    let mut data = vec![0f32; 3 * 224 * 224];
    for (x, y, p) in im.enumerate_pixels() {
        for c in 0..3 {
            data[c * 224 * 224 + (y as usize) * 224 + x as usize] =
                (p.0[c] as f32 / 255.0 - MEAN[c]) / STD[c];
        }
    }
    let tensor = ort::value::Tensor::from_array(([1usize, 3, 224, 224], data)).ok()?;
    let mut sess = s.lock().unwrap();
    let outputs = sess.run(ort::inputs!["pixel_values" => tensor]).ok()?;
    let (_, raw) = outputs[0].try_extract_tensor::<f32>().ok()?;
    let mut v: Vec<f32> = raw.to_vec();
    v.truncate(512);
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
    v.iter_mut().for_each(|x| *x /= n);
    Some(v)
}

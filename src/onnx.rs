//! ort(onnxruntime)常駐 — CLIP画像埋め込み(似た画像の芯)。CPUで1枚十数ms、GPU不要。
//! モデルは engine/models/clip-vision.onnx (Xenova/clip-vit-base-patch32, 512次元)。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use serde_json::{json, Value};

static CLIP: OnceLock<Option<Mutex<ort::session::Session>>> = OnceLock::new();

pub const MODEL_FILE: &str = "clip-vision.onnx";
// テキスト側(意味検索): 同じ Xenova/clip-vit-base-patch32 の text_model(MIT)。画像側と同じ 512 次元空間
pub const TEXT_FILE: &str = "clip-text.onnx";
const TEXT_URL: &str = "https://huggingface.co/Xenova/clip-vit-base-patch32/resolve/main/onnx/text_model.onnx";
const TEXT_BYTES: u64 = 254_058_553;
pub const TOK_FILE: &str = "clip-tokenizer.json";
const TOK_URL: &str = "https://huggingface.co/Xenova/clip-vit-base-patch32/resolve/main/tokenizer.json";
const TOK_BYTES: u64 = 2_224_119;
static TEXT: OnceLock<Option<(Mutex<ort::session::Session>, tokenizers::Tokenizer)>> = OnceLock::new();
const MODEL_URL: &str = "https://huggingface.co/Xenova/clip-vit-base-patch32/resolve/main/onnx/vision_model.onnx";
const MODEL_BYTES: u64 = 351_685_709;
static DOWNLOADING: AtomicBool = AtomicBool::new(false);
static GOT_MB: AtomicUsize = AtomicUsize::new(0);
static TOTAL_MB: AtomicUsize = AtomicUsize::new(0);

pub fn model_path(root: &Path) -> PathBuf {
    root.join("engine/models").join(MODEL_FILE)
}
pub fn text_path(root: &Path) -> PathBuf { root.join("engine/models").join(TEXT_FILE) }
pub fn tok_path(root: &Path) -> PathBuf { root.join("engine/models").join(TOK_FILE) }
pub fn text_present(root: &Path) -> bool { text_path(root).exists() && tok_path(root).exists() }

/// UI向け: モデル有無とDL進捗(/api/activity の ai.clip)
pub fn status(root: &Path) -> Value {
    json!({
        "model": MODEL_FILE, "present": model_path(root).exists(), "size_mb": MODEL_BYTES >> 20,
        "text_present": text_present(root), "text_size_mb": (TEXT_BYTES + TOK_BYTES) >> 20,
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

/// テキスト側(254MB+2MB)を初回だけDL。VLM 無しでも「dog」「a boat on water」で検索できるようになる
pub async fn ensure_text_model(root: &Path, client: &reqwest::Client) -> Result<(), String> {
    use std::io::Write;
    for (url, p, expect) in [(TEXT_URL, text_path(root), TEXT_BYTES), (TOK_URL, tok_path(root), TOK_BYTES)] {
        if p.metadata().map(|m| m.len() == expect).unwrap_or(false) { continue; }
        std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
        let tmp = p.with_extension("part");
        let mut resp = client.get(url).send().await.map_err(|e| format!("CLIPテキストDL接続失敗: {e}"))?
            .error_for_status().map_err(|e| format!("CLIPテキストDL失敗: {e}"))?;
        let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        let mut got: u64 = 0;
        while let Some(chunk) = resp.chunk().await.map_err(|e| format!("DL中断: {e}"))? {
            f.write_all(&chunk).map_err(|e| e.to_string())?;
            got += chunk.len() as u64;
        }
        drop(f);
        if got != expect { let _ = std::fs::remove_file(&tmp); return Err(format!("DLサイズ不一致({got}/{expect})")); }
        std::fs::rename(&tmp, &p).map_err(|e| e.to_string())?;
        println!("🧭 {} 取得完了({}MB)", p.file_name().unwrap().to_string_lossy(), got >> 20);
    }
    Ok(())
}

fn text(root: &Path) -> Option<&'static (Mutex<ort::session::Session>, tokenizers::Tokenizer)> {
    if TEXT.get().is_none() && !text_present(root) { return None; }
    let root = root.to_path_buf();
    TEXT.get_or_init(move || {
        let built = (|| -> Result<_, String> {
            let tok = tokenizers::Tokenizer::from_file(tok_path(&root)).map_err(|e| e.to_string())?;
            let b = ort::session::Session::builder().map_err(|e| e.to_string())?;
            let mut b = b.with_intra_threads(2).map_err(|e| e.to_string())?;
            let s = b.commit_from_file(text_path(&root)).map_err(|e| e.to_string())?;
            Ok((Mutex::new(s), tok))
        })();
        match built {
            Ok(x) => { println!("🧭 clip-text.onnx 読込OK(意味検索)"); Some(x) }
            Err(e) => { println!("⚠ clip-text読込失敗({e}) — 意味検索は無効"); None }
        }
    }).as_ref()
}

/// 英語テキスト→CLIP埋め込み(512次元・L2正規化)。画像埋め込みとの内積が類似度。1件 1ms 程度
pub fn embed_text(root: &Path, q: &str) -> Option<Vec<f32>> {
    let (sess, tok) = text(root)?;
    let enc = tok.encode(q, true).ok()?;
    let ids: Vec<i64> = enc.get_ids().iter().map(|&x| x as i64).collect();
    let n = ids.len();
    let tensor = ort::value::Tensor::from_array(([1usize, n], ids)).ok()?;
    let mut s = sess.lock().unwrap();
    let outputs = s.run(ort::inputs!["input_ids" => tensor]).ok()?;
    let (_, raw) = outputs[0].try_extract_tensor::<f32>().ok()?;
    let mut v: Vec<f32> = raw.to_vec();
    v.truncate(512);
    let nrm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
    v.iter_mut().for_each(|x| *x /= nrm);
    Some(v)
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

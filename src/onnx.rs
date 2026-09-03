//! ort(onnxruntime)常駐 — CLIP画像埋め込み(似た画像の芯)。CPUで1枚十数ms、GPU不要。
//! モデルは engine/models/clip-vision.onnx (Xenova/clip-vit-base-patch32, 512次元)。

use std::path::Path;
use std::sync::{Mutex, OnceLock};

static CLIP: OnceLock<Option<Mutex<ort::session::Session>>> = OnceLock::new();

// CLIP標準の正規化(ImageNetと違う専用値)
const MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_73];
const STD: [f32; 3] = [0.268_629_54, 0.261_302_58, 0.275_777_11];

fn clip(root: &Path) -> Option<&'static Mutex<ort::session::Session>> {
    let root = root.to_path_buf();
    CLIP.get_or_init(move || {
        let p = root.join("engine/models/clip-vision.onnx");
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

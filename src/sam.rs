//! SAM2(内蔵) — クリックや範囲からマスクを切る。外部サービスには一切投げない。
//!
//! 重みは onnx-community/sam2-hiera-tiny の ONNX 2本(Apache-2.0):
//!   vision_encoder 134MB … 画像を1回だけ潰す。重い(CPUで約1.5秒)
//!   prompt_encoder_mask_decoder 21MB … クリック1回ごとに走る。軽い(約0.08秒)
//! なので「画像の埋め込みは sha1 で数枚ぶん覚えておき、2回目からのクリックは即返す」構造にしてある。
//! ORT は CPU 実行(このビルドの cuda フィーチャは内蔵LLM用で、ONNX には効かない)。

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};

pub const ENC_FILE: &str = "sam2_vision_encoder.onnx";
pub const DEC_FILE: &str = "sam2_prompt_encoder_mask_decoder.onnx";
const ENC_URL: &str = "https://huggingface.co/onnx-community/sam2-hiera-tiny/resolve/main/onnx/vision_encoder.onnx";
const DEC_URL: &str = "https://huggingface.co/onnx-community/sam2-hiera-tiny/resolve/main/onnx/prompt_encoder_mask_decoder.onnx";
const ENC_BYTES: u64 = 134_261_339;
const DEC_BYTES: u64 = 20_657_357;
/// エンコーダの入力は 1024²固定。正規化は ImageNet(preprocessor_config.json)
const SIDE: usize = 1024;
const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const STD: [f32; 3] = [0.229, 0.224, 0.225];
/// デコーダが返すマスクは 256²
const MSIDE: usize = 256;

static ENC: OnceLock<Option<Mutex<ort::session::Session>>> = OnceLock::new();
static DEC: OnceLock<Option<Mutex<ort::session::Session>>> = OnceLock::new();
static DOWNLOADING: AtomicBool = AtomicBool::new(false);
static GOT_MB: AtomicUsize = AtomicUsize::new(0);
static TOTAL_MB: AtomicUsize = AtomicUsize::new(0);

pub fn enc_path(root: &Path) -> PathBuf { root.join("engine/models").join(ENC_FILE) }
pub fn dec_path(root: &Path) -> PathBuf { root.join("engine/models").join(DEC_FILE) }
pub fn present(root: &Path) -> bool { enc_path(root).exists() && dec_path(root).exists() }

pub fn status(root: &Path) -> Value {
    json!({
        "model": "sam2-hiera-tiny", "license": "apache-2.0",
        "present": present(root), "size_mb": (ENC_BYTES + DEC_BYTES) >> 20,
        "downloading": DOWNLOADING.load(Relaxed), "got_mb": GOT_MB.load(Relaxed), "total_mb": TOTAL_MB.load(Relaxed),
    })
}

/// 初回だけ2本まとめてDL(.part→rename)。既にあれば即 Ok
pub async fn ensure_model(root: &Path, client: &reqwest::Client) -> Result<(), String> {
    use std::io::Write;
    if present(root) { return Ok(()); }
    if DOWNLOADING.swap(true, Relaxed) { return Err("SAM2 のDL中です".into()); }
    let r = async {
        std::fs::create_dir_all(enc_path(root).parent().unwrap()).map_err(|e| e.to_string())?;
        TOTAL_MB.store(((ENC_BYTES + DEC_BYTES) >> 20) as usize, Relaxed);
        GOT_MB.store(0, Relaxed);
        let mut done: u64 = 0;
        for (p, url, bytes) in [(enc_path(root), ENC_URL, ENC_BYTES), (dec_path(root), DEC_URL, DEC_BYTES)] {
            if p.exists() { done += bytes; GOT_MB.store((done >> 20) as usize, Relaxed); continue; }
            let tmp = p.with_extension("part");
            let mut resp = client.get(url).send().await.map_err(|e| format!("SAM2 DL接続失敗: {e}"))?
                .error_for_status().map_err(|e| format!("SAM2 DL失敗: {e}"))?;
            let total = resp.content_length().unwrap_or(bytes);
            let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
            let mut got: u64 = 0;
            while let Some(c) = resp.chunk().await.map_err(|e| format!("DL中断: {e}"))? {
                f.write_all(&c).map_err(|e| e.to_string())?;
                got += c.len() as u64;
                GOT_MB.store(((done + got) >> 20) as usize, Relaxed);
            }
            drop(f);
            if got != total {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("DLサイズ不一致({got}/{total})"));
            }
            std::fs::rename(&tmp, &p).map_err(|e| e.to_string())?;
            done += got;
        }
        println!("✂ SAM2(内蔵)取得完了({}MB)", (ENC_BYTES + DEC_BYTES) >> 20);
        Ok(())
    }.await;
    DOWNLOADING.store(false, Relaxed);
    r
}

fn session(cell: &'static OnceLock<Option<Mutex<ort::session::Session>>>, p: PathBuf, what: &str)
    -> Option<&'static Mutex<ort::session::Session>> {
    if cell.get().is_none() && !p.exists() { return None; }
    let what = what.to_string();
    cell.get_or_init(move || {
        // CLIP と同じくスレッドは絞る(既定=全コアだと収集中に UI まで重くなる)
        // SAM2 は GPU の効きが桁違い(4090実測 1.568秒 → 0.028秒)。既定で載せにいく
        let built = crate::ep::build(&p, 4, crate::ep::gpu_available(), &what);
        match built {
            Ok(s) => { println!("✂ {what} 読込OK"); Some(Mutex::new(s)) }
            Err(e) => { println!("⚠ {what} 読込失敗({e}) — マスクは無効"); None }
        }
    }).as_ref()
}

/// 1枚ぶんの画像埋め込み(デコーダに毎回渡す3本)
pub struct Feats {
    hi0: Vec<f32>, // [1,32,256,256]
    hi1: Vec<f32>, // [1,64,128,128]
    emb: Vec<f32>, // [1,256,64,64]
}
/// 直近数枚だけ覚える(1枚あたり約12MB)。クリックのたびに1.5秒待たせないため
static CACHE: Mutex<VecDeque<(String, Arc<Feats>)>> = Mutex::new(VecDeque::new());
const CACHE_N: usize = 4;

/// 画像→埋め込み。同じ sha1 が直近にあれば使い回す
pub fn encode(root: &Path, sha1: &str, img: &image::DynamicImage) -> Option<Arc<Feats>> {
    if let Some((_, f)) = CACHE.lock().unwrap().iter().find(|(k, _)| k == sha1) {
        return Some(f.clone());
    }
    let s = session(&ENC, enc_path(root), ENC_FILE)?;
    let r = img.resize_exact(SIDE as u32, SIDE as u32, image::imageops::FilterType::Triangle).to_rgb8();
    let mut x = vec![0f32; 3 * SIDE * SIDE];
    for (i, p) in r.pixels().enumerate() {
        for c in 0..3 {
            x[c * SIDE * SIDE + i] = (p[c] as f32 / 255.0 - MEAN[c]) / STD[c];
        }
    }
    let t = ort::value::Tensor::from_array(([1usize, 3, SIDE, SIDE], x)).ok()?;
    let mut sess = s.lock().unwrap();
    let out = sess.run(ort::inputs!["pixel_values" => t]).ok()?;
    let grab = |name: &str| -> Option<Vec<f32>> {
        out.get(name)?.try_extract_tensor::<f32>().ok().map(|(_, d)| d.to_vec())
    };
    let f = Arc::new(Feats {
        hi0: grab("high_res_feats_0")?,
        hi1: grab("high_res_feats_1")?,
        emb: grab("image_embeddings")?,
    });
    let mut c = CACHE.lock().unwrap();
    c.push_back((sha1.to_string(), f.clone()));
    while c.len() > CACHE_N { c.pop_front(); }
    Some(f)
}

/// 埋め込みを捨てる(マスクを消した/画像を編集した時)
pub fn forget(sha1: &str) { CACHE.lock().unwrap().retain(|(k, _)| k != sha1); }

/// クリック点(正規化xy と 1=前景/0=背景)と範囲(正規化 x1y1x2y2)からマスクを切る。
/// 返り値は 256² のロジットと iou。SAM の作法で範囲は「左上=ラベル2, 右下=ラベル3」の2点に化ける
pub fn segment(root: &Path, f: &Feats, points: &[(f32, f32, f32)], bx: Option<[f32; 4]>) -> Option<(Vec<f32>, f32)> {
    let s = session(&DEC, dec_path(root), DEC_FILE)?;
    let mut pts: Vec<f32> = vec![];
    let mut labs: Vec<f32> = vec![];
    if let Some(b) = bx {
        pts.extend_from_slice(&[b[0] * SIDE as f32, b[1] * SIDE as f32, b[2] * SIDE as f32, b[3] * SIDE as f32]);
        labs.extend_from_slice(&[2.0, 3.0]);
    }
    for (x, y, l) in points {
        pts.extend_from_slice(&[x * SIDE as f32, y * SIDE as f32]);
        labs.push(*l);
    }
    if labs.is_empty() { return None; }
    let n = labs.len();
    let mut sess = s.lock().unwrap();
    let out = sess.run(ort::inputs![
        "image_embeddings" => ort::value::Tensor::from_array(([1usize, 256, 64, 64], f.emb.clone())).ok()?,
        "high_res_feats_0" => ort::value::Tensor::from_array(([1usize, 32, 256, 256], f.hi0.clone())).ok()?,
        "high_res_feats_1" => ort::value::Tensor::from_array(([1usize, 64, 128, 128], f.hi1.clone())).ok()?,
        "input_points" => ort::value::Tensor::from_array(([1usize, n, 2], pts)).ok()?,
        "input_labels" => ort::value::Tensor::from_array(([1usize, n], labs)).ok()?,
        "input_masks" => ort::value::Tensor::from_array(([1usize, 1, MSIDE, MSIDE], vec![0f32; MSIDE * MSIDE])).ok()?,
        "has_input_masks" => ort::value::Tensor::from_array(([1usize], vec![0f32])).ok()?,
    ]).ok()?;
    let (_, masks) = out.get("pred_masks")?.try_extract_tensor::<f32>().ok()?;
    let (_, iou) = out.get("iou_scores")?.try_extract_tensor::<f32>().ok()?;
    // 3案返ってくるので iou が一番高い物を採る
    let best = (0..iou.len()).max_by(|a, b| iou[*a].partial_cmp(&iou[*b]).unwrap())?;
    let off = best * MSIDE * MSIDE;
    Some((masks.get(off..off + MSIDE * MSIDE)?.to_vec(), iou[best]))
}

/// ロジット→輪郭ポリゴン(正規化・時計回り)。小さすぎる島は捨てる
pub fn mask_to_shapes(mask: &[f32], cls: &str, conf: f32) -> Vec<Value> {
    let bin: Vec<bool> = mask.iter().map(|v| *v > 0.0).collect();
    let mut seen = vec![false; MSIDE * MSIDE];
    let mut out = vec![];
    for sy in 0..MSIDE {
        for sx in 0..MSIDE {
            let i = sy * MSIDE + sx;
            if !bin[i] || seen[i] { continue; }
            // 連結成分を塗って大きさを測る(小さい島は輪郭も取らない)
            let mut stack = vec![(sx, sy)];
            let mut area = 0usize;
            let mut cells = vec![];
            seen[i] = true;
            while let Some((x, y)) = stack.pop() {
                area += 1;
                cells.push((x, y));
                for (dx, dy) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
                    let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                    if nx < 0 || ny < 0 || nx >= MSIDE as i32 || ny >= MSIDE as i32 { continue; }
                    let j = ny as usize * MSIDE + nx as usize;
                    if bin[j] && !seen[j] { seen[j] = true; stack.push((nx as usize, ny as usize)); }
                }
            }
            if area < 64 { continue; } // 256²のうち64画素未満=ゴミ
            let mut comp = vec![false; MSIDE * MSIDE];
            for (x, y) in cells { comp[y * MSIDE + x] = true; }
            if let Some(poly) = trace(&comp, sx, sy) {
                let simp = simplify(&poly, 1.2);
                if simp.len() >= 3 {
                    let pts: Vec<f32> = simp.iter().flat_map(|(x, y)| {
                        [*x as f32 / MSIDE as f32, *y as f32 / MSIDE as f32]
                    }).collect();
                    out.push(json!({"cls": cls, "conf": conf, "points": pts}));
                }
            }
        }
    }
    out
}

/// Moore近傍の輪郭追跡(外周だけ)。start は成分の最初に見つかった画素
fn trace(m: &[bool], sx: usize, sy: usize) -> Option<Vec<(usize, usize)>> {
    const D: [(i32, i32); 8] = [(1, 0), (1, 1), (0, 1), (-1, 1), (-1, 0), (-1, -1), (0, -1), (1, -1)];
    let at = |x: i32, y: i32| -> bool {
        x >= 0 && y >= 0 && (x as usize) < MSIDE && (y as usize) < MSIDE && m[y as usize * MSIDE + x as usize]
    };
    let (mut cx, mut cy) = (sx as i32, sy as i32);
    let mut dir = 0usize;
    let mut poly = vec![(sx, sy)];
    for _ in 0..(MSIDE * MSIDE * 4) {
        let mut moved = false;
        for k in 0..8 {
            let d = (dir + 6 + k) % 8; // 一つ戻ってから左回りに探す
            let (nx, ny) = (cx + D[d].0, cy + D[d].1);
            if at(nx, ny) {
                cx = nx; cy = ny; dir = d; moved = true;
                poly.push((cx as usize, cy as usize));
                break;
            }
        }
        if !moved { break; }
        if cx == sx as i32 && cy == sy as i32 && poly.len() > 2 { break; }
    }
    (poly.len() >= 3).then_some(poly)
}

/// Douglas-Peucker。点を減らさないとサイドカーが太る
fn simplify(p: &[(usize, usize)], eps: f32) -> Vec<(usize, usize)> {
    if p.len() < 3 { return p.to_vec(); }
    let (a, b) = (p[0], p[p.len() - 1]);
    let (mut far, mut fd) = (0usize, 0f32);
    for (i, q) in p.iter().enumerate().take(p.len() - 1).skip(1) {
        let (x0, y0, x1, y1) = (a.0 as f32, a.1 as f32, b.0 as f32, b.1 as f32);
        let (px, py) = (q.0 as f32, q.1 as f32);
        let (dx, dy) = (x1 - x0, y1 - y0);
        let den = (dx * dx + dy * dy).sqrt();
        let d = if den < 1e-6 { ((px - x0).powi(2) + (py - y0).powi(2)).sqrt() }
                else { ((py - y0) * dx - (px - x0) * dy).abs() / den };
        if d > fd { fd = d; far = i; }
    }
    if fd <= eps { return vec![a, b]; }
    let mut l = simplify(&p[..=far], eps);
    let r = simplify(&p[far..], eps);
    l.pop();
    l.extend(r);
    l
}

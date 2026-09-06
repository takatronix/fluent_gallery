//! 言葉で物を探す(内蔵 GroundingDINO tiny)。外部サービスには投げない。
//!
//! 重みは onnx-community/grounding-dino-tiny-ONNX の int8(204MB, Apache-2.0)＋BERT tokenizer。
//! 「dog. person.」のようにクラス語をピリオド終端で並べて渡すと、900個の候補から箱が返る。
//! そのまま sam::segment に箱を渡すと精密な輪郭になる(合計 約5秒/枚 CPU)。
//!
//! 実装は過去プロジェクトで実戦済みの手順を踏襲している。特に効くのが2点:
//!  - 返り句を必ず元のクラス名へ引き戻す(部分句 "tray" が新クラスにならないように)
//!  - クラス語が多いとモデル内部で token 長 256 を超えて壊れるので、24語ずつに分けて何度も検出する

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

pub const MODEL_FILE: &str = "gdino_tiny_int8.onnx";
pub const TOK_FILE: &str = "gdino_tokenizer.json";
const MODEL_URL: &str = "https://huggingface.co/onnx-community/grounding-dino-tiny-ONNX/resolve/main/onnx/model_int8.onnx";
const TOK_URL: &str = "https://huggingface.co/onnx-community/grounding-dino-tiny-ONNX/resolve/main/tokenizer.json";
const MODEL_BYTES: u64 = 203_795_968;
/// 入力は 800²固定・ImageNet正規化(preprocessor_config.json)
const SIDE: usize = 800;
const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const STD: [f32; 3] = [0.229, 0.224, 0.225];
/// 1回のプロンプトに詰めるクラス語の上限(超えると token 256 を越えて壊れる)
const CHUNK: usize = 24;
/// 箱を採るしきい値 / 返り句のトークンを拾うしきい値
pub const BOX_THR: f32 = 0.35;
const TEXT_THR: f32 = 0.25;
/// 同じクラスの重なりを潰す
const NMS_IOU: f32 = 0.85;

static SESS: OnceLock<Option<Mutex<ort::session::Session>>> = OnceLock::new();
static TOK: OnceLock<Option<tokenizers::Tokenizer>> = OnceLock::new();
static DOWNLOADING: AtomicBool = AtomicBool::new(false);
static GOT_MB: AtomicUsize = AtomicUsize::new(0);
static TOTAL_MB: AtomicUsize = AtomicUsize::new(0);

pub fn model_path(root: &Path) -> PathBuf { root.join("engine/models").join(MODEL_FILE) }
pub fn tok_path(root: &Path) -> PathBuf { root.join("engine/models").join(TOK_FILE) }
pub fn present(root: &Path) -> bool { model_path(root).exists() && tok_path(root).exists() }

pub fn status(root: &Path) -> Value {
    json!({
        "model": "grounding-dino-tiny(int8)", "license": "apache-2.0",
        "present": present(root), "size_mb": MODEL_BYTES >> 20,
        "downloading": DOWNLOADING.load(Relaxed), "got_mb": GOT_MB.load(Relaxed), "total_mb": TOTAL_MB.load(Relaxed),
    })
}

pub async fn ensure_model(root: &Path, client: &reqwest::Client) -> Result<(), String> {
    use std::io::Write;
    if present(root) { return Ok(()); }
    if DOWNLOADING.swap(true, Relaxed) { return Err("検出モデルのDL中です".into()); }
    let r = async {
        std::fs::create_dir_all(model_path(root).parent().unwrap()).map_err(|e| e.to_string())?;
        TOTAL_MB.store((MODEL_BYTES >> 20) as usize, Relaxed);
        GOT_MB.store(0, Relaxed);
        for (p, url, check) in [(model_path(root), MODEL_URL, true), (tok_path(root), TOK_URL, false)] {
            if p.exists() { continue; }
            let tmp = p.with_extension("part");
            let mut resp = client.get(url).send().await.map_err(|e| format!("DL接続失敗: {e}"))?
                .error_for_status().map_err(|e| format!("DL失敗: {e}"))?;
            let total = resp.content_length().unwrap_or(0);
            let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
            let mut got: u64 = 0;
            while let Some(c) = resp.chunk().await.map_err(|e| format!("DL中断: {e}"))? {
                f.write_all(&c).map_err(|e| e.to_string())?;
                got += c.len() as u64;
                if check { GOT_MB.store((got >> 20) as usize, Relaxed); }
            }
            drop(f);
            if total > 0 && got != total {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("DLサイズ不一致({got}/{total})"));
            }
            std::fs::rename(&tmp, &p).map_err(|e| e.to_string())?;
        }
        println!("🔎 言葉で探すモデル取得完了({}MB)", MODEL_BYTES >> 20);
        Ok(())
    }.await;
    DOWNLOADING.store(false, Relaxed);
    r
}

fn sess(root: &Path) -> Option<&'static Mutex<ort::session::Session>> {
    if SESS.get().is_none() && !model_path(root).exists() { return None; }
    let p = model_path(root);
    SESS.get_or_init(move || {
        // int8 は CUDA で動かない(ConvInteger 未実装)し、fp16 にしても速度は横ばいだった。
        // GPU に載せる価値が無いので CPU 固定にしている(docs/model-placement-design.md)
        let built = crate::ep::build(&p, 4, false, "grounding-dino");
        match built {
            Ok(s) => { println!("🔎 grounding-dino 読込OK"); Some(Mutex::new(s)) }
            Err(e) => { println!("⚠ grounding-dino 読込失敗({e})"); None }
        }
    }).as_ref()
}

fn tokenizer(root: &Path) -> Option<&'static tokenizers::Tokenizer> {
    if TOK.get().is_none() && !tok_path(root).exists() { return None; }
    let p = tok_path(root);
    TOK.get_or_init(move || tokenizers::Tokenizer::from_file(&p).ok()).as_ref()
}

/// クラス名を語句化: 小文字・`_`/`-`→空白・英数字以外を落として空白を畳む。
/// 返り句の "tray." や "##tray" の記号もこれで落ちる
pub fn norm(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() { out.push(c); }
        else { if !out.ends_with(' ') { out.push(' '); } }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 「dog. person.」形式のクラス列を分解。カンマ・改行・ピリオドのどれでも切る
/// (ここで切らないとクラス列全体が1ラベルに潰れて、全部が先頭クラスに化ける)
pub fn parse_labels(prompt: &str) -> Vec<String> {
    prompt.replace('\n', ",").replace('.', ",").split(',')
        .map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// 返り句(部分句かもしれない)を必ず元のクラス名へ引き戻す。
/// 完全一致 → 返り句のトークンが全部クラス語に含まれる(containment)を最優先 → 重なり率
fn match_label(returned: &str, phrases: &[(String, String)]) -> String {
    let r = norm(returned);
    let fallback = || phrases.first().map(|(o, _)| o.clone()).unwrap_or_else(|| "object".into());
    if r.is_empty() { return fallback(); }
    for (orig, ph) in phrases { if &r == ph { return orig.clone(); } }
    let rtok: Vec<&str> = r.split(' ').collect();
    let (mut best, mut best_score) = (None, -1.0f32);
    for (orig, ph) in phrases {
        let ptok: Vec<&str> = ph.split(' ').collect();
        let inter = rtok.iter().filter(|t| ptok.contains(t)).count();
        if ptok.is_empty() || inter == 0 { continue; }
        let union = ptok.len() + rtok.iter().filter(|t| !ptok.contains(t)).count();
        let contain = if rtok.iter().all(|t| ptok.contains(t)) { 1.0 } else { 0.0 };
        let score = contain * 2.0 + inter as f32 / union as f32 + 0.01 * ptok.len() as f32;
        if score > best_score { best = Some(orig.clone()); best_score = score; }
    }
    best.unwrap_or_else(fallback)
}

pub struct Det { pub xyxy: [f32; 4], pub conf: f32, pub cls: String } // xyxy は 0-1 正規化

fn iou(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let (ix1, iy1) = (a[0].max(b[0]), a[1].max(b[1]));
    let (ix2, iy2) = (a[2].min(b[2]), a[3].min(b[3]));
    let inter = (ix2 - ix1).max(0.0) * (iy2 - iy1).max(0.0);
    if inter <= 0.0 { return 0.0; }
    let aa = (a[2] - a[0]).max(0.0) * (a[3] - a[1]).max(0.0);
    let ba = (b[2] - b[0]).max(0.0) * (b[3] - b[1]).max(0.0);
    let den = aa + ba - inter;
    if den > 0.0 { inter / den } else { 0.0 }
}

/// 同じクラス内の重なりだけ落とす(クラスをまたぐ重なりは別物のことがある)
fn nms(mut dets: Vec<Det>) -> Vec<Det> {
    dets.sort_by(|a, b| b.conf.partial_cmp(&a.conf).unwrap());
    let mut kept: Vec<Det> = vec![];
    for d in dets {
        if kept.iter().any(|k| k.cls == d.cls && iou(&k.xyxy, &d.xyxy) >= NMS_IOU) { continue; }
        kept.push(d);
    }
    kept
}

/// 画像＋クラス語 → 箱。クラス語が多ければ 24 語ずつに分けて何度も検出する
pub fn detect(root: &Path, img: &image::DynamicImage, labels: &[String], thr: f32) -> Option<Vec<Det>> {
    let s = sess(root)?;
    let tk = tokenizer(root)?;
    let phrases: Vec<(String, String)> = labels.iter().map(|l| (l.clone(), norm(l)))
        .filter(|(_, p)| !p.is_empty()).collect();
    if phrases.is_empty() { return Some(vec![]); }

    // 画像側は 1 回だけ作る
    let r = img.resize_exact(SIDE as u32, SIDE as u32, image::imageops::FilterType::Triangle).to_rgb8();
    let mut px = vec![0f32; 3 * SIDE * SIDE];
    for (i, p) in r.pixels().enumerate() {
        for c in 0..3 { px[c * SIDE * SIDE + i] = (p[c] as f32 / 255.0 - MEAN[c]) / STD[c]; }
    }
    let mask = vec![1i64; SIDE * SIDE];

    let mut dets: Vec<Det> = vec![];
    for chunk in phrases.chunks(CHUNK) {
        // GDINO の作法: 小文字の句をピリオドで終端して並べる
        let text: String = chunk.iter().map(|(_, p)| format!("{p}. ")).collect();
        let Ok(enc) = tk.encode(text.trim(), true) else { continue };
        let ids: Vec<i64> = enc.get_ids().iter().map(|v| *v as i64).collect();
        let att: Vec<i64> = enc.get_attention_mask().iter().map(|v| *v as i64).collect();
        let n = ids.len();
        if n == 0 || n > 256 { continue; }
        let toks: Vec<String> = enc.get_tokens().to_vec();

        // 出力は借用なので、必要な数字を写してからロックを離す
        let ran = {
        let mut sess_g = s.lock().unwrap();
        let out = match sess_g.run(ort::inputs![
            "pixel_values" => ort::value::Tensor::from_array(([1usize, 3, SIDE, SIDE], px.clone())).ok()?,
            "input_ids" => ort::value::Tensor::from_array(([1usize, n], ids)).ok()?,
            "token_type_ids" => ort::value::Tensor::from_array(([1usize, n], vec![0i64; n])).ok()?,
            "attention_mask" => ort::value::Tensor::from_array(([1usize, n], att)).ok()?,
            "pixel_mask" => ort::value::Tensor::from_array(([1usize, SIDE, SIDE], mask.clone())).ok()?,
        ]) { Ok(o) => o, Err(_) => continue };
        let Some((ls, logits)) = out.get("logits").and_then(|v| v.try_extract_tensor::<f32>().ok()) else { continue };
        let Some((_, boxes)) = out.get("pred_boxes").and_then(|v| v.try_extract_tensor::<f32>().ok()) else { continue };
        (ls[1] as usize, ls[2] as usize, logits.to_vec(), boxes.to_vec()) // [1, 900, 256]
        };
        let (nq, nt, logits, boxes) = ran;

        for q in 0..nq {
            let row = &logits[q * nt..(q + 1) * nt];
            // ロジット→確率。一番強いトークンの値がその候補のスコア
            let probs: Vec<f32> = row.iter().map(|v| 1.0 / (1.0 + (-v).exp())).collect();
            let score = probs.iter().cloned().fold(0.0f32, f32::max);
            if score < thr { continue; }
            // しきい値を超えたトークンを繋いで返り句にする([CLS]等と記号は捨てる)
            let phrase: String = (0..nt.min(toks.len()))
                .filter(|i| probs[*i] > TEXT_THR)
                .map(|i| toks[i].trim_start_matches("##").to_string())
                .filter(|t| !t.starts_with('['))
                .collect::<Vec<_>>().join(" ");
            let cls = match_label(&phrase, chunk);
            let b = &boxes[q * 4..q * 4 + 4]; // cx,cy,w,h(0-1)
            let (cx, cy, w, h) = (b[0], b[1], b[2], b[3]);
            dets.push(Det {
                xyxy: [(cx - w / 2.0).clamp(0.0, 1.0), (cy - h / 2.0).clamp(0.0, 1.0),
                       (cx + w / 2.0).clamp(0.0, 1.0), (cy + h / 2.0).clamp(0.0, 1.0)],
                conf: score, cls,
            });
        }
    }
    Some(nms(dets))
}

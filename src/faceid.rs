//! 顔ID — 本人判定を「数学」に置き換える(docs/face-id-design.md)。
//! SCRFD(det_10g.onnx)で顔検出+5点ランドマーク → 相似変換で112x112へアライン →
//! ArcFace(w600k_r50.onnx)で512次元埋め込み(L2正規化) → cosineで本人判定。
//! モデルはDeep-Live-Cam導入時の ~/.insightface/models/buffalo_l/ を再利用(DL不要)。

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

static DET: OnceLock<Option<Mutex<ort::session::Session>>> = OnceLock::new();
static REC: OnceLock<Option<Mutex<ort::session::Session>>> = OnceLock::new();

/// 本人確定のしきい値(ArcFace cosine)。実測で調整する舵
pub const FACE_SAME: f32 = 0.42;
/// 別人確定のしきい値(登録全員に対してこれ未満なら不一致)
pub const FACE_DIFF: f32 = 0.28;

const DET_SIZE: usize = 640;

fn models_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| "/root".into())
        .join(".insightface/models/buffalo_l")
}

fn session(cell: &'static OnceLock<Option<Mutex<ort::session::Session>>>, file: &str)
           -> Option<&'static Mutex<ort::session::Session>> {
    let file = file.to_string();
    cell.get_or_init(move || {
        let p = models_dir().join(&file);
        let built = (|| -> Result<ort::session::Session, String> {
            let b = ort::session::Session::builder().map_err(|e| e.to_string())?;
            let mut b = b.with_intra_threads(2).map_err(|e| e.to_string())?;
            b.commit_from_file(&p).map_err(|e| e.to_string())
        })();
        match built {
            Ok(s) => {
                println!("🧭 {file} 読込OK(顔ID)");
                Some(Mutex::new(s))
            }
            Err(e) => {
                println!("⚠ {file} 読込失敗({e}) — 顔IDは無効");
                None
            }
        }
    })
    .as_ref()
}

#[derive(Clone, Debug)]
pub struct Face {
    pub bbox: [f32; 4],     // x1,y1,x2,y2 (元画像座標)
    pub kps: [[f32; 2]; 5], // 両目・鼻・口角(元画像座標)
    pub score: f32,
}

/// SCRFD検出。入力640x640レターボックス、stride 8/16/32のanchor-free出力をデコード。
pub fn detect_faces(img: &image::DynamicImage) -> Vec<Face> {
    let Some(s) = session(&DET, "det_10g.onnx") else { return vec![] };
    let (w0, h0) = (img.width() as f32, img.height() as f32);
    let scale = (DET_SIZE as f32 / w0).min(DET_SIZE as f32 / h0);
    let (nw, nh) = ((w0 * scale) as u32, (h0 * scale) as u32);
    let resized = img.resize_exact(nw.max(1), nh.max(1), image::imageops::FilterType::Triangle).into_rgb8();
    // レターボックス(左上詰め・余白は黒=SCRFDの流儀)
    let mut data = vec![0f32; 3 * DET_SIZE * DET_SIZE];
    for (x, y, p) in resized.enumerate_pixels() {
        for c in 0..3 {
            data[c * DET_SIZE * DET_SIZE + (y as usize) * DET_SIZE + x as usize] =
                (p.0[c] as f32 - 127.5) / 128.0;
        }
    }
    let Ok(tensor) = ort::value::Tensor::from_array(([1usize, 3, DET_SIZE, DET_SIZE], data)) else { return vec![] };
    let mut sess = s.lock().unwrap();
    let iname = sess.inputs()[0].name().to_string(); // モデル毎に入力名が違うので動的取得
    let Ok(outputs) = sess.run(ort::inputs![iname.as_str() => tensor]) else { return vec![] };

    // 出力9本: [score8, score16, score32, bbox8, bbox16, bbox32, kps8, kps16, kps32]
    let mut cands: Vec<Face> = vec![];
    for (si, stride) in [8usize, 16, 32].iter().enumerate() {
        let (Ok((_, sc)), Ok((_, bb)), Ok((_, kp))) = (
            outputs[si].try_extract_tensor::<f32>(),
            outputs[si + 3].try_extract_tensor::<f32>(),
            outputs[si + 6].try_extract_tensor::<f32>(),
        ) else { continue };
        let fm = DET_SIZE / stride; // feature map一辺
        let num_anchors = 2usize;
        for idx in 0..(fm * fm * num_anchors) {
            let score = sc[idx];
            if score < 0.5 {
                continue;
            }
            let cell = idx / num_anchors;
            let (cx, cy) = ((cell % fm) as f32 * *stride as f32, (cell / fm) as f32 * *stride as f32);
            // bbox=中心からの距離(l,t,r,b)×stride
            let (l, t, r, b) = (bb[idx * 4], bb[idx * 4 + 1], bb[idx * 4 + 2], bb[idx * 4 + 3]);
            let bbox = [
                (cx - l * *stride as f32) / scale,
                (cy - t * *stride as f32) / scale,
                (cx + r * *stride as f32) / scale,
                (cy + b * *stride as f32) / scale,
            ];
            let mut kps = [[0f32; 2]; 5];
            for k in 0..5 {
                kps[k] = [
                    (cx + kp[idx * 10 + k * 2] * *stride as f32) / scale,
                    (cy + kp[idx * 10 + k * 2 + 1] * *stride as f32) / scale,
                ];
            }
            cands.push(Face { bbox, kps, score });
        }
    }
    // NMS(IoU 0.4)
    cands.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<Face> = vec![];
    'next: for c in cands {
        for o in &out {
            if iou(&c.bbox, &o.bbox) > 0.4 {
                continue 'next;
            }
        }
        out.push(c);
        if out.len() >= 16 {
            break;
        }
    }
    out
}

fn iou(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let (x1, y1) = (a[0].max(b[0]), a[1].max(b[1]));
    let (x2, y2) = (a[2].min(b[2]), a[3].min(b[3]));
    let inter = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    let ua = (a[2] - a[0]).max(0.0) * (a[3] - a[1]).max(0.0)
        + (b[2] - b[0]).max(0.0) * (b[3] - b[1]).max(0.0) - inter;
    if ua <= 0.0 { 0.0 } else { inter / ua }
}

/// ArcFace標準の112x112正準5点
const ARC_DST: [[f32; 2]; 5] = [
    [38.2946, 51.6963], [73.5318, 51.5014], [56.0252, 71.7366],
    [41.5493, 92.3655], [70.7299, 92.2041],
];

/// 5点対応から相似変換(回転+等方スケール+平行移動)を最小二乗で解く。
/// 点を複素数と見なすと z→a*z+b の線形最小二乗になる(aが回転スケール、bが平行移動)。
fn similarity_from_kps(src: &[[f32; 2]; 5]) -> (f32, f32, f32, f32) {
    // Σ|a*z+b-w|² 最小化: 正規方程式
    let n = 5.0f64;
    let (mut sz_re, mut sz_im, mut sw_re, mut sw_im) = (0f64, 0f64, 0f64, 0f64);
    let (mut szz, mut szw_re, mut szw_im) = (0f64, 0f64, 0f64);
    for k in 0..5 {
        let (zr, zi) = (src[k][0] as f64, src[k][1] as f64);
        let (wr, wi) = (ARC_DST[k][0] as f64, ARC_DST[k][1] as f64);
        sz_re += zr; sz_im += zi; sw_re += wr; sw_im += wi;
        szz += zr * zr + zi * zi;
        // conj(z)*w
        szw_re += zr * wr + zi * wi;
        szw_im += zr * wi - zi * wr;
    }
    let denom = szz - (sz_re * sz_re + sz_im * sz_im) / n;
    let a_re = (szw_re - (sz_re * sw_re + sz_im * sw_im) / n) / denom;
    let a_im = (szw_im - (sz_re * sw_im - sz_im * sw_re) / n) / denom;
    let b_re = (sw_re - (a_re * sz_re - a_im * sz_im)) / n;
    let b_im = (sw_im - (a_re * sz_im + a_im * sz_re)) / n;
    (a_re as f32, a_im as f32, b_re as f32, b_im as f32)
}

/// 顔を112x112へアライン(逆変換でbilinearサンプル)して埋め込み(512次元・L2正規化)
pub fn embed_face(img: &image::DynamicImage, kps: &[[f32; 2]; 5]) -> Option<Vec<f32>> {
    let s = session(&REC, "w600k_r50.onnx")?;
    let (ar, ai, br, bi) = similarity_from_kps(kps);
    // 逆変換: z = (w - b) / a
    let det = ar * ar + ai * ai;
    if det < 1e-9 {
        return None;
    }
    let rgb = img.to_rgb8();
    let (iw, ih) = (rgb.width() as i64, rgb.height() as i64);
    let mut data = vec![0f32; 3 * 112 * 112];
    for dy in 0..112u32 {
        for dx in 0..112u32 {
            let (wr, wi) = (dx as f32, dy as f32);
            let (tr, ti) = (wr - br, wi - bi);
            let sx = (tr * ar + ti * ai) / det;
            let sy = (ti * ar - tr * ai) / det;
            // bilinear
            let (x0, y0) = (sx.floor() as i64, sy.floor() as i64);
            let (fx, fy) = (sx - x0 as f32, sy - y0 as f32);
            let mut px = [0f32; 3];
            for (ox, oy, w) in [
                (0i64, 0i64, (1.0 - fx) * (1.0 - fy)),
                (1, 0, fx * (1.0 - fy)),
                (0, 1, (1.0 - fx) * fy),
                (1, 1, fx * fy),
            ] {
                let (xx, yy) = ((x0 + ox).clamp(0, iw - 1), (y0 + oy).clamp(0, ih - 1));
                let p = rgb.get_pixel(xx as u32, yy as u32);
                for c in 0..3 {
                    px[c] += w * p.0[c] as f32;
                }
            }
            for c in 0..3 {
                data[c * 112 * 112 + dy as usize * 112 + dx as usize] = (px[c] - 127.5) / 127.5;
            }
        }
    }
    let tensor = ort::value::Tensor::from_array(([1usize, 3, 112, 112], data)).ok()?;
    let mut sess = s.lock().unwrap();
    let iname = sess.inputs()[0].name().to_string();
    let outputs = sess.run(ort::inputs![iname.as_str() => tensor]).ok()?;
    let (_, raw) = outputs[0].try_extract_tensor::<f32>().ok()?;
    let mut v: Vec<f32> = raw.to_vec();
    v.truncate(512);
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
    v.iter_mut().for_each(|x| *x /= n);
    Some(v)
}

/// 参照群との最大cosine(正規化済み=dot)
pub fn best_sim(emb: &[f32], refs: &[Vec<f32>]) -> f32 {
    refs.iter()
        .map(|r| r.iter().zip(emb).map(|(a, b)| a * b).sum::<f32>())
        .fold(-1.0f32, f32::max)
}

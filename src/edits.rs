//! 非破壊調整 — 原本(sha1ファイル)は不変、エフェクトはサイドカーの edits 履歴スタック。
//! 表示は store/renders/ のキャッシュ(全て再生成可能)、データセット書き出し時に焼き込み。
//! フィルタ名は fluent_scene の FS_* に合わせる(grayscale/sepia/invert/posterize/vignette/sharpen/blur)。

use image::DynamicImage;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};

use crate::store;

/// 履歴のリビジョン(=レンダキャッシュのキー)。edits配列のJSONをハッシュ。
pub fn rev(edits: &Value) -> String {
    let s = serde_json::to_string(edits).unwrap_or_default();
    hex::encode(Sha1::digest(s.as_bytes()))[..12].to_string()
}

pub fn render_path(root: &Path, sha1: &str, rev: &str, w: u32, seg: bool) -> PathBuf {
    let s = if seg { ".seg" } else { "" };
    root.join("store/renders").join(store::shard(sha1)).join(format!("{sha1}.{rev}.w{w}{s}.jpg"))
}

/// この画像のレンダキャッシュを全部捨てる(履歴が変わった時)。
pub fn clear_renders(root: &Path, sha1: &str) {
    let dir = root.join("store/renders").join(store::shard(sha1));
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with(sha1) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

fn clamp01(v: f32) -> f32 { v.clamp(0.0, 1.0) }

/// ピクセル一括変換(RGB f32 0..1)
fn map_px(img: DynamicImage, f: impl Fn(f32, f32, f32, f32, f32) -> [f32; 3] + Sync) -> DynamicImage {
    let mut rgb = img.into_rgb8();
    let (w, h) = (rgb.width() as f32, rgb.height() as f32);
    for (x, y, p) in rgb.enumerate_pixels_mut() {
        let [r, g, b] = p.0.map(|c| c as f32 / 255.0);
        let out = f(r, g, b, x as f32 / w, y as f32 / h);
        p.0 = out.map(|c| (clamp01(c) * 255.0).round() as u8);
    }
    DynamicImage::ImageRgb8(rgb)
}

fn adjust(img: DynamicImage, pr: &Value) -> DynamicImage {
    let g = |k: &str| pr[k].as_f64().unwrap_or(0.0) as f32;
    let (ex, temp, con, sat) = (g("exposure"), g("temperature"), g("contrast"), g("saturation"));
    if [ex, temp, con, sat].iter().all(|v| v.abs() < 1e-4) {
        return img;
    }
    let gain = 2f32.powf(ex * 2.0); // ±2EV
    map_px(img, move |r, g, b, _, _| {
        let (mut r, mut g, mut b) = (r * gain, g * gain, b * gain);
        r *= 1.0 + 0.3 * temp;
        b *= 1.0 - 0.3 * temp;
        let c = 1.0 + con;
        r = (r - 0.5) * c + 0.5;
        g = (g - 0.5) * c + 0.5;
        b = (b - 0.5) * c + 0.5;
        let luma = 0.299 * r + 0.587 * g + 0.114 * b;
        let s = 1.0 + sat;
        [luma + (r - luma) * s, luma + (g - luma) * s, luma + (b - luma) * s]
    })
}

fn filter(img: DynamicImage, pr: &Value) -> DynamicImage {
    let name = pr["name"].as_str().unwrap_or("");
    let amt = pr["amount"].as_f64().unwrap_or(1.0) as f32;
    match name {
        "grayscale" => map_px(img, |r, g, b, _, _| {
            let l = 0.299 * r + 0.587 * g + 0.114 * b;
            [l, l, l]
        }),
        "sepia" => map_px(img, |r, g, b, _, _| {
            [0.393 * r + 0.769 * g + 0.189 * b,
             0.349 * r + 0.686 * g + 0.168 * b,
             0.272 * r + 0.534 * g + 0.131 * b]
        }),
        "invert" => map_px(img, |r, g, b, _, _| [1.0 - r, 1.0 - g, 1.0 - b]),
        "posterize" => {
            let levels = pr["levels"].as_f64().unwrap_or(5.0).max(2.0) as f32;
            map_px(img, move |r, g, b, _, _| {
                let q = |v: f32| ((v * (levels - 1.0)).round()) / (levels - 1.0);
                [q(r), q(g), q(b)]
            })
        }
        "vignette" => {
            let k = 0.85 * amt;
            map_px(img, move |r, g, b, x, y| {
                let (dx, dy) = (x - 0.5, y - 0.5);
                let d = (dx * dx + dy * dy).sqrt() / std::f32::consts::FRAC_1_SQRT_2;
                let f = 1.0 - k * (d * d);
                [r * f, g * f, b * f]
            })
        }
        "sharpen" => DynamicImage::ImageRgb8(image::imageops::unsharpen(&img.into_rgb8(), 1.2, (12.0 * amt) as i32)),
        "blur" => DynamicImage::ImageRgb8(image::imageops::blur(&img.into_rgb8(), (3.0 * amt).max(0.3))),
        _ => img,
    }
}

/// ✨自動補正 — グレーワールドWB + ヒストグラム自動レベル(0.5%クリップ) + 微彩度。
/// 内容依存だが決定的(同じ画像→同じ結果)なので履歴opとして安全。
fn auto_enhance(img: DynamicImage) -> DynamicImage {
    let rgb = img.to_rgb8();
    let (w, h) = (rgb.width(), rgb.height());
    let step = ((w as u64 * h as u64 / 100_000).max(1)) as usize; // 最大10万px標本
    let (mut sr, mut sg, mut sb, mut n) = (0u64, 0u64, 0u64, 0u64);
    let mut hist = [0u32; 256];
    for (i, p) in rgb.pixels().enumerate() {
        if i % step != 0 {
            continue;
        }
        let [r, g, b] = p.0;
        sr += r as u64;
        sg += g as u64;
        sb += b as u64;
        let luma = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) as usize;
        hist[luma.min(255)] += 1;
        n += 1;
    }
    if n == 0 {
        return img;
    }
    // WB: グレーワールドを「半分だけ」効かせる — 全開だと夕日・電球の意図した暖かさまで
    // 中和して寒色に振る(「色温度が逆」事件)。小さいキャストは触らない
    let (ar, ag, ab) = (sr as f32 / n as f32, sg as f32 / n as f32, sb as f32 / n as f32);
    let soften = |g: f32| (1.0 + (g - 1.0) * 0.45).clamp(0.85, 1.2);
    let (gr_raw, gb_raw) = (ag / ar.max(1.0), ag / ab.max(1.0));
    let (gr, gb) = if (gr_raw - 1.0).abs() < 0.04 && (gb_raw - 1.0).abs() < 0.04 {
        (1.0, 1.0)
    } else {
        (soften(gr_raw), soften(gb_raw))
    };
    // 自動レベル: 0.3%クリップ・70%がけ(全開ストレッチは白飛び/黒潰れを作る)
    let clip = (n as u32) / 300;
    let (mut lo, mut hi, mut acc) = (0usize, 255usize, 0u32);
    for (i, c) in hist.iter().enumerate() {
        acc += c;
        if acc > clip {
            lo = i;
            break;
        }
    }
    acc = 0;
    for (i, c) in hist.iter().enumerate().rev() {
        acc += c;
        if acc > clip {
            hi = i;
            break;
        }
    }
    let (lo, hi) = (lo as f32 / 255.0, (hi.max(lo + 8)) as f32 / 255.0);
    let span = hi - lo;
    let k = 0.7; // レベル補正の効かせ具合
    map_px(img, move |r, g, b, _, _| {
        let (r0, g0, b0) = (r * gr, g, b * gb);
        let st = |v: f32| v + ((v - lo) / span - v) * k;
        let (r, g, b) = (st(r0), st(g0), st(b0));
        let luma = 0.299 * r + 0.587 * g + 0.114 * b;
        let s = 1.05; // ほんの少しだけ彩度
        [luma + (r - luma) * s, luma + (g - luma) * s, luma + (b - luma) * s]
    })
}

/// 履歴を順に適用(op: adjust / crop / rotate / flip / filter / auto)。
pub fn apply(mut img: DynamicImage, edits: &Value) -> DynamicImage {
    let Some(list) = edits.as_array() else { return img };
    for e in list {
        let pr = &e["params"];
        img = match e["op"].as_str().unwrap_or("") {
            "adjust" => adjust(img, pr),
            "auto" => auto_enhance(img),
            "filter" => filter(img, pr),
            "rotate" => match pr["deg"].as_i64().unwrap_or(0).rem_euclid(360) {
                90 => img.rotate90(),
                180 => img.rotate180(),
                270 => img.rotate270(),
                _ => img,
            },
            "flip" => match pr["dir"].as_str().unwrap_or("h") {
                "v" => img.flipv(),
                _ => img.fliph(),
            },
            "crop" => {
                // 比率指定(fx/fy/fw/fh 0..1)優先 — 回転等の後でも正しく効く。ピクセル指定(x/y/w/h)も後方互換
                let (iw, ih) = (img.width(), img.height());
                let f = |k: &str| pr[k].as_f64().map(|v| v.clamp(0.0, 1.0) as f32);
                let (x, y, w, h) = if let (Some(fx), Some(fy), Some(fw), Some(fh)) =
                    (f("fx"), f("fy"), f("fw"), f("fh"))
                {
                    ((fx * iw as f32) as u32, (fy * ih as f32) as u32,
                     (fw * iw as f32) as u32, (fh * ih as f32) as u32)
                } else {
                    let g = |k: &str| pr[k].as_u64().unwrap_or(0) as u32;
                    (g("x"), g("y"), g("w"), g("h"))
                };
                let (x, y) = (x.min(iw - 1), y.min(ih - 1));
                let w = w.clamp(1, iw - x);
                let h = h.clamp(1, ih - y);
                img.crop_imm(x, y, w, h)
            }
            _ => img,
        };
    }
    img
}

/// マスク座標(正規化・原本基準)を編集履歴(crop/rotate/flip)に追従させる。
/// これをやらないと「クロップしたらマスクがズレる」(2026-09-03バグ)
fn transform_shapes(shapes: &Value, edits: &Value) -> Value {
    let mut out: Vec<Value> = vec![];
    for s in shapes.as_array().map(|v| v.as_slice()).unwrap_or(&[]) {
        let Some(pts) = s["points"].as_array() else { continue };
        let mut xy: Vec<f32> = pts.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect();
        for e in edits.as_array().map(|v| v.as_slice()).unwrap_or(&[]) {
            let pr = &e["params"];
            match e["op"].as_str().unwrap_or("") {
                "rotate" => {
                    let deg = pr["deg"].as_i64().unwrap_or(0).rem_euclid(360);
                    for i in (0..xy.len()).step_by(2) {
                        let (x, y) = (xy[i], xy[i + 1]);
                        let (nx, ny) = match deg {
                            90 => (1.0 - y, x),  // image::rotate90(時計回り)
                            180 => (1.0 - x, 1.0 - y),
                            270 => (y, 1.0 - x),
                            _ => (x, y),
                        };
                        xy[i] = nx;
                        xy[i + 1] = ny;
                    }
                }
                "flip" => {
                    let v = pr["dir"].as_str().unwrap_or("h") == "v";
                    for i in (0..xy.len()).step_by(2) {
                        if v { xy[i + 1] = 1.0 - xy[i + 1]; } else { xy[i] = 1.0 - xy[i]; }
                    }
                }
                "crop" => {
                    let f = |k: &str| pr[k].as_f64().map(|v| v as f32);
                    if let (Some(fx), Some(fy), Some(fw), Some(fh)) = (f("fx"), f("fy"), f("fw"), f("fh")) {
                        let (fw, fh) = (fw.max(0.001), fh.max(0.001));
                        for i in (0..xy.len()).step_by(2) {
                            xy[i] = (xy[i] - fx) / fw;
                            xy[i + 1] = (xy[i + 1] - fy) / fh;
                        }
                    }
                }
                _ => {} // adjust/filter/auto は幾何に影響しない
            }
        }
        out.push(json!({"cls": s["cls"], "conf": s["conf"], "points": xy}));
    }
    json!(out)
}

/// マスクのαチャネル(0-255)を作る: 多角形を走査線で塗り→ボックスぼかし2回でフェザー(抜け際が自然)
fn mask_alpha(w: usize, h: usize, shapes: &Value) -> Vec<u8> {
    let mut a = vec![0u8; w * h];
    for s in shapes.as_array().map(|v| v.as_slice()).unwrap_or(&[]) {
        let Some(pts) = s["points"].as_array() else { continue };
        let xy: Vec<f32> = pts.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect();
        if xy.len() < 6 {
            continue;
        }
        let n = xy.len() / 2;
        for y in 0..h {
            let fy = y as f32 + 0.5;
            let mut xs: Vec<f32> = vec![];
            for i in 0..n {
                let (x0, y0) = (xy[2 * i] * w as f32, xy[2 * i + 1] * h as f32);
                let j = (i + 1) % n;
                let (x1, y1) = (xy[2 * j] * w as f32, xy[2 * j + 1] * h as f32);
                if (y0 <= fy) != (y1 <= fy) {
                    xs.push(x0 + (fy - y0) * (x1 - x0) / (y1 - y0));
                }
            }
            xs.sort_by(|p, q| p.partial_cmp(q).unwrap());
            for pair in xs.chunks(2) {
                if let [s0, s1] = pair {
                    let (b0, b1) = ((s0.max(0.0) as usize).min(w), (s1.max(0.0) as usize).min(w));
                    for x in b0..b1 {
                        a[y * w + x] = 255;
                    }
                }
            }
        }
    }
    // フェザー: 半径3のボックスぼかし×2(≒ガウス)。境界線を「引かない」のが綺麗さの正体
    let blur = |src: &[u8], w: usize, h: usize, horizontal: bool| -> Vec<u8> {
        let r = 3i32;
        let mut out = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                let (mut sum, mut cnt) = (0u32, 0u32);
                for d in -r..=r {
                    let (sx, sy) = if horizontal { (x as i32 + d, y as i32) } else { (x as i32, y as i32 + d) };
                    if sx >= 0 && sy >= 0 && (sx as usize) < w && (sy as usize) < h {
                        sum += src[sy as usize * w + sx as usize] as u32;
                        cnt += 1;
                    }
                }
                out[y * w + x] = (sum / cnt.max(1)) as u8;
            }
        }
        out
    };
    let a = blur(&a, w, h, true);
    let a = blur(&a, w, h, false);
    let a = blur(&a, w, h, true);
    blur(&a, w, h, false)
}

/// マスク表示=背景を沈める(αブレンド・境界線なし)。被写体だけがふわっと浮かぶ
fn draw_seg(img: &mut image::RgbImage, shapes: &Value) {
    let (w, h) = (img.width() as usize, img.height() as usize);
    if w == 0 || h == 0 {
        return;
    }
    let alpha = mask_alpha(w, h, shapes);
    let bg = [10.0f32, 10.0, 13.0]; // アプリ背景色に沈める
    for y in 0..h {
        for x in 0..w {
            let a = alpha[y * w + x] as f32 / 255.0;
            if a >= 0.995 {
                continue;
            }
            let p = img.get_pixel_mut(x as u32, y as u32);
            for c in 0..3 {
                p.0[c] = (p.0[c] as f32 * a + bg[c] * (1.0 - a)) as u8;
            }
        }
    }
}

/// 切り抜きPNG(透過・フェザー付き) — 「背景なかったことにする」本体。編集履歴も適用済み
pub fn cutout_png(root: &Path, sha1: &str, ext: &str, edits: &Value, shapes: &Value, w_limit: u32) -> Option<Vec<u8>> {
    let mut img = image::open(store::image_path(root, sha1, ext)).ok()?;
    img = apply(img, edits);
    if w_limit > 0 && (img.width() > w_limit || img.height() > w_limit) {
        img = img.thumbnail(w_limit, w_limit);
    }
    let rgb = img.into_rgb8();
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    let ts = transform_shapes(shapes, edits);
    let alpha = mask_alpha(w, h, &ts);
    let mut rgba = image::RgbaImage::new(w as u32, h as u32);
    for y in 0..h {
        for x in 0..w {
            let p = rgb.get_pixel(x as u32, y as u32);
            rgba.put_pixel(x as u32, y as u32, image::Rgba([p.0[0], p.0[1], p.0[2], alpha[y * w + x]]));
        }
    }
    let mut buf = std::io::Cursor::new(Vec::new());
    image::codecs::png::PngEncoder::new(&mut buf);
    image::DynamicImage::ImageRgba8(rgba).write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some(buf.into_inner())
}

/// 履歴適用済みJPEGを返す(w>0なら長辺wへ縮小、seg=trueでマスク輪郭を焼く)。キャッシュ命中なら即返し。
pub fn render(root: &Path, sha1: &str, ext: &str, edits: &Value, w: u32, seg: Option<&Value>) -> Option<Vec<u8>> {
    let rp = render_path(root, sha1, &rev(edits), w, seg.is_some());
    if let Ok(b) = std::fs::read(&rp) {
        return Some(b);
    }
    let mut img = image::open(store::image_path(root, sha1, ext)).ok()?;
    img = apply(img, edits);
    if w > 0 && (img.width() > w || img.height() > w) {
        img = img.thumbnail(w, w);
    }
    let mut rgb = img.into_rgb8();
    if let Some(shapes) = seg {
        let ts = transform_shapes(shapes, edits); // 編集に追従したマスク座標で描く
        draw_seg(&mut rgb, &ts);
    }
    let mut buf = std::io::Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 90)
        .encode_image(&rgb)
        .ok()?;
    std::fs::create_dir_all(rp.parent()?).ok()?;
    let _ = std::fs::write(&rp, buf.get_ref());
    Some(buf.into_inner())
}

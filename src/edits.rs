//! 非破壊調整 — 原本(sha1ファイル)は不変、エフェクトはサイドカーの edits 履歴スタック。
//! 表示は store/renders/ のキャッシュ(全て再生成可能)、データセット書き出し時に焼き込み。
//! フィルタ名は fluent_scene の FS_* に合わせる(grayscale/sepia/invert/posterize/vignette/sharpen/blur/canny)。

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
        "canny" => canny(img, pr),
        _ => img,
    }
}

/// Canny: Gaussian → Sobel → 非極大抑制 → 二重閾値と8近傍ヒステリシス。
/// low/high は8bit輝度のSobel勾配強度(0..1020、既定50/100)。逆順なら入れ替える。
/// sigma はGaussianの標準偏差(0.3..5px、既定1.2)。黒背景に白い輪郭を返す。
fn canny(img: DynamicImage, pr: &Value) -> DynamicImage {
    let parameter = |name: &str, default: f64, min: f64, max: f64| {
        pr[name].as_f64().filter(|v| v.is_finite()).unwrap_or(default).clamp(min, max) as f32
    };
    let low = parameter("low", 50.0, 0.0, 1020.0);
    let high = parameter("high", 100.0, 0.0, 1020.0);
    let (low, high) = (low.min(high), low.max(high));
    let sigma = parameter("sigma", 1.2, 0.3, 5.0);
    let gray = img.into_luma8();
    let (width, height) = gray.dimensions();
    let (w, h) = (width as usize, height as usize);
    // 3×3の勾配を計算できない画像にも、同寸法の有効な輪郭画像を返す。
    if w < 3 || h < 3 {
        return DynamicImage::ImageLuma8(image::GrayImage::new(width, height));
    }
    let smooth = image::imageops::blur(&gray, sigma);
    let pixels = smooth.as_raw();
    let mut magnitude = vec![0.0f32; w * h];
    let mut direction = vec![0u8; w * h];
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let i = y * w + x;
            let sample = |j: usize| pixels[j] as f32;
            let gx = -sample(i - w - 1) + sample(i - w + 1)
                - 2.0 * sample(i - 1) + 2.0 * sample(i + 1)
                - sample(i + w - 1) + sample(i + w + 1);
            let gy = -sample(i - w - 1) - 2.0 * sample(i - w) - sample(i - w + 1)
                + sample(i + w - 1) + 2.0 * sample(i + w) + sample(i + w + 1);
            magnitude[i] = gx.hypot(gy);
            // 角度を0/45/90/135°へ量子化。atan2不要で高解像度画像にも対応する。
            direction[i] = if gy.abs() <= gx.abs() * 0.414_213_57 {
                0
            } else if gy.abs() >= gx.abs() * 2.414_213_7 {
                2
            } else if gx * gy > 0.0 {
                1
            } else {
                3
            };
        }
    }
    let mut thin = vec![0.0f32; w * h];
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let i = y * w + x;
            let (before, after) = match direction[i] {
                0 => (i - 1, i + 1),
                1 => (i - w - 1, i + w + 1),
                2 => (i - w, i + w),
                _ => (i - w + 1, i + w - 1),
            };
            // 一方を厳密比較にして、同強度の二重線を1pxに抑える。
            if magnitude[i] >= magnitude[before] && magnitude[i] > magnitude[after] {
                thin[i] = magnitude[i];
            }
        }
    }
    drop(magnitude);
    drop(direction);
    let edges = canny_hysteresis(&thin, w, h, low, high);
    DynamicImage::ImageLuma8(image::GrayImage::from_raw(width, height, edges).unwrap())
}

/// 強い輪郭につながる弱い輪郭だけを残す。0強度は閾値0でも輪郭にしない。
fn canny_hysteresis(magnitude: &[f32], w: usize, h: usize, low: f32, high: f32) -> Vec<u8> {
    let mut edges = vec![0u8; magnitude.len()];
    let mut pending = Vec::new();
    for (i, &strength) in magnitude.iter().enumerate() {
        if strength > 0.0 && strength >= high {
            edges[i] = 255;
            pending.push(i);
        } else if strength > 0.0 && strength >= low {
            edges[i] = 128;
        }
    }
    while let Some(i) = pending.pop() {
        let (x, y) = (i % w, i / w);
        for ny in y.saturating_sub(1)..=(y + 1).min(h - 1) {
            for nx in x.saturating_sub(1)..=(x + 1).min(w - 1) {
                let neighbor = ny * w + nx;
                if edges[neighbor] == 128 {
                    edges[neighbor] = 255;
                    pending.push(neighbor);
                }
            }
        }
    }
    for value in &mut edges {
        if *value != 255 {
            *value = 0;
        }
    }
    edges
}

/// 自動補正: 信頼できる低彩度部分から控えめにWBを推定し、露出を輝度で補正。
/// 黒点/白点の強制ストレッチはしない。低コントラスト・単色画像はそのまま保つ。
/// 同じ画像から必ず同じ結果を返し、透明部分は統計に含めずαも保持する。
fn auto_enhance_v2(img: DynamicImage) -> DynamicImage {
    let mut rgba = img.to_rgba8();
    let step = (rgba.as_raw().len() / 4).div_ceil(100_000).max(1);
    let mut hist = [0u32; 256];
    let mut neutral_sum = [[0.0f32; 3]; 8];
    let mut neutral_count = [0u32; 8];
    let mut n = 0u32;
    let luma = |rgb: [f32; 3]| 0.299 * rgb[0] + 0.587 * rgb[1] + 0.114 * rgb[2];
    for pixel in rgba.as_raw().chunks_exact(4).step_by(step) {
        // 透明背景の隠れたRGB値や、ごく薄いエッジは露出/WBの根拠にしない。
        if pixel[3] < 128 { continue; }
        let rgb = [pixel[0] as f32, pixel[1] as f32, pixel[2] as f32];
        let y = luma(rgb);
        hist[y.round().clamp(0.0, 255.0) as usize] += 1;
        n += 1;
        let min = rgb.into_iter().fold(f32::INFINITY, f32::min);
        let max = rgb.into_iter().fold(0.0f32, f32::max);
        // 飽和色・暗部ノイズ・白飛びはホワイトバランス推定から除外する。
        if y >= 40.0 && y <= 224.0 && min >= 24.0 && max <= 245.0
            && (max - min) / max <= 0.25
        {
            let bin = y as usize / 32;
            for c in 0..3 { neutral_sum[bin][c] += rgb[c] / y; }
            neutral_count[bin] += 1;
        }
    }
    if n < 16 { return img; }
    let percentile = |fraction: f32| {
        let rank = (n as f32 * fraction) as u32;
        let mut sum = 0;
        for (value, count) in hist.iter().enumerate() {
            sum += count;
            if sum > rank { return value as f32 / 255.0; }
        }
        1.0
    };
    let (lo, mid, hi) = (percentile(0.02), percentile(0.50), percentile(0.98));
    // 単色、霧、背景等の狭い階調を無理に全レンジへ引き伸ばさない。
    if hi - lo < 32.0 / 255.0 { return img; }

    let mut wb = [1.0f32; 3];
    let count: u32 = neutral_count.iter().sum();
    if count >= (n / 10).max(16) {
        let mut mean = [0.0f32; 3];
        for bin in &neutral_sum {
            for c in 0..3 { mean[c] += bin[c] / count as f32; }
        }
        // 明るさの違う少なくとも3群で色被りが一致したときだけ補正する。
        // 被写体全体の平均を灰色と仮定すると、空・植物・夕景の色を壊してしまう。
        let mut bins = 0;
        let mut consistent = true;
        for bin in 0..8 {
            if neutral_count[bin] < (n / 200).max(4) { continue; }
            bins += 1;
            for c in 0..3 {
                let ratio = neutral_sum[bin][c] / neutral_count[bin] as f32;
                consistent &= (ratio - mean[c]).abs() <= 0.04;
            }
        }
        if bins >= 3 && consistent && mean.iter().any(|v| (v - 1.0).abs() > 0.025) {
            for c in 0..3 { wb[c] = (1.0 + (1.0 / mean[c] - 1.0) * 0.6).clamp(0.9, 1.12); }
        }
    }

    // 正常な露出や、すでに黒から白まで使っている夜景/ハイキー画像には触らない。
    // 暗い画像は最大約0.85段、明るすぎる画像は最大約0.4段に抑える。
    let exposure = if mid < 0.36 && hi < 0.90 {
        (0.36 * (1.0 - mid) / (0.64 * mid.max(0.01))).clamp(1.0, 1.8)
    } else if mid > 0.72 && lo > 0.10 {
        (0.72 * (1.0 - mid) / (0.28 * mid)).clamp(0.75, 1.0)
    } else { 1.0 };
    if wb == [1.0; 3] && exposure == 1.0 { return img; }

    for pixel in rgba.pixels_mut() {
        if pixel[3] == 0 { continue; }
        let rgb = [pixel[0] as f32 / 255.0 * wb[0],
                   pixel[1] as f32 / 255.0 * wb[1],
                   pixel[2] as f32 / 255.0 * wb[2]];
        let y = luma(rgb).clamp(0.0, 1.0);
        // y' = gain*y / (1 + (gain-1)*y): 黒/白を固定した滑らかな単調曲線。
        // RGB共通倍率と共通の上限制御で、色相変化とチャンネル別の白飛びを防ぐ。
        let gain = exposure / (1.0 + (exposure - 1.0) * y);
        let max = rgb.into_iter().fold(0.0f32, f32::max);
        let gain = gain.min(1.0 / max.max(1e-6));
        for c in 0..3 { pixel[c] = (rgb[c] * gain * 255.0).round().clamp(0.0, 255.0) as u8; }
    }
    if img.color().has_alpha() {
        DynamicImage::ImageRgba8(rgba)
    } else {
        DynamicImage::ImageRgb8(DynamicImage::ImageRgba8(rgba).into_rgb8())
    }
}

/// 自動補正の世代を履歴に保存し、既存の結果とキャッシュを変えない。
pub const AUTO_VERSION: u64 = 3;

fn auto_for_version(img: DynamicImage, params: &Value) -> DynamicImage {
    if params["version"].as_u64() == Some(AUTO_VERSION) { auto_enhance(img) }
    else { auto_enhance_v2(img) }
}

/// 写真向け自動補正。ヒストグラムから露出と穏やかなコントラストを決める。
/// 黒白を固定する単調曲線なので、逆光の明部があっても暗い被写体を持ち上げられる。
/// WBは複数の明るさで一致する低彩度部分だけを根拠とし、彩度や局所HDRは加えない。
fn auto_enhance(img: DynamicImage) -> DynamicImage {
    let mut rgba = img.to_rgba8();
    let step = (rgba.as_raw().len() / 4).div_ceil(100_000).max(1);
    let luma = |rgb: [f32; 3]| 0.299 * rgb[0] + 0.587 * rgb[1] + 0.114 * rgb[2];
    let mut hist = [0u32; 256];
    let mut neutral_sum = [[0.0f32; 3]; 8];
    let mut neutral_count = [0u32; 8];
    let mut n = 0u32;
    for pixel in rgba.as_raw().chunks_exact(4).step_by(step) {
        if pixel[3] < 128 { continue; }
        let rgb = [pixel[0] as f32, pixel[1] as f32, pixel[2] as f32];
        let y = luma(rgb);
        hist[y.round().clamp(0.0, 255.0) as usize] += 1;
        n += 1;
        let min = rgb.into_iter().fold(f32::INFINITY, f32::min);
        let max = rgb.into_iter().fold(0.0f32, f32::max);
        if (40.0..=224.0).contains(&y) && min >= 24.0 && max <= 245.0
            && (max - min) / max <= 0.32
        {
            let bin = y as usize / 32;
            for c in 0..3 { neutral_sum[bin][c] += rgb[c] / y; }
            neutral_count[bin] += 1;
        }
    }
    // 無地や少数色の図形を写真と仮定して増幅しない。
    if n < 16 || hist.iter().filter(|&&count| count > 0).count() < 8 { return img; }
    let percentile = |fraction: f32| {
        let rank = (n as f32 * fraction) as u32;
        let mut sum = 0;
        for (value, count) in hist.iter().enumerate() {
            sum += count;
            if sum > rank { return value as f32 / 255.0; }
        }
        1.0
    };
    let (lo, mid, hi) = (percentile(0.02), percentile(0.50), percentile(0.98));
    let spread = hi - lo;
    if spread < 8.0 / 255.0 { return img; }
    // ほぼ無地の画像の微小なノイズを、大きな階調差に引き伸ばさない。
    let confidence = ((spread * 255.0 - 8.0) / 24.0).clamp(0.0, 1.0);

    let mut wb = [1.0f32; 3];
    let count: u32 = neutral_count.iter().sum();
    if count >= (n / 20).max(16) {
        let mut mean = [0.0f32; 3];
        for bin in &neutral_sum {
            for c in 0..3 { mean[c] += bin[c] / count as f32; }
        }
        let mut bins = 0;
        let mut consistent = true;
        for bin in 0..8 {
            if neutral_count[bin] < (n / 250).max(4) { continue; }
            bins += 1;
            for c in 0..3 {
                let ratio = neutral_sum[bin][c] / neutral_count[bin] as f32;
                consistent &= (ratio - mean[c]).abs() <= 0.04;
            }
        }
        if bins >= 3 && consistent && mean.iter().any(|v| (v - 1.0).abs() > 0.025) {
            for c in 0..3 {
                wb[c] = (1.0 + (1.0 / mean[c] - 1.0) * 0.65 * confidence).clamp(0.88, 1.16);
            }
        }
    }

    // 中央値を適正な範囲に寄せるが、曲線の暗部倍率は0.72〜2.5倍に制限する。
    // 98%点によるON/OFFを設けないため、明るい空を含む逆光写真でも働く。
    let mid = mid.clamp(1.0 / 255.0, 254.0 / 255.0);
    let target = mid.clamp(0.44, 0.64);
    let exposure = ((target * (1.0 - mid) / ((1.0 - target) * mid)).clamp(0.72, 2.5))
        .powf(confidence);
    let expose = |y: f32| exposure * y / (1.0 + (exposure - 1.0) * y);
    let exposed_mid = expose(mid);
    let exposed_spread = expose(hi) - expose(lo);
    // 既に広い階調はそのまま。狭い階調でも増幅は最大1.45倍に抑える。
    let contrast = 1.0 + ((0.84 / exposed_spread.max(0.01)).sqrt().clamp(1.0, 1.45) - 1.0) * confidence;
    if wb == [1.0; 3] && exposure == 1.0 && contrast == 1.0 { return img; }

    // 中央値を固定したlog-odds曲線。黒/白は保存し、階調の逆転やハードクリップを避ける。
    // LUT化により、フル解像度の各ピクセルでpow/logを計算しない。
    let center_odds = exposed_mid / (1.0 - exposed_mid);
    let mut tone = [0.0f32; 4097];
    for (i, value) in tone.iter_mut().enumerate().skip(1).take(4095) {
        let y = expose(i as f32 / 4096.0);
        let odds = center_odds * (y / ((1.0 - y) * center_odds)).powf(contrast);
        *value = odds / (1.0 + odds);
    }
    tone[4096] = 1.0;
    for pixel in rgba.pixels_mut() {
        if pixel[3] == 0 { continue; }
        let original = [pixel[0] as f32 / 255.0, pixel[1] as f32 / 255.0, pixel[2] as f32 / 255.0];
        let original_max = original.into_iter().fold(0.0f32, f32::max);
        // 白飛び済みの画素から色は復元できない。WBを滑らかに弱め、原本の白を保つ。
        let wb_strength = ((1.0 - original_max) / 0.08).clamp(0.0, 1.0);
        let rgb = [original[0] * (1.0 + (wb[0] - 1.0) * wb_strength),
                   original[1] * (1.0 + (wb[1] - 1.0) * wb_strength),
                   original[2] * (1.0 + (wb[2] - 1.0) * wb_strength)];
        let y = luma(rgb).clamp(0.0, 1.0);
        let at = y * 4096.0;
        let index = (at as usize).min(4095);
        let mapped = tone[index] + (tone[index + 1] - tone[index]) * (at - index as f32);
        let max = rgb.into_iter().fold(0.0f32, f32::max);
        // 8bitへの丸めで新たな255を増やさず、既存の白い点だけを白に保つ。
        let ceiling = if original_max < 1.0 { 254.0 / 255.0 } else { 1.0 };
        let gain = (mapped / y.max(1e-6)).min(ceiling / max.max(1e-6));
        for c in 0..3 { pixel[c] = (rgb[c] * gain * 255.0).round().clamp(0.0, 255.0) as u8; }
    }
    if img.color().has_alpha() { DynamicImage::ImageRgba8(rgba) }
    else { DynamicImage::ImageRgb8(DynamicImage::ImageRgba8(rgba).into_rgb8()) }
}

/// 言語指示1件を履歴1件として保存する。最大16個の色調操作のみ許可し、再帰はしない。
/// 幾何操作はマスク座標の追従が必要なため、従来通り独立した履歴に保存する。
fn pipeline(mut img: DynamicImage, pr: &Value) -> DynamicImage {
    let Some(list) = pr["edits"].as_array() else { return img };
    for edit in list.iter().take(16) {
        img = match edit["op"].as_str().unwrap_or("") {
            "filter" => filter(img, &edit["params"]),
            "adjust" => adjust(img, &edit["params"]),
            "auto" => auto_for_version(img, &edit["params"]),
            _ => img,
        };
    }
    img
}

/// 履歴を順に適用(op: adjust / crop / rotate / flip / filter / auto / pipeline)。
pub fn apply(mut img: DynamicImage, edits: &Value) -> DynamicImage {
    let Some(list) = edits.as_array() else { return img };
    for e in list {
        let pr = &e["params"];
        img = match e["op"].as_str().unwrap_or("") {
            "adjust" => adjust(img, pr),
            "auto" => auto_for_version(img, pr),
            "filter" => filter(img, pr),
            "pipeline" => pipeline(img, pr),
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
    // A second display request must never read a partially encoded rendition.
    static NEXT_WRITE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = NEXT_WRITE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = rp.with_extension(format!("{}.{}.tmp", std::process::id(), serial));
    if std::fs::write(&tmp, buf.get_ref()).is_ok() { let _ = std::fs::rename(&tmp, &rp); }
    let _ = std::fs::remove_file(tmp);
    Some(buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GrayImage, Luma, Rgb, RgbImage};

    fn gray_ramp(start: u8, end: u8) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_fn(256, 4, |x, _| {
            let value = start as u32 + x * (end - start) as u32 / 255;
            Rgb([value as u8; 3])
        }))
    }

    #[test]
    fn auto_preserves_flat_images_and_tiny_noise() {
        for rgb in [[0, 0, 0], [32, 32, 32], [128, 128, 128], [255, 255, 255],
                    [230, 180, 140], [250, 25, 10]] {
            let input = DynamicImage::ImageRgb8(RgbImage::from_pixel(16, 16, Rgb(rgb)));
            assert_eq!(auto_enhance(input.clone()).into_rgb8(), input.into_rgb8());
        }
        for (start, end) in [(25, 30), (115, 120), (235, 240)] {
            let input = gray_ramp(start, end);
            assert_eq!(auto_enhance(input.clone()).into_rgb8(), input.into_rgb8());
        }
    }

    #[test]
    fn auto_preserves_well_exposed_gray_ramp_and_full_range_scene() {
        let input = gray_ramp(0, 255);
        assert_eq!(auto_enhance(input.clone()).into_rgb8(), input.into_rgb8());
        // A dark subject with real white highlights already uses the available dynamic range.
        let input = DynamicImage::ImageRgb8(RgbImage::from_fn(100, 4, |x, _| {
            Rgb([if x < 95 { 32 } else { 255 }; 3])
        }));
        assert_eq!(auto_enhance(input.clone()).into_rgb8(), input.into_rgb8());
    }

    #[test]
    fn auto_lifts_underexposure_without_crushing_shadows_or_clipping_highlights() {
        let input = gray_ramp(0, 160);
        let output = auto_enhance(input.clone()).into_rgb8();
        let middle = output.get_pixel(128, 0)[0];
        assert!(middle > input.to_rgb8().get_pixel(128, 0)[0] + 8);
        assert_eq!(output.get_pixel(0, 0).0, [0; 3]);
        assert!(output.get_pixel(255, 0)[0] < 255);
        let mut previous = 0;
        for pixel in output.rows().next().unwrap() {
            assert!(pixel[0] >= previous);
            assert_eq!(pixel.0, [pixel[0]; 3]);
            previous = pixel[0];
        }
        assert_eq!(auto_enhance(input.clone()).into_rgb8(), auto_enhance(input).into_rgb8());
    }

    #[test]
    fn auto_recovers_bright_midtones_without_turning_white_gray() {
        let input = gray_ramp(140, 255);
        let output = auto_enhance(input.clone()).into_rgb8();
        assert!(output.get_pixel(128, 0)[0] + 3 < input.to_rgb8().get_pixel(128, 0)[0]);
        assert_eq!(output.get_pixel(255, 0).0, [255; 3]);
        assert!(output.get_pixel(0, 0)[0] > 100);
    }

    #[test]
    fn auto_restores_low_contrast_without_full_histogram_stretch() {
        let input = gray_ramp(80, 170);
        let before = input.to_rgb8();
        let output = auto_enhance(input).into_rgb8();
        let range = |image: &RgbImage| image.get_pixel(242, 0)[0] - image.get_pixel(13, 0)[0];
        assert!(range(&output) >= range(&before) + 10);
        assert!(range(&output) < range(&before) * 2);
        assert!((output.get_pixel(128, 0)[0] as i16 - before.get_pixel(128, 0)[0] as i16).abs() < 10);
        assert!(output.pixels().all(|p| p[0] > 0 && p[0] < 255));
    }

    #[test]
    fn auto_lifts_backlit_subject_even_with_bright_sky() {
        let input = DynamicImage::ImageRgb8(RgbImage::from_fn(256, 5, |x, row| {
            let value = if row < 4 { 20 + x * 70 / 255 } else { 210 + x * 45 / 255 };
            Rgb([value as u8; 3])
        }));
        let before = input.to_rgb8();
        let output = auto_enhance(input).into_rgb8();
        assert!(output.get_pixel(128, 0)[0] >= before.get_pixel(128, 0)[0] + 20);
        // Bright sky detail remains ordered and has not turned into a solid white band.
        assert!(output.get_pixel(240, 4)[0] >= 240);
        assert!(output.get_pixel(0, 4)[0] < output.get_pixel(200, 4)[0]);
        let clipped = |image: &RgbImage| image.pixels().filter(|p| p[0] == 255).count();
        assert!(clipped(&output) <= clipped(&before) + 12);
    }

    #[test]
    fn auto_high_key_midtones_recover_despite_black_details() {
        let input = DynamicImage::ImageRgb8(RgbImage::from_fn(256, 10, |x, row| {
            Rgb([if row == 0 { (x / 8) as u8 } else { (150 + x * 105 / 255) as u8 }; 3])
        }));
        let before = input.to_rgb8();
        let output = auto_enhance(input).into_rgb8();
        assert!(output.get_pixel(128, 5)[0] + 8 < before.get_pixel(128, 5)[0]);
        assert_eq!(output.get_pixel(255, 5)[0], 255);
        assert_eq!(output.get_pixel(0, 0)[0], 0);
    }

    #[test]
    fn auto_history_version_preserves_previous_results_including_pipelines() {
        let input = gray_ramp(80, 170);
        let legacy = auto_enhance_v2(input.clone()).into_rgb8();
        let modern = auto_enhance(input.clone()).into_rgb8();
        assert_ne!(legacy, modern);
        for params in [json!({}), json!({"version": 2})] {
            assert_eq!(apply(input.clone(), &json!([{"op": "auto", "params": params}])).into_rgb8(), legacy);
            assert_eq!(apply(input.clone(), &json!([{"op": "pipeline", "params": {"edits": [{"op": "auto", "params": params}]}}])).into_rgb8(), legacy);
        }
        assert_eq!(apply(input.clone(), &json!([{"op": "auto", "params": {"version": AUTO_VERSION}}])).into_rgb8(), modern);
        assert_eq!(apply(input, &json!([{"op": "pipeline", "params": {"edits": [{"op": "auto", "params": {"version": AUTO_VERSION}}]}}])).into_rgb8(), modern);
    }

    #[test]
    fn auto_keeps_hues_in_color_dominant_images() {
        for color in [[255, 0, 0], [40, 220, 20], [15, 45, 230]] {
            let input = DynamicImage::ImageRgb8(RgbImage::from_fn(256, 4, |x, _| {
                let scale = (x + 1) as f32 / 256.0;
                Rgb(color.map(|c| (c as f32 * scale).round() as u8))
            }));
            let original = input.to_rgb8();
            let output = auto_enhance(input).into_rgb8();
            for (before, after) in original.pixels().zip(output.pixels()) {
                // A common RGB multiplier preserves normalized channel ratios within rounding.
                let before_max = *before.0.iter().max().unwrap() as f32;
                let after_max = *after.0.iter().max().unwrap() as f32;
                for c in 0..3 {
                    if before_max > 20.0 && after_max > 20.0 {
                        assert!((before[c] as f32 / before_max - after[c] as f32 / after_max).abs() < 0.06);
                    }
                    if before[c] == 0 { assert_eq!(after[c], 0); }
                }
            }
        }
    }

    #[test]
    fn auto_corrects_consistent_neutral_cast_without_green_anchor() {
        for cast in [[1.10, 1.0, 0.88], [1.16, 1.0, 0.82], [0.94, 1.10, 0.94], [0.90, 1.0, 1.10]] {
            let input = DynamicImage::ImageRgb8(RgbImage::from_fn(256, 4, |x, _| {
                let level = 60.0 + x as f32 * 140.0 / 255.0;
                Rgb(cast.map(|gain| (level * gain).round() as u8))
            }));
            let before = input.to_rgb8().get_pixel(160, 0).0;
            let output = auto_enhance(input).into_rgb8();
            let after = output.get_pixel(160, 0).0;
            let spread = |rgb: [u8; 3]| *rgb.iter().max().unwrap() - *rgb.iter().min().unwrap();
            assert!(spread(after) < spread(before) * 3 / 4, "cast {cast:?}: {before:?} -> {after:?}");
        }
    }

    #[test]
    fn auto_white_balance_preserves_clipped_white_and_black_endpoints() {
        let input = DynamicImage::ImageRgb8(RgbImage::from_fn(256, 5, |x, row| {
            if row == 4 { return Rgb([if x < 128 { 0 } else { 255 }; 3]); }
            let level = 60.0 + x as f32 * 140.0 / 255.0;
            Rgb([1.16, 1.0, 0.82].map(|gain| (level * gain).round() as u8))
        }));
        let output = auto_enhance(input).into_rgb8();
        assert_eq!(output.get_pixel(0, 4).0, [0; 3]);
        assert_eq!(output.get_pixel(255, 4).0, [255; 3]);
        let cast = output.get_pixel(128, 0).0;
        assert!(cast[0] - cast[2] < 30);
    }

    #[test]
    fn auto_preserves_alpha_and_ignores_invisible_rgb_in_statistics() {
        let visible = gray_ramp(0, 160).into_rgba8();
        let input = image::RgbaImage::from_fn(256, 8, |x, y| {
            if y < 4 { *visible.get_pixel(x, y) }
            else { image::Rgba([255, 15, 220, 0]) }
        });
        let original = input.clone();
        let output = auto_enhance(DynamicImage::ImageRgba8(input)).into_rgba8();
        let expected = auto_enhance(DynamicImage::ImageRgba8(visible)).into_rgba8();
        for (x, y, pixel) in output.enumerate_pixels() {
            if y < 4 { assert_eq!(*pixel, *expected.get_pixel(x, y)); }
            else { assert_eq!(*pixel, *original.get_pixel(x, y)); }
        }
        let input = image::RgbaImage::from_fn(256, 4, |x, _| {
            let value = (x * 160 / 255) as u8;
            image::Rgba([value, value, value, 128 + x as u8 / 2])
        });
        let original_alpha: Vec<u8> = input.pixels().map(|p| p[3]).collect();
        let output = auto_enhance(DynamicImage::ImageRgba8(input)).into_rgba8();
        assert!(output.get_pixel(128, 0)[0] > 80);
        assert_eq!(output.pixels().map(|p| p[3]).collect::<Vec<_>>(), original_alpha);
    }

    #[test]
    fn auto_handles_empty_tiny_and_fully_transparent_images() {
        for (w, h) in [(0, 0), (0, 4), (1, 1), (2, 3), (16, 16)] {
            let input = image::RgbaImage::from_pixel(w, h, image::Rgba([12, 200, 89, 0]));
            assert_eq!(auto_enhance(DynamicImage::ImageRgba8(input.clone())).into_rgba8(), input);
        }
        let input = DynamicImage::ImageRgb8(RgbImage::from_pixel(1, 1, Rgb([128; 3])));
        assert_eq!(auto_enhance(input.clone()).into_rgb8(), input.into_rgb8());
    }

    fn step_image(horizontal: bool, brightness: u8) -> DynamicImage {
        DynamicImage::ImageLuma8(GrayImage::from_fn(32, 32, |x, y| {
            Luma([if (if horizontal { y } else { x }) >= 16 { brightness } else { 0 }])
        }))
    }

    #[test]
    fn canny_constant_and_tiny_images_are_black() {
        for (w, h) in [(0, 0), (0, 3), (1, 8), (8, 2), (16, 16)] {
            for brightness in [0, 127, 255] {
                let input = DynamicImage::ImageLuma8(GrayImage::from_pixel(w, h, Luma([brightness])));
                let output = canny(input, &json!({"low": 0, "high": 0})).into_luma8();
                assert_eq!(output.dimensions(), (w, h));
                assert!(output.pixels().all(|p| p[0] == 0));
            }
        }
    }

    #[test]
    fn canny_thins_horizontal_and_vertical_edges() {
        for horizontal in [false, true] {
            let output = canny(step_image(horizontal, 255), &json!({})).into_luma8();
            for line in 1..31 {
                let positions: Vec<u32> = (0..32).filter(|&position| {
                    let (x, y) = if horizontal { (line, position) } else { (position, line) };
                    output.get_pixel(x, y)[0] == 255
                }).collect();
                assert_eq!(positions.len(), 1, "each line should contain one edge: {positions:?}");
                assert!((15..=16).contains(&positions[0]));
            }
            assert!(output.pixels().all(|p| p[0] == 0 || p[0] == 255));
        }
    }

    #[test]
    fn canny_thresholds_control_weak_edges_and_are_order_independent() {
        let sensitive = canny(step_image(false, 32), &json!({"low": 5, "high": 20})).into_luma8();
        assert!(sensitive.pixels().any(|p| p[0] == 255));
        let insensitive = canny(step_image(false, 32), &json!({"low": 100, "high": 200})).into_luma8();
        assert!(insensitive.pixels().all(|p| p[0] == 0));
        let reversed = canny(step_image(false, 32), &json!({"low": 20, "high": 5})).into_luma8();
        assert_eq!(sensitive, reversed);
    }

    #[test]
    fn canny_clamps_parameters_and_falls_back_for_invalid_values() {
        let clamped = canny(step_image(false, 255), &json!({"low": 0, "high": 1020, "sigma": 0.3})).into_luma8();
        let excessive = canny(step_image(false, 255), &json!({"low": -100, "high": 99999, "sigma": -9})).into_luma8();
        assert_eq!(clamped, excessive);
        let defaults = canny(step_image(false, 255), &json!({})).into_luma8();
        let invalid = canny(step_image(false, 255), &json!({"low": "bad", "high": null, "sigma": []})).into_luma8();
        assert_eq!(defaults, invalid);
    }

    #[test]
    fn canny_hysteresis_keeps_connected_weak_edges_only() {
        let (w, h) = (8, 5);
        let mut strengths = vec![0.0; w * h];
        strengths[w + 1] = 120.0; // 強い輪郭
        strengths[2 * w + 2] = 60.0; // 斜めに接続
        strengths[3 * w + 3] = 60.0; // 間接的に接続
        strengths[3 * w + 4] = 49.0; // low未満
        strengths[w + 6] = 80.0; // 孤立した弱い輪郭
        let output = canny_hysteresis(&strengths, w, h, 50.0, 100.0);
        assert_eq!(output.iter().filter(|&&v| v == 255).count(), 3);
        assert_eq!(output[3 * w + 3], 255);
        assert_eq!(output[3 * w + 4], 0);
        assert_eq!(output[w + 6], 0);

        // 配列上の隣接で、行末と次の行頭を誤ってつながない。
        let mut wrapped = vec![0.0; w * h];
        wrapped[2 * w - 1] = 120.0;
        wrapped[2 * w] = 80.0;
        assert_eq!(canny_hysteresis(&wrapped, w, h, 50.0, 100.0)[2 * w], 0);
    }

    #[test]
    fn pipeline_matches_sequential_operations_and_renders_canny() {
        let input = step_image(false, 255);
        let edits = json!([
            {"op": "filter", "params": {"name": "canny"}},
            {"op": "filter", "params": {"name": "invert"}},
            {"op": "adjust", "params": {"exposure": -0.2}}
        ]);
        let expected = apply(input.clone(), &edits).into_rgb8();
        let combined = json!([{"op": "pipeline", "params": {"edits": edits}}]);
        assert_eq!(apply(input, &combined).into_rgb8(), expected);
        assert!(expected.pixels().any(|p| p[0] == 0));
        assert!(expected.pixels().any(|p| p[0] > 0));
    }

    #[test]
    fn pipeline_ignores_nested_and_geometric_operations_and_is_bounded() {
        let input = DynamicImage::ImageRgb8(RgbImage::from_pixel(4, 2, Rgb([10, 20, 30])));
        let unsupported = json!([{"op": "pipeline", "params": {"edits": [
            {"op": "rotate", "params": {"deg": 90}},
            {"op": "pipeline", "params": {"edits": [{"op": "filter", "params": {"name": "invert"}}]}}
        ]}}]);
        assert_eq!(apply(input.clone(), &unsupported).into_rgb8(), input.to_rgb8());
        let mut operations = vec![json!({"op": "adjust", "params": {}}); 16];
        operations.push(json!({"op": "filter", "params": {"name": "invert"}}));
        let oversized = json!([{"op": "pipeline", "params": {"edits": operations}}]);
        assert_eq!(apply(input.clone(), &oversized).into_rgb8(), input.into_rgb8());
    }
}

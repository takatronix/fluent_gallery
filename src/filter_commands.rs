//! Local language-to-filter composer, using the same filter names as fluent_scene.
//! A whole instruction is one non-destructive history entry. Unsupported language
//! returns None so the caller can ask its configured local language model.

use serde_json::{json, Map, Value};

const MAX_EDITS: usize = 8;

pub const SYSTEM_PROMPT: &str = r#"あなたは画像の見た目を調整するフィルタ設計者です。ユーザーの指示を次の JSON に変換し、JSON だけを返してください。
{"op":"pipeline","label":"短い日本語の説明","params":{"edits":[{"op":"filter","params":{"name":"canny"}}]}}
edits は指定された順で 1〜8 個。使える操作:
- filter: params.name は canny, grayscale, sepia, invert, posterize, vignette, sharpen, blur のみ。
  canny は境界線・輪郭・エッジ・線画だけを白線と黒背景で出す。鉛筆画は canny の後に invert。
  canny の任意パラメータ: low/high は 0〜1020 (通常 50/100)、sigma は 0.3〜5 (通常 1.2)。
  posterize の levels は 2〜32。vignette/sharpen/blur の amount は 0.1〜3 (通常 1)。
- adjust: params は exposure, contrast, saturation, temperature の数値のみ。それぞれ -1〜1、0 が無調整。
  exposure +0.3 は明るく、contrast +0.3 はコントラスト増、saturation -0.3 は彩度減、temperature +0.3 は暖色、-0.3 は寒色。
- auto: params は {}。自動補正。
禁止された効果を適用しない。画像の検索・削除・リセット・回転・切り抜き・人物や背景の変更・未知のフィルタは扱えません。背景だけ・顔だけなどの部分指定も扱えません。指示に応えられない時は {"unsupported":true}。複数の指示の一部だけを満たすチェーンを返さない。曖昧な要求にランダムな効果を返さない。"#;

#[derive(Clone, Copy)]
enum Effect {
    Filter(&'static str),
    Adjust(&'static str, f64),
    Tone(&'static str),
    Look(&'static str),
    Auto,
}

// Longer aliases at the same location win, keeping a look such as 鉛筆画 intact.
const VOCABULARY: &[(&[&str], Effect)] = &[
    (
        &["鉛筆画", "鉛筆", "スケッチ", "デッサン", "sketch", "pencil"],
        Effect::Look("sketch"),
    ),
    (
        &["映画風", "映画", "シネマ", "cinema", "cinematic"],
        Effect::Look("cinema"),
    ),
    (
        &[
            "レトロ",
            "古写真",
            "アンティーク",
            "ノスタルジ",
            "retro",
            "vintage",
        ],
        Effect::Look("retro"),
    ),
    (&["ノワール", "ノワール風", "noir"], Effect::Look("noir")),
    (
        &[
            "境界線",
            "境界",
            "輪郭",
            "エッジ",
            "線画",
            "canny",
            "edge_sobel",
            "edges",
            "outline",
        ],
        Effect::Filter("canny"),
    ),
    (
        &[
            "モノクロ",
            "白黒",
            "グレースケール",
            "grayscale",
            "greyscale",
            "monochrome",
            "black and white",
        ],
        Effect::Filter("grayscale"),
    ),
    (&["セピア", "sepia"], Effect::Filter("sepia")),
    (
        &["色反転", "色を反転", "ネガ", "invert", "negative"],
        Effect::Filter("invert"),
    ),
    (
        &[
            "ポスタリゼーション",
            "ポスタライズ",
            "ポスター",
            "減色",
            "posterize",
            "posterise",
        ],
        Effect::Filter("posterize"),
    ),
    (
        &["ビネット", "周辺減光", "周辺を暗く", "vignette"],
        Effect::Filter("vignette"),
    ),
    (
        &["シャープ", "くっきり", "鮮明", "sharpen"],
        Effect::Filter("sharpen"),
    ),
    (&["ぼか", "ぼや", "ブラー", "blur"], Effect::Filter("blur")),
    (
        &["明る", "明るさを上げ", "brighten", "brighter"],
        Effect::Adjust("exposure", 0.3),
    ),
    (
        &["暗く", "暗め", "明るさを下げ", "darken", "darker"],
        Effect::Adjust("exposure", -0.3),
    ),
    (
        &["暖か", "暖色", "温か", "あたたか", "warm"],
        Effect::Adjust("temperature", 0.4),
    ),
    (
        &["冷た", "寒色", "青っぽ", "cool", "cold"],
        Effect::Adjust("temperature", -0.4),
    ),
    (
        &["鮮やか", "あざやか", "ビビッド", "vivid", "vibrant"],
        Effect::Adjust("saturation", 0.35),
    ),
    (&["露出", "exposure"], Effect::Tone("exposure")),
    (&["コントラスト", "contrast"], Effect::Tone("contrast")),
    (&["彩度", "saturation"], Effect::Tone("saturation")),
    (&["色温度", "temperature"], Effect::Tone("temperature")),
    (
        &[
            "自動補正",
            "自動調整",
            "auto enhance",
            "auto-enhance",
            "auto_enhance",
        ],
        Effect::Auto,
    ),
];

struct Hit {
    start: usize,
    end: usize,
    effect: Effect,
}

fn contains_any(text: &str, words: &[&str]) -> bool {
    words.iter().any(|word| text.contains(word))
}

fn contains_word(text: &str, word: &str) -> bool {
    text.match_indices(word).any(|(start, _)| {
        !text[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphabetic())
            && !text[start + word.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
    })
}

/// Keyword composition can only express whole-image filters. Decline an entire
/// request when known unsupported requirements accompany a supported keyword;
/// otherwise "背景だけぼかして" would silently blur everything.
fn has_unsupported_requirement(text: &str) -> bool {
    contains_any(
        text,
        &[
            "背景",
            "前景",
            "人物だけ",
            "人物のみ",
            "人物を",
            "顔だけ",
            "顔のみ",
            "顔を",
            "肌だけ",
            "肌を",
            "髪だけ",
            "髪を",
            "目だけ",
            "目を",
            "服だけ",
            "服を",
            "被写体だけ",
            "被写体のみ",
            "被写体を",
            "人だけ",
            "人以外",
            "顔以外",
            "人物以外",
            "一部だけ",
            "一部分",
            "部分的",
            "選択範囲",
            "選択した",
            "右半分",
            "左半分",
            "上半分",
            "下半分",
            "中央だけ",
            "中心だけ",
            "周囲だけ",
            "回転",
            "左右反転",
            "上下反転",
            "ミラー",
            "クロップ",
            "トリミング",
            "切り抜",
            "リサイズ",
            "拡大",
            "縮小",
            "透明",
            "透過",
            "削除",
            "消去",
            "除去",
            "取り除",
            "描いて",
            "描き足",
            "生成して",
            "探して",
            "検索して",
            "油絵",
            "油彩",
            "水彩",
            "水墨",
            "墨絵",
            "アニメ",
            "セル画",
            "美肌",
            "モザイク",
            "ドット絵",
            "エンボス",
            "二値化",
            "2値化",
            "ソラリゼーション",
            "万華鏡",
            "色相",
            "ステンドグラス",
            "グリッチ",
            "ブラウン管",
            "魚眼",
            "テクスチャ",
        ],
    ) || [
        "background",
        "foreground",
        "face",
        "skin",
        "hair",
        "subject",
        "person",
        "region",
        "rotate",
        "rotation",
        "crop",
        "resize",
        "mirror",
        "transparent",
        "transparency",
        "delete",
        "erase",
        "remove",
        "draw",
        "generate",
        "search",
        "watercolor",
        "oilpaint",
        "sumie",
        "anime",
        "toon",
        "beauty",
        "pixelate",
        "pixelart",
        "emboss",
        "threshold",
        "solarize",
        "kaleido",
        "hue",
        "stainedglass",
        "glitch",
        "crt",
        "ntsc",
        "fisheye",
        "texture",
        "lut",
    ]
    .iter()
    .any(|word| contains_word(text, word))
}

fn clause_before(text: &str) -> &str {
    text.rsplit(['、', '。', ',', '.', ';', '；', '\n', '！', '!', '？', '?'])
        .next()
        .unwrap_or(text)
}

fn clause_after(text: &str) -> &str {
    text.split(['、', '。', ',', '.', ';', '；', '\n', '！', '!', '？', '?'])
        .next()
        .unwrap_or(text)
}

fn is_negative(before: &str, after: &str) -> bool {
    // Bound negation to this effect; a later positive instruction is independent.
    let after = clause_after(after);
    contains_any(
        after,
        &[
            "ない",
            "なく",
            "なし",
            "無し",
            "せず",
            "さず",
            "しません",
            "するな",
            "すな",
            "禁止",
            "不要",
            "やめ",
            "外して",
            "除いて",
            "解除",
            "消して",
            "変えず",
            "維持",
            "そのまま",
        ],
    ) || contains_any(
        clause_before(before),
        &[
            "don't ", "do not ", "without ", "no ", "not ", "avoid ", "remove ", "skip ",
        ],
    )
}

fn strength(before: &str, after: &str) -> f64 {
    let scope = format!("{} {}", clause_before(before), clause_after(after));
    if contains_any(
        &scope,
        &[
            "ほんのり",
            "少し",
            "すこし",
            "弱",
            "軽く",
            "うっすら",
            "控えめ",
            "subtle",
            "slightly",
            "gentle",
        ],
    ) {
        0.5
    } else if contains_any(
        &scope,
        &[
            "強",
            "かなり",
            "もっと",
            "激しく",
            "めっちゃ",
            "大きく",
            "strong",
            "very",
        ],
    ) {
        1.6
    } else {
        1.0
    }
}

fn filter(name: &str, extra: Value) -> Value {
    let mut params = extra.as_object().cloned().unwrap_or_default();
    params.insert("name".into(), json!(name));
    json!({"op":"filter", "params":params})
}

fn adjust(key: &str, amount: f64) -> Value {
    let mut params = Map::new();
    params.insert(key.into(), json!(amount.clamp(-1.0, 1.0)));
    json!({"op":"adjust", "params":params})
}

fn filter_label(name: &str) -> &'static str {
    match name {
        "canny" => "境界線",
        "grayscale" => "モノクロ",
        "sepia" => "セピア",
        "invert" => "色反転",
        "posterize" => "減色",
        "vignette" => "ビネット",
        "sharpen" => "シャープ",
        "blur" => "ぼかし",
        _ => "フィルタ",
    }
}

fn tone_label(key: &str) -> &'static str {
    match key {
        "exposure" => "明るさ",
        "contrast" => "コントラスト",
        "saturation" => "彩度",
        "temperature" => "色温度",
        _ => "調整",
    }
}

/// Recognized effects preserve the user's written order, including repeated effects.
pub fn parse(text: &str) -> Option<Value> {
    let prompt = text.trim();
    if prompt.is_empty() || prompt.chars().count() > 2000 {
        return None;
    }
    let normalized = prompt.to_lowercase();
    if has_unsupported_requirement(&normalized) {
        return None;
    }
    let mut hits = Vec::new();
    for (aliases, effect) in VOCABULARY {
        for alias in *aliases {
            for (start, _) in normalized.match_indices(alias) {
                let end = start + alias.len();
                // English vocabulary must be a whole word (avoid e.g. "cooldown").
                if alias.is_ascii()
                    && (normalized[..start]
                        .chars()
                        .next_back()
                        .is_some_and(|c| c.is_ascii_alphabetic())
                        || normalized[end..]
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_ascii_alphabetic()))
                {
                    continue;
                }
                hits.push(Hit {
                    start,
                    end,
                    effect: *effect,
                });
            }
        }
    }
    hits.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    let mut selected = Vec::<Hit>::new();
    for hit in hits {
        if selected.last().is_none_or(|last| hit.start >= last.end) {
            selected.push(hit);
        }
    }
    let mut edits = Vec::new();
    let mut labels = Vec::new();
    for (i, hit) in selected.iter().enumerate() {
        let previous = if i == 0 { 0 } else { selected[i - 1].end };
        let next = selected.get(i + 1).map_or(normalized.len(), |h| h.start);
        let before = &normalized[previous..hit.start];
        let after = &normalized[hit.end..next];
        if is_negative(before, after) {
            continue;
        }
        let k = strength(before, after);
        match hit.effect {
            Effect::Filter(name) => {
                let extra = match name {
                    "blur" | "sharpen" | "vignette" => json!({"amount":k}),
                    "posterize" => json!({"levels":(5.0 / k).round().clamp(2.0, 32.0)}),
                    "canny" if k != 1.0 => json!({"low":50.0 / k,"high":100.0 / k}),
                    _ => json!({}),
                };
                edits.push(filter(name, extra));
                labels.push(filter_label(name));
            }
            Effect::Adjust(key, amount) => {
                edits.push(adjust(key, amount * k));
                labels.push(tone_label(key));
            }
            Effect::Tone(key) => {
                let scope = format!("{} {}", clause_before(before), clause_after(after));
                let direction = if contains_any(
                    &scope,
                    &[
                        "下げ", "落と", "減ら", "低く", "低め", "弱", "抑え", "控え", "decrease",
                        "reduce", "lower", "less",
                    ],
                ) || (key == "temperature"
                    && contains_any(&scope, &["寒", "冷"]))
                {
                    -1.0
                } else {
                    1.0
                };
                edits.push(adjust(key, 0.3 * direction * k));
                labels.push(tone_label(key));
            }
            Effect::Look(name) => {
                match name {
                    "sketch" => {
                        edits.extend([filter("canny", json!({})), filter("invert", json!({}))]);
                        labels.push("鉛筆画");
                    }
                    "retro" => {
                        edits.extend([
                            filter("sepia", json!({})),
                            filter("vignette", json!({"amount":0.65 * k})),
                        ]);
                        labels.push("レトロ");
                    }
                    "cinema" => {
                        edits.extend([json!({"op":"adjust","params":{"contrast":0.2 * k,"saturation":-0.15 * k}}),
                        filter("vignette", json!({"amount":0.65 * k}))]);
                        labels.push("映画風");
                    }
                    "noir" => {
                        edits.extend([
                            filter("grayscale", json!({})),
                            adjust("contrast", 0.3 * k),
                            filter("vignette", json!({"amount":0.8 * k})),
                        ]);
                        labels.push("ノワール");
                    }
                    _ => {}
                }
            }
            Effect::Auto => {
                edits.push(json!({"op":"auto","params":{}}));
                labels.push("自動補正");
            }
        }
    }
    if edits.is_empty() || edits.len() > MAX_EDITS {
        return None;
    }
    let pipeline = json!({"op":"pipeline", "label":labels.join(" → "), "prompt":prompt, "params":{"edits":edits}});
    validate(&pipeline).ok()
}

fn number(
    params: &Map<String, Value>,
    name: &str,
    lo: f64,
    hi: f64,
    output: &mut Map<String, Value>,
) -> Result<(), String> {
    if let Some(value) = params.get(name) {
        let value = value
            .as_f64()
            .filter(|v| v.is_finite())
            .ok_or_else(|| format!("{name} は有限の数値で指定してください"))?;
        output.insert(name.into(), json!(value.clamp(lo, hi)));
    }
    Ok(())
}

fn allowed_keys(params: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    if let Some(key) = params.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("未対応のパラメータ: {key}"));
    }
    Ok(())
}

/// Validate every operation before any history is changed; drop model metadata.
pub fn validate(value: &Value) -> Result<Value, String> {
    if value["unsupported"].as_bool() == Some(true) {
        return Err("この指示に対応するフィルタがありません".into());
    }
    if value["op"].as_str() != Some("pipeline") {
        return Err("フィルタの pipeline が必要です".into());
    }
    let items = value["params"]["edits"]
        .as_array()
        .ok_or("params.edits が必要です")?;
    if items.is_empty() || items.len() > MAX_EDITS {
        return Err("フィルタは 1〜8 個で指定してください".into());
    }
    let mut edits = Vec::with_capacity(items.len());
    for item in items {
        let params = item["params"]
            .as_object()
            .ok_or("各操作に params オブジェクトが必要です")?;
        let mut out = Map::new();
        let op = item["op"].as_str().ok_or("操作名が必要です")?;
        match op {
            "filter" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("フィルタ名が必要です")?;
                match name {
                    "grayscale" | "sepia" | "invert" => allowed_keys(params, &["name"])?,
                    "posterize" => {
                        allowed_keys(params, &["name", "levels"])?;
                        number(params, "levels", 2.0, 32.0, &mut out)?;
                        if let Some(levels) = out.get_mut("levels") {
                            *levels = json!(levels.as_f64().unwrap().round());
                        }
                    }
                    "vignette" | "sharpen" | "blur" => {
                        allowed_keys(params, &["name", "amount"])?;
                        number(params, "amount", 0.1, 3.0, &mut out)?;
                    }
                    "canny" => {
                        allowed_keys(params, &["name", "sigma", "low", "high"])?;
                        number(params, "sigma", 0.3, 5.0, &mut out)?;
                        number(params, "low", 0.0, 1020.0, &mut out)?;
                        number(params, "high", 0.0, 1020.0, &mut out)?;
                        if out.contains_key("low") || out.contains_key("high") {
                            let low = out.get("low").and_then(Value::as_f64).unwrap_or(50.0);
                            let high = out.get("high").and_then(Value::as_f64).unwrap_or(100.0);
                            out.insert("low".into(), json!(low.min(high)));
                            out.insert("high".into(), json!(low.max(high)));
                        }
                    }
                    _ => return Err(format!("未対応のフィルタ: {name}")),
                }
                out.insert("name".into(), json!(name));
            }
            "adjust" => {
                let keys = ["exposure", "contrast", "saturation", "temperature"];
                allowed_keys(params, &keys)?;
                if params.is_empty() {
                    return Err("調整する値が必要です".into());
                }
                for key in keys {
                    number(params, key, -1.0, 1.0, &mut out)?;
                }
            }
            "auto" => {
                allowed_keys(params, &["version"])?;
                // Distinguish corrected auto rendering from previously baked cache results.
                if params.get("version").is_some_and(|v| v != 2) {
                    return Err("未対応の自動補正バージョンです".into());
                }
                out.insert("version".into(), json!(2));
            }
            _ => return Err(format!("未対応の操作: {op}")),
        }
        edits.push(json!({"op":op,"params":out}));
    }
    let label = value["label"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("言葉でフィルタ");
    let mut result = json!({"op":"pipeline","label":label.chars().take(120).collect::<String>(),"params":{"edits":edits}});
    if let Some(prompt) = value["prompt"].as_str() {
        result["prompt"] = json!(prompt.chars().take(2000).collect::<String>());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(value: &Value) -> Vec<&str> {
        value["params"]["edits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|edit| {
                edit["params"]["name"]
                    .as_str()
                    .unwrap_or(edit["op"].as_str().unwrap())
            })
            .collect()
    }

    #[test]
    fn boundary_instruction_uses_canny_and_keeps_original_prompt() {
        for text in [
            "境界線だけにして",
            "輪郭だけ",
            "エッジを抽出",
            "線画にして",
            "Canny",
        ] {
            let value = parse(text).unwrap();
            assert_eq!(value["op"], "pipeline");
            assert_eq!(names(&value), ["canny"]);
            assert_eq!(value["prompt"], text);
        }
    }

    #[test]
    fn compound_order_is_observable_and_looks_expand_in_place() {
        assert_eq!(
            names(&parse("ぼかしてから境界線だけにして色を反転").unwrap()),
            ["blur", "canny", "invert"]
        );
        assert_eq!(
            names(&parse("色反転してからぼかして").unwrap()),
            ["invert", "blur"]
        );
        assert_eq!(
            names(&parse("鉛筆画にしてからセピア").unwrap()),
            ["canny", "invert", "sepia"]
        );
        assert_eq!(names(&parse("レトロ").unwrap()), ["sepia", "vignette"]);
    }

    #[test]
    fn negative_and_unknown_commands_do_not_apply_a_positive_effect() {
        for text in [
            "ぼかさない",
            "ぼかすな",
            "モノクロにするな",
            "モノクロにしないで",
            "輪郭線は不要",
            "don't blur",
            "no sepia",
            "猫を探して",
            "リセット",
            "元に戻して",
            "cooldown",
        ] {
            assert!(parse(text).is_none(), "unexpected interpretation: {text}");
        }
        assert_eq!(
            names(&parse("ぼかさないでモノクロにして").unwrap()),
            ["grayscale"]
        );
        assert_eq!(names(&parse("no blur, sepia please").unwrap()), ["sepia"]);
    }

    #[test]
    fn scoped_or_mixed_unsupported_instructions_do_not_become_whole_image_edits() {
        for text in [
            "背景だけぼかして",
            "背景をぼかして",
            "顔だけ明るく",
            "人物以外をモノクロにして",
            "左半分をセピア",
            "一部だけ境界線にして",
            "ぼかしてから90度回転して",
            "モノクロにして犬を描いて",
            "セピアで水彩画にして",
            "Cannyと二値化",
            "blur the background",
            "brighten only the face",
            "grayscale and crop",
            "sepia and watercolor",
            "invert then erase the person",
        ] {
            assert!(parse(text).is_none(), "partial interpretation: {text}");
        }
        // Whole-image requests and filter-name words inside unrelated words still work.
        assert_eq!(names(&parse("画像全体をぼかして").unwrap()), ["blur"]);
        assert_eq!(names(&parse("境界線だけにして").unwrap()), ["canny"]);
        assert_eq!(
            names(&parse("モノクロの写真にして").unwrap()),
            ["grayscale"]
        );
        assert_eq!(
            names(&parse("absolutely grayscale").unwrap()),
            ["grayscale"]
        );
    }

    #[test]
    fn adjustments_respect_direction_and_local_strength() {
        let value = parse("少し明るく、彩度を下げて、色温度を上げて").unwrap();
        let edits = value["params"]["edits"].as_array().unwrap();
        assert_eq!(edits[0]["params"]["exposure"], 0.15);
        assert_eq!(edits[1]["params"]["saturation"], -0.3);
        assert_eq!(edits[2]["params"]["temperature"], 0.3);
        let weak = parse("少しぼかして").unwrap();
        let strong = parse("強くぼかして").unwrap();
        assert!(
            weak["params"]["edits"][0]["params"]["amount"]
                .as_f64()
                .unwrap()
                < strong["params"]["edits"][0]["params"]["amount"]
                    .as_f64()
                    .unwrap()
        );
    }

    #[test]
    fn validation_bounds_values_and_rejects_unsupported_effects_atomically() {
        let value = validate(&json!({"op":"pipeline","params":{"edits":[
            {"op":"adjust","params":{"exposure":200,"saturation":-9}},
            {"op":"filter","params":{"name":"blur","amount":500}}
        ]}}))
        .unwrap();
        assert_eq!(value["params"]["edits"][0]["params"]["exposure"], 1.0);
        assert_eq!(value["params"]["edits"][0]["params"]["saturation"], -1.0);
        assert_eq!(value["params"]["edits"][1]["params"]["amount"], 3.0);
        for bad in [
            json!({"op":"filter","params":{"name":"unknown"}}),
            json!({"op":"delete","params":{}}),
            json!({"op":"pipeline","params":{"edits":[]}}),
            json!({"op":"adjust","params":{"exposure":"2"}}),
            json!({"op":"filter","params":{}}),
            json!({"op":"filter","params":{"name":"blur","command":"anything"}}),
        ] {
            assert!(validate(&json!({"op":"pipeline","params":{"edits":[bad]}})).is_err());
        }
        assert!(validate(&json!({"unsupported":true})).is_err());
        assert!(validate(
            &json!({"op":"pipeline","params":{"edits":vec![json!({"op":"auto","params":{}});9]}})
        )
        .is_err());
    }

    #[test]
    fn canny_validation_matches_backend_thresholds_and_defaults() {
        let value = validate(&json!({"op":"pipeline","params":{"edits":[
            {"op":"filter","params":{"name":"canny","low":200,"high":10}},
            {"op":"filter","params":{"name":"canny","high":20}},
            {"op":"filter","params":{"name":"canny","low":-5,"high":5000,"sigma":50}}
        ]}}))
        .unwrap();
        let edits = &value["params"]["edits"];
        assert_eq!(edits[0]["params"]["low"], 10.0);
        assert_eq!(edits[0]["params"]["high"], 200.0);
        assert_eq!(edits[1]["params"]["low"], 20.0);
        assert_eq!(edits[1]["params"]["high"], 50.0);
        assert_eq!(edits[2]["params"]["low"], 0.0);
        assert_eq!(edits[2]["params"]["high"], 1020.0);
        assert_eq!(edits[2]["params"]["sigma"], 5.0);
    }
}

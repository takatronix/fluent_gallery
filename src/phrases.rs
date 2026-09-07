//! よく使う言語指示(タグ)。目標を書くのが面倒な人向けに、クリックで足せる語を出す。
//! フォルダの目標を保存した時に句読点で切った断片を学習し、使った物ほど上に来る(2026-09-07 指示「学習して増えていく」)。
//! 正本は store/phrases.json: {"gen":[{text,count,last,hidden}], "crawl":[…]}。出荷時の種(seed)は count 0 で常に混ぜ、hidden で消せる。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const SEED_GEN: &[&str] = &[
    "写真のようにリアルに", "80年代のアニメ絵", "アニメ調", "水彩画風", "油絵風", "線画", "ドット絵", "3DCG風",
    "被写体はそのまま", "背景を変えて", "季節を変えて", "ポーズをいろいろ変えて", "構図をいろいろ変えて",
    "人は写さない", "全身が写るように", "顔のアップ", "正面向き", "横向き",
    "夜景", "雨の日", "雪景色", "夕焼け", "逆光", "柔らかい自然光", "スタジオ撮影", "白背景",
    "文字は入れない", "高精細", "シンプルな背景",
];
const SEED_CRAWL: &[&str] = &[
    "実写優先", "イラスト優先", "人が写っていない物", "全身が写っている物", "顔がはっきり写っている物",
    "高解像度", "白背景", "透かし無し", "1体だけ写っている物", "正面向き", "横向き", "屋外", "屋内", "夜",
];

pub fn path(root: &Path) -> PathBuf { root.join("store/phrases.json") }

fn load(root: &Path) -> Value {
    std::fs::read_to_string(path(root)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({}))
}
fn save(root: &Path, v: &Value) {
    let _ = std::fs::create_dir_all(root.join("store"));
    let _ = std::fs::write(path(root), serde_json::to_string_pretty(v).unwrap_or_default());
}
fn now() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) }
fn kind_key(kind: &str) -> &'static str { if kind == "gen" { "gen" } else { "crawl" } }
fn seeds(kind: &str) -> &'static [&'static str] { if kind == "gen" { SEED_GEN } else { SEED_CRAWL } }

/// 一覧: 学習済み(count 順・新しい順)→ 種の順。hidden は出さない。[{text, count, seed}]
pub fn list(root: &Path, kind: &str) -> Vec<Value> {
    let all = load(root);
    let learned = all[kind_key(kind)].as_array().cloned().unwrap_or_default();
    let hidden: Vec<String> = learned.iter().filter(|e| e["hidden"].as_bool() == Some(true)).filter_map(|e| e["text"].as_str().map(String::from)).collect();
    let mut out: Vec<(i64, u64, String, bool)> = vec![];
    for e in &learned {
        if e["hidden"].as_bool() == Some(true) { continue; }
        if let Some(t) = e["text"].as_str() {
            out.push((e["count"].as_i64().unwrap_or(0), e["last"].as_u64().unwrap_or(0), t.to_string(), false));
        }
    }
    for s in seeds(kind) {
        if hidden.iter().any(|h| h == s) || out.iter().any(|(_, _, t, _)| t == s) { continue; }
        out.push((0, 0, s.to_string(), true));
    }
    out.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    out.into_iter().map(|(c, _, t, seed)| json!({"text": t, "count": c, "seed": seed})).collect()
}

fn bump(root: &Path, kind: &str, texts: &[String]) {
    if texts.is_empty() { return; }
    let mut all = load(root);
    let k = kind_key(kind);
    if !all[k].is_array() { all[k] = json!([]); }
    let arr = all[k].as_array_mut().unwrap();
    for t in texts {
        if let Some(e) = arr.iter_mut().find(|e| e["text"].as_str() == Some(t.as_str())) {
            e["count"] = json!(e["count"].as_i64().unwrap_or(0) + 1);
            e["last"] = json!(now());
            e["hidden"] = json!(false); // 使ったら復活
        } else {
            arr.push(json!({"text": t, "count": 1, "last": now()}));
        }
    }
    // 増えすぎない(count 1 の古い物から落とす)
    if arr.len() > 300 {
        arr.sort_by(|a, b| b["count"].as_i64().cmp(&a["count"].as_i64()).then(b["last"].as_u64().cmp(&a["last"].as_u64())));
        arr.truncate(300);
    }
    save(root, &all);
}

/// タグをクリックして使った(1 回分)
pub fn used(root: &Path, kind: &str, text: &str) {
    let t = text.trim();
    if t.is_empty() { return; }
    bump(root, kind, &[t.to_string()]);
}

/// 目標の文から断片を学習する。句読点・改行・スラッシュで切り、2〜40 文字の物だけ。短い目標は丸ごとも 1 語として覚える
pub fn learn(root: &Path, kind: &str, goal: &str) {
    let g = goal.trim();
    if g.is_empty() { return; }
    let mut frags: Vec<String> = g.split(|c: char| matches!(c, '。' | '、' | '，' | ',' | '.' | '\n' | ';' | '；' | '/' | '／' | '・'))
        .map(|s| s.trim().trim_matches(|c: char| c == '　' || c == ' ').to_string())
        .filter(|s| { let n = s.chars().count(); (2..=40).contains(&n) })
        .collect();
    if g.chars().count() <= 40 && !frags.iter().any(|f| f == g) { frags.push(g.to_string()); }
    frags.sort(); frags.dedup();
    bump(root, kind, &frags);
}

/// タグを消す(種も消せる=hidden)。使えば復活
pub fn hide(root: &Path, kind: &str, text: &str) {
    let mut all = load(root);
    let k = kind_key(kind);
    if !all[k].is_array() { all[k] = json!([]); }
    let arr = all[k].as_array_mut().unwrap();
    if let Some(e) = arr.iter_mut().find(|e| e["text"].as_str() == Some(text)) { e["hidden"] = json!(true); }
    else { arr.push(json!({"text": text, "count": 0, "last": 0, "hidden": true})); }
    save(root, &all);
}

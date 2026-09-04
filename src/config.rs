//! 設定の正本 = `store/config.json`(docs/gen-design.md §8.1)。設定画面(UI)が行ごとに自動保存で書く。
//! 優先順: 環境変数(開発・一時上書き) > config.json > 旧 `~/ml-hub/config/settings.json`(Linux の後方互換、読むだけ)。
//! キーは平文でここに置く(store 内だが書き出し/zip には含めない)。値の読み出しは点区切りパス(`keys.anthropic`, `gen.base`)。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static ROOT: OnceLock<PathBuf> = OnceLock::new();
static CFG: Mutex<Option<Value>> = Mutex::new(None);

/// 既定値(UI はここに無いキーを出さない=未実装の設定を見せない)
pub fn defaults() -> Value {
    json!({
        "keys": {"anthropic": "", "openai": "", "openrouter": "", "xai": "", "pexels": "", "pixabay": ""},
        "roles": {"judge": ""},                       // 目利きの既定モデル(空=claude-sonnet-5)。フォルダ設定が優先
        "gen": {"base": "", "port": 8092, "size": "1024x1024", "steps": 0, "model": "flux2-klein-4b", "preview": true}, // base=別マシンの sd-server(空=内蔵)。steps 0=モデルの既定。preview=途中経過(sd-cli)
        "vlm": {"base": ""},                          // 別マシンの llama-server / OpenAI 互換 VLM(空=内蔵)
        "tools": {"sd_server": "", "llama_server": ""}, // バイナリの手動指定(空=自動検出)
        "autopilot": {"interval_min": 30, "groom": true}, // ♻見回りの周期 / 属性・マスクの自動お手入れ
        "storage": {"cache_mb": 20480},               // preview/render キャッシュの上限
    })
}

pub fn path() -> PathBuf {
    ROOT.get().map(|r| r.join("store/config.json")).unwrap_or_else(|| PathBuf::from("store/config.json"))
}

pub fn init(root: &Path) {
    let _ = ROOT.set(root.to_path_buf());
    let v = std::fs::read_to_string(path()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .filter(|v| v.is_object()).unwrap_or_else(|| json!({}));
    *CFG.lock().unwrap() = Some(v);
}

/// 保存されている生の設定(既定とマージ済み)
pub fn get() -> Value {
    let mut base = defaults();
    if let Some(v) = CFG.lock().unwrap().as_ref() {
        merge(&mut base, v);
    }
    base
}

fn merge(base: &mut Value, over: &Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(k) {
                    Some(bv) if bv.is_object() && v.is_object() => merge(bv, v),
                    _ => { b.insert(k.clone(), v.clone()); }
                }
            }
        }
        (b, o) => *b = o.clone(),
    }
}

fn ptr(path: &str) -> String { format!("/{}", path.replace('.', "/")) }

pub fn value(path: &str) -> Value { get().pointer(&ptr(path)).cloned().unwrap_or(Value::Null) }
pub fn get_str(path: &str) -> Option<String> {
    value(path).as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}
pub fn get_u64(path: &str, default: u64) -> u64 { value(path).as_u64().unwrap_or(default) }
pub fn get_bool(path: &str, default: bool) -> bool { value(path).as_bool().unwrap_or(default) }

/// 環境変数(開発用の上書き)があればそれ、無ければ config
pub fn env_or(env: &str, path: &str) -> Option<String> {
    std::env::var(env).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).or_else(|| get_str(path))
}

/// API キー: config の keys.<name> → 旧 ml-hub settings.json の <name>_api_key
pub fn key(name: &str) -> Option<String> {
    get_str(&format!("keys.{name}")).or_else(|| legacy(&format!("{name}_api_key")))
}

/// 旧 ~/ml-hub/config/settings.json(Linux の ml-hub と共用していた置き場)。読むだけ
pub fn legacy(name: &str) -> Option<String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let p = Path::new(&home).join("ml-hub/config/settings.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
    v[name].as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// 設定画面が書く。既定に無いパスは受け付けない(未実装の設定を貯めない)。空文字は「既定に戻す」
pub fn set(path: &str, v: Value) -> Result<Value, String> {
    let d = defaults();
    let Some(dv) = d.pointer(&ptr(path)) else { return Err(format!("知らない設定: {path}")) };
    if dv.is_object() { return Err(format!("節ごとの一括設定はできません: {path}")); }
    let v = match (dv, &v) {
        (Value::Number(_), Value::Number(_)) | (Value::Bool(_), Value::Bool(_)) | (Value::String(_), Value::String(_)) => v,
        (Value::Number(_), Value::String(s)) => s.trim().parse::<f64>().ok().and_then(|f| serde_json::Number::from_f64(f)).map(Value::Number).ok_or("数値をください")?,
        (Value::Bool(_), Value::String(s)) => json!(matches!(s.trim(), "1" | "true" | "on")),
        (Value::String(_), Value::Number(n)) => json!(n.to_string()),
        _ => return Err(format!("型が違います: {path}")),
    };
    let mut g = CFG.lock().unwrap();
    let cur = g.get_or_insert_with(|| json!({}));
    let parts: Vec<&str> = path.split('.').collect();
    let mut node = &mut *cur;
    for p in &parts[..parts.len() - 1] {
        if !node[*p].is_object() { node[*p] = json!({}); }
        node = &mut node[*p];
    }
    let last = parts[parts.len() - 1];
    // 既定と同じ(または空)なら消して既定に戻す
    let is_default = &v == dv || v.as_str().map(|s| s.is_empty()).unwrap_or(false);
    if is_default { node.as_object_mut().map(|o| o.remove(last)); } else { node[last] = v; }
    let p = path_file();
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    std::fs::write(&p, serde_json::to_string_pretty(cur).unwrap_or_default()).map_err(|e| format!("保存失敗: {e}"))?;
    drop(g);
    Ok(get())
}

fn path_file() -> PathBuf { path() }

/// 設定画面向け: キーは末尾 4 桁だけ、旧ファイル由来のキーは "legacy" と印を付ける
pub fn masked() -> Value {
    let mut v = get();
    if let Some(keys) = v["keys"].as_object_mut() {
        for (name, val) in keys.iter_mut() {
            let s = val.as_str().unwrap_or("").to_string();
            *val = if !s.is_empty() {
                json!({"set": true, "tail": s.chars().rev().take(4).collect::<String>().chars().rev().collect::<String>(), "from": "config"})
            } else if let Some(l) = legacy(&format!("{name}_api_key")) {
                json!({"set": true, "tail": l.chars().rev().take(4).collect::<String>().chars().rev().collect::<String>(), "from": "legacy"})
            } else {
                json!({"set": false})
            };
        }
    }
    v
}

/// いま効いている環境変数の上書き(設定画面に「開発用の上書き中」と出す)
pub fn env_overrides() -> Value {
    let names = ["FG_GEN_BASE", "FG_GEN_PORT", "FG_SD_SERVER", "FG_VLM_BASE", "FG_VLM_PORT", "FG_LLAMA_SERVER", "FG_CACHE_MB", "PORT"];
    let mut o = serde_json::Map::new();
    for n in names {
        if let Ok(v) = std::env::var(n) { if !v.is_empty() { o.insert(n.into(), json!(v)); } }
    }
    Value::Object(o)
}

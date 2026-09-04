//! VLM属性付け — builtin(ollama qwen2.5vl)/claude/gpt。1枚ごとにサイドカー保存(いつ止めても無駄なし)。

use base64::Engine;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;

pub const OLLAMA: &str = "http://127.0.0.1:11434";
pub const BUILTIN_MODEL: &str = "qwen2.5vl:7b";

/// Mac 既定: アプリ同梱の llama-server(OpenAI 互換, 画像入力可)を内蔵 VLM として使う。
/// FG_VLM_BASE(例 http://127.0.0.1:8081/v1)が設定され /health が通れば ollama より優先。
pub fn local_vlm_base() -> Option<String> {
    std::env::var("FG_VLM_BASE").ok().filter(|s| !s.is_empty()).map(|s| s.trim_end_matches('/').to_string())
}
pub async fn local_vlm_ok(client: &reqwest::Client) -> bool {
    let Some(base) = local_vlm_base() else { return false };
    let health = base.trim_end_matches("/v1").to_string() + "/health";
    client.get(health).timeout(std::time::Duration::from_secs(2)).send().await.map(|r| r.status().is_success()).unwrap_or(false)
}
/// OpenAI 互換 chat/completions に画像+プロンプトを投げて JSON を返す(llama-server / LM Studio / vLLM 共通)
/// 属性付け JSON の形(llama-server の json_schema 制約用)。列挙値は PROMPT と同じ。
/// 実測: 制約なしだと 7B でも 3 割が caption だけ/暴走で壊れる → 制約ありで 30/30 が通る
pub fn enrich_schema() -> Value {
    json!({"type":"object","properties":{
        "caption":{"type":"string"},
        "tags":{"type":"array","items":{"type":"string"},"minItems":5,"maxItems":12},
        "attrs":{"type":"object","properties":{
            "scene":{"enum":["indoor","outdoor","studio","street","nature","abstract","other"]},
            "subject":{"enum":["person","face","animal","food","vehicle","building","object","landscape","text","other"]},
            "gender":{"enum":["male","female","mixed","none"]},
            "animal":{"enum":["dog","cat","bird","fish","horse","rabbit","reptile","insect","farm","wild","other","none"]},
            "people_count":{"enum":["0","1","2","group"]},
            "age_group":{"enum":["child","teen","adult","senior","none"]},
            "framing":{"enum":["closeup","upper_body","full_body","wide"]},
            "watermark":{"type":"boolean"},
            "lighting":{"enum":["daylight","night","indoor","studio","dramatic","flat","other"]},
            "style":{"enum":["photo","illustration","anime","3dcg","painting","sketch","other"]},
            "quality":{"type":"integer","minimum":1,"maximum":10},
            "nsfw":{"type":"boolean"}},
          "required":["scene","subject","gender","animal","people_count","age_group","framing","watermark","lighting","style","quality","nsfw"]}},
      "required":["caption","tags","attrs"]})
}
pub fn judge_schema() -> Value {
    json!({"type":"object","properties":{"match":{"type":"boolean"},"quality":{"type":"integer","minimum":1,"maximum":10}},"required":["match","quality"]})
}
pub async fn describe_openai_compat(client: &reqwest::Client, base: &str, model: &str, b64: &str, prompt: &str, temp: f64, max_tokens: u32, schema: Option<Value>) -> Result<String, String> {
    let mut body = json!({"model": model, "max_tokens": max_tokens, "temperature": temp,
            "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": format!("data:image/jpeg;base64,{b64}")}},
                {"type": "text", "text": prompt}]}]});
    if let Some(sc) = schema {
        body["response_format"] = json!({"type": "json_schema", "json_schema": {"name": "out", "schema": sc}});
    }
    let v: Value = client
        .post(format!("{base}/chat/completions"))
        .json(&body)
        .timeout(std::time::Duration::from_secs(300))
        .send().await.map_err(|e| format!("内蔵VLM(llama-server)接続失敗: {e}"))?
        .json().await.map_err(|e| e.to_string())?;
    v["choices"][0]["message"]["content"].as_str().map(str::to_string).ok_or_else(|| format!("応答無し: {}", v))
}

pub const PROMPT: &str = r#"Describe this image for a dataset library. Reply with ONLY a JSON object:
{"caption": "one sentence, concrete, in English",
 "tags": ["5-12 short lowercase tags"],
 "attrs": {"scene": "indoor|outdoor|studio|street|nature|abstract|other",
           "subject": "person|face|animal|food|vehicle|building|object|landscape|text|other",
           "gender": "male|female|mixed|none",
           "animal": "dog|cat|bird|fish|horse|rabbit|reptile|insect|farm|wild|other|none",
           "people_count": "0|1|2|group",
           "age_group": "child|teen|adult|senior|none",
           "framing": "closeup|upper_body|full_body|wide",
           "watermark": true|false,
           "lighting": "daylight|night|indoor|studio|dramatic|flat|other",
           "style": "photo|illustration|anime|3dcg|painting|sketch|other",
           "quality": 1-10, "nsfw": true|false}}
nsfw: true only for nudity, sexual content, or sexually explicit material. Swimwear, gravure-style, or revealing-but-clothed outfits are false.
gender/age_group: apparent, for the people in the image; "none" if no people.
animal: main animal type; "none" if no animal. Put the exact species/breed in tags (e.g. "shiba inu").
watermark: true if any watermark, stock-photo logo, brand logo, or overlaid text/caption is visible."#;

#[derive(Default)]
pub struct EnrichState {
    pub alive: AtomicBool,
    pub stop: AtomicBool,
    pub done: AtomicUsize,
    pub total: AtomicUsize,
    pub errors: AtomicUsize,
    pub backend: Mutex<String>,
    pub last: Mutex<String>,
    /// ユーザー優先: ここに未来時刻が入っている間、バックフィルは1件ごとに道を譲る
    /// (画像を開いた時の遅延エンリッチ等、人が待ってる仕事を先に通す)
    pub yield_until: Mutex<Option<std::time::Instant>>,
}

impl EnrichState {
    /// ユーザー起点の仕事が来た: n秒間バックフィルを待たせる
    pub fn user_priority(&self, secs: u64) {
        *self.yield_until.lock().unwrap() = Some(std::time::Instant::now() + std::time::Duration::from_secs(secs));
    }
    /// バックフィル側: 譲るべき間はここで待つ
    pub async fn wait_if_yielding(&self) {
        loop {
            let until = *self.yield_until.lock().unwrap();
            match until {
                Some(t) if t > std::time::Instant::now() => {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                _ => break,
            }
        }
    }
}

impl EnrichState {
    pub fn status(&self) -> Value {
        json!({
            "alive": self.alive.load(Relaxed), "done": self.done.load(Relaxed),
            "total": self.total.load(Relaxed), "errors": self.errors.load(Relaxed),
            "backend": self.backend.lock().unwrap().clone(),
            "last": self.last.lock().unwrap().clone(),
        })
    }
}

pub fn mlhub_key(name: &str) -> Option<String> {
    let p = dirs_home().join("ml-hub/config/settings.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
    v[name].as_str().map(str::to_string)
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var("HOME").map(Into::into).unwrap_or_else(|_| "/root".into())
}

fn parse_json(text: &str) -> Option<Value> {
    let t = text.trim();
    let t = if t.starts_with("```") {
        t.trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```")
    } else {
        t
    };
    let a = t.find('{')?;
    let b = t.rfind('}')?;
    serde_json::from_str(&t[a..=b]).ok()
}

pub async fn ensure_builtin(client: &reqwest::Client) -> Result<(), String> {
    if local_vlm_ok(client).await {
        return Ok(());
    }
    let tags: Value = client
        .get(format!("{OLLAMA}/api/tags"))
        .send()
        .await
        .map_err(|e| format!("ollamaに接続できません: {e}"))?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let have = tags["models"]
        .as_array()
        .map(|a| a.iter().any(|m| m["name"].as_str().unwrap_or("").starts_with("qwen2.5vl")))
        .unwrap_or(false);
    if !have {
        client
            .post(format!("{OLLAMA}/api/pull"))
            .json(&json!({"model": BUILTIN_MODEL, "stream": false}))
            .timeout(std::time::Duration::from_secs(1800))
            .send()
            .await
            .map_err(|e| format!("内蔵VLMのpull失敗: {e}"))?;
    }
    Ok(())
}

pub async fn describe(client: &reqwest::Client, img: &Path, backend: &str) -> Result<Value, String> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(std::fs::read(img).map_err(|e| e.to_string())?);
    for attempt in 0..2 {
        let r: Result<Value, String> = match backend {
            "builtin" if local_vlm_base().is_some() => async {
                let base = local_vlm_base().unwrap();
                let text = describe_openai_compat(client, &base, "vlm", &b64, PROMPT, 0.1 + attempt as f64 * 0.4, 700, Some(enrich_schema())).await?;
                parse_json(&text).ok_or_else(|| "JSON壊れ".into())
            }
            .await,
            "builtin" => async {
                let v: Value = client
                    .post(format!("{OLLAMA}/api/generate"))
                    .json(&json!({"model": BUILTIN_MODEL, "prompt": PROMPT, "images": [b64],
                                  "stream": false, "format": "json",
                                  "options": {"temperature": 0.1 + attempt as f64 * 0.4}}))
                    .timeout(std::time::Duration::from_secs(180))
                    .send()
                    .await
                    .map_err(|e| e.to_string())?
                    .json()
                    .await
                    .map_err(|e| e.to_string())?;
                parse_json(v["response"].as_str().unwrap_or("")).ok_or_else(|| "JSON壊れ".into())
            }
            .await,
            "claude" => async {
                let key = mlhub_key("anthropic_api_key").ok_or("anthropic_api_key未設定")?;
                let media = if img.extension().and_then(|e| e.to_str()) == Some("png") {
                    "image/png"
                } else {
                    "image/jpeg"
                };
                let v: Value = client
                    .post("https://api.anthropic.com/v1/messages")
                    .header("x-api-key", key)
                    .header("anthropic-version", "2023-06-01")
                    .json(&json!({"model": "claude-sonnet-5", "max_tokens": 700,
                        "messages": [{"role": "user", "content": [
                            {"type": "image", "source": {"type": "base64", "media_type": media, "data": b64}},
                            {"type": "text", "text": PROMPT}]}]}))
                    .send()
                    .await
                    .map_err(|e| e.to_string())?
                    .json()
                    .await
                    .map_err(|e| e.to_string())?;
                // thinkingブロック対策: type=="text" を選ぶ(教訓)
                let text = v["content"]
                    .as_array()
                    .and_then(|a| a.iter().find(|b| b["type"] == "text"))
                    .and_then(|b| b["text"].as_str())
                    .ok_or("textブロック無し")?;
                parse_json(text).ok_or_else(|| "JSON壊れ".into())
            }
            .await,
            "gpt" => async {
                let key = mlhub_key("openai_api_key").ok_or("openai_api_key未設定")?;
                let v: Value = client
                    .post("https://api.openai.com/v1/chat/completions")
                    .bearer_auth(key)
                    .json(&json!({"model": "gpt-5.2", "max_completion_tokens": 700,
                        "messages": [{"role": "user", "content": [
                            {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{b64}")}},
                            {"type": "text", "text": PROMPT}]}]}))
                    .send()
                    .await
                    .map_err(|e| e.to_string())?
                    .json()
                    .await
                    .map_err(|e| e.to_string())?;
                let text = v["choices"][0]["message"]["content"].as_str().ok_or("応答無し")?;
                parse_json(text).ok_or_else(|| "JSON壊れ".into())
            }
            .await,
            _ => Err(format!("unknown backend: {backend}")),
        };
        match r {
            Ok(v) => return Ok(v),
            Err(e) if attempt == 1 => return Err(e),
            Err(_) => continue,
        }
    }
    Err("unreachable".into())
}

//! LoRA 棚(G4, docs/gen-design.md §5) — `store/lora/<stem>.safetensors` + `<stem>.json`(名前/親モデル/トリガー語/出典/ライセンス)。
//! 取り込みは Hugging Face / Civitai の URL(親モデルを検問して台帳のモデルに紐づける)か、ファイルのアップロード。
//! 試し描き(probe)は内蔵の生成エンジンで 2 枚描いてカードの顔にする(`store/lora/previews/<stem>_N.jpg`)。
//! 生成側: sd-cli は `--lora-model-dir store/lora` + プロンプトの `<lora:stem:scale>`、sd-server は `lora:[{path,multiplier}]`(gen.rs)。
//! LoRA は親モデルごとに互換が無い(klein 4B 用は Z-Image に載らない)。Mac/CUDA どちらでも同じファイルが効く。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;

#[derive(Default)]
pub struct LoraState {
    pub importing: AtomicBool,
    pub name: Mutex<String>,
    pub got_mb: AtomicUsize,
    pub total_mb: AtomicUsize,
    pub last: Mutex<String>,
    pub probing: Mutex<String>, // 試し描き中の stem
}

impl LoraState {
    pub fn status(&self) -> Value {
        json!({"importing": self.importing.load(Relaxed), "name": self.name.lock().unwrap().clone(),
               "got_mb": self.got_mb.load(Relaxed), "total_mb": self.total_mb.load(Relaxed),
               "last": self.last.lock().unwrap().clone(), "probing": self.probing.lock().unwrap().clone()})
    }
}

pub fn dir(root: &Path) -> PathBuf { root.join("store/lora") }
pub fn previews_dir(root: &Path) -> PathBuf { dir(root).join("previews") }
pub fn file_path(root: &Path, stem: &str) -> PathBuf { dir(root).join(format!("{stem}.safetensors")) }
fn meta_path(root: &Path, stem: &str) -> PathBuf { dir(root).join(format!("{stem}.json")) }

/// ファイル名に使える形へ(sd-cli の `<lora:name:scale>` に入るので英数字と _- だけ)
pub fn safe_stem(name: &str) -> String {
    let s: String = name.trim().trim_end_matches(".safetensors").chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    let s = s.trim_matches('_').chars().take(60).collect::<String>();
    if s.is_empty() { "lora".into() } else { s }
}

/// 親モデルの当たり付け(Civitai の baseModel / HF のリポジトリ名 / ファイル名から)
pub fn base_from_text(s: &str) -> &'static str {
    let t = s.to_lowercase();
    if t.contains("klein") { return "flux2-klein-4b"; }
    if t.contains("z-image") || t.contains("z_image") || t.contains("zimage") || t.contains("z image") { return "z-image-turbo"; }
    if t.contains("qwen") { return "qwen-image"; }
    if t.contains("flux.2") || t.contains("flux2") || t.contains("flux-2") { return "flux2-dev"; }
    if t.contains("flux") { return "flux1"; }
    if t.contains("pony") || t.contains("illustrious") || t.contains("sdxl") || t.contains("xl") { return "sdxl"; }
    if t.contains("sd 1") || t.contains("sd1") || t.contains("1.5") { return "sd15"; }
    "unknown"
}
/// 台帳のどの生成モデルに載るか(空=載らない)
pub fn model_for_base(base: &str) -> Option<&'static str> {
    match base {
        "flux2-klein-4b" => Some("flux2-klein-4b"),
        "z-image-turbo" => Some("z-image-turbo"),
        "qwen-image" => Some("qwen-image-edit-2509"),
        "unknown" => Some("flux2-klein-4b"), // 分からない物は既定モデルで試し描きして確かめる
        _ => None,
    }
}

pub fn load_meta(root: &Path, stem: &str) -> Value {
    std::fs::read_to_string(meta_path(root, stem)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({}))
}
pub fn save_meta(root: &Path, stem: &str, m: &Value) {
    let _ = std::fs::create_dir_all(dir(root));
    let _ = std::fs::write(meta_path(root, stem), serde_json::to_string_pretty(m).unwrap_or_default());
}

/// 棚の一覧: 実ファイルが正本。json は付随情報。previews は試し描き/作例の枚数
pub fn list(root: &Path, albums: &[Value]) -> Vec<Value> {
    let mut out = vec![];
    let Ok(rd) = std::fs::read_dir(dir(root)) else { return out };
    let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().and_then(|x| x.to_str()) == Some("safetensors")).collect();
    files.sort();
    for f in files {
        let stem = f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let m = load_meta(root, &stem);
        let size_mb = f.metadata().map(|x| x.len() >> 20).unwrap_or(0);
        let previews: Vec<String> = (0..6).filter(|i| previews_dir(root).join(format!("{stem}_{i}.jpg")).exists()).map(|i| i.to_string()).collect();
        let art = previews_dir(root).join(format!("{stem}_art.jpg")).exists();
        let used_by: Vec<String> = albums.iter().filter(|a| a["recipe"]["lora"].as_array().map(|l| l.iter().any(|x| x["file"].as_str() == Some(stem.as_str()))).unwrap_or(false))
            .filter_map(|a| a["name"].as_str().map(String::from)).collect();
        let base = m["base"].as_str().map(String::from).unwrap_or_else(|| base_from_text(&stem).to_string());
        out.push(json!({
            "file": stem, "name": m["name"].as_str().unwrap_or(&stem), "base": base, "model": model_for_base(&base),
            "triggers": m["triggers"].as_array().cloned().unwrap_or_default(), "source": m["source"], "license": m["license"],
            "description": m["description"].as_str().unwrap_or("").chars().take(200).collect::<String>(),
            "size_mb": size_mb, "previews": previews, "art": art, "imported": m["imported"], "used_by": used_by,
        }));
    }
    out
}

pub fn delete(root: &Path, stem: &str) -> bool {
    let ok = std::fs::remove_file(file_path(root, stem)).is_ok();
    let _ = std::fs::remove_file(meta_path(root, stem));
    for i in 0..6 { let _ = std::fs::remove_file(previews_dir(root).join(format!("{stem}_{i}.jpg"))); }
    let _ = std::fs::remove_file(previews_dir(root).join(format!("{stem}_art.jpg")));
    ok
}

pub struct Resolved {
    pub download_url: String,
    pub file_name: String,
    pub name: String,
    pub base: String,
    pub triggers: Vec<String>,
    pub description: String,
    pub preview_url: Option<String>,
    pub license: String,
    pub source: String,
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c { '<' => in_tag = true, '>' => in_tag = false, _ if !in_tag => out.push(c), _ => {} }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// URL → ダウンロード先と付随情報。Hugging Face(リポジトリ or resolve 直リンク)/ Civitai(モデル or バージョン)/ 直リンク
pub async fn resolve(client: &reqwest::Client, url: &str, civitai_key: Option<&str>) -> Result<Resolved, String> {
    let u = url.trim();
    let to = std::time::Duration::from_secs(20);
    if let Some(rest) = u.strip_prefix("https://huggingface.co/").or_else(|| u.strip_prefix("https://hf.co/")) {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() < 2 { return Err("Hugging Face の URL は <user>/<repo> の形で".into()); }
        let repo = format!("{}/{}", parts[0], parts[1]);
        // 直リンク: .../resolve/<rev>/<path>
        if let Some(i) = parts.iter().position(|p| *p == "resolve" || *p == "blob") {
            if parts.len() > i + 2 {
                let path = parts[i + 2..].join("/");
                let dl = format!("https://huggingface.co/{repo}/resolve/{}/{path}", parts[i + 1]);
                let fname = path.rsplit('/').next().unwrap_or("lora.safetensors").to_string();
                return Ok(Resolved { download_url: dl, file_name: fname.clone(), name: parts[1].to_string(), base: base_from_text(&format!("{repo} {fname}")).into(),
                                     triggers: vec![], description: String::new(), preview_url: None, license: String::new(), source: u.to_string() });
            }
        }
        let api: Value = client.get(format!("https://huggingface.co/api/models/{repo}?blobs=true")).timeout(to).send().await
            .map_err(|e| format!("HF に繋がりません: {e}"))?.error_for_status().map_err(|e| format!("HF: {e}"))?
            .json().await.map_err(|e| format!("HF 応答壊れ: {e}"))?;
        let mut cands: Vec<(u64, String)> = api["siblings"].as_array().map(|a| a.iter().filter_map(|s| {
            let n = s["rfilename"].as_str()?; let sz = s["size"].as_u64().unwrap_or(0);
            (n.ends_with(".safetensors") && sz > 1_000_000 && sz < 3_000_000_000).then(|| (sz, n.to_string()))
        }).collect()).unwrap_or_default();
        if cands.is_empty() { return Err("このリポジトリに LoRA(.safetensors)が見つかりません".into()); }
        // lora らしい名前を優先、次に小さい順(重み本体を避ける)
        cands.sort_by_key(|(sz, n)| (if n.to_lowercase().contains("lora") { 0 } else { 1 }, *sz));
        let (_, fname) = cands.remove(0);
        let license = api["cardData"]["license"].as_str().unwrap_or("").to_string();
        let tags: Vec<String> = api["tags"].as_array().map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect()).unwrap_or_default();
        let base_hint = format!("{repo} {fname} {}", api["cardData"]["base_model"].as_str().unwrap_or(""));
        let base_hint = format!("{base_hint} {}", tags.iter().filter(|t| t.starts_with("base_model:")).cloned().collect::<Vec<_>>().join(" "));
        return Ok(Resolved { download_url: format!("https://huggingface.co/{repo}/resolve/main/{fname}"), file_name: fname.rsplit('/').next().unwrap_or("lora.safetensors").to_string(),
                             name: parts[1].to_string(), base: base_from_text(&base_hint).into(), triggers: api["cardData"]["instance_prompt"].as_str().map(|s| vec![s.to_string()]).unwrap_or_default(),
                             description: String::new(), preview_url: None, license, source: u.to_string() });
    }
    if u.contains("civitai.com/") {
        let vid = u.split("modelVersionId=").nth(1).and_then(|s| s.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<u64>().ok());
        let mid = u.split("/models/").nth(1).and_then(|s| s.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<u64>().ok());
        let req = |url: String| { let r = client.get(url).timeout(to); if let Some(k) = civitai_key { r.bearer_auth(k) } else { r } };
        let ver: Value = if let Some(v) = vid {
            req(format!("https://civitai.com/api/v1/model-versions/{v}")).send().await.map_err(|e| format!("Civitai に繋がりません: {e}"))?
                .error_for_status().map_err(|e| format!("Civitai: {e}"))?.json().await.map_err(|e| format!("Civitai 応答壊れ: {e}"))?
        } else if let Some(m) = mid {
            let model: Value = req(format!("https://civitai.com/api/v1/models/{m}")).send().await.map_err(|e| format!("Civitai に繋がりません: {e}"))?
                .error_for_status().map_err(|e| format!("Civitai: {e}"))?.json().await.map_err(|e| format!("Civitai 応答壊れ: {e}"))?;
            if model["type"].as_str().map(|t| !t.eq_ignore_ascii_case("LORA")).unwrap_or(false) {
                return Err(format!("LoRA ではありません(type={})", model["type"]));
            }
            let mut v = model["modelVersions"][0].clone();
            v["model"] = json!({"name": model["name"], "type": model["type"], "description": model["description"]});
            v
        } else {
            return Err("Civitai の URL は /models/<id> か modelVersionId=<id> を含む形で".into());
        };
        let files = ver["files"].as_array().cloned().unwrap_or_default();
        let f = files.iter().find(|f| f["type"].as_str() == Some("Model") && f["name"].as_str().map(|n| n.ends_with(".safetensors")).unwrap_or(false))
            .or_else(|| files.first()).ok_or("ダウンロードできるファイルがありません")?;
        let mut dl = f["downloadUrl"].as_str().unwrap_or("").to_string();
        if dl.is_empty() { dl = format!("https://civitai.com/api/download/models/{}", ver["id"]); }
        if let Some(k) = civitai_key { dl = format!("{dl}{}token={k}", if dl.contains('?') { "&" } else { "?" }); }
        let base_model = ver["baseModel"].as_str().unwrap_or("");
        let name = format!("{} {}", ver["model"]["name"].as_str().unwrap_or(""), ver["name"].as_str().unwrap_or("")).trim().to_string();
        return Ok(Resolved {
            download_url: dl, file_name: f["name"].as_str().unwrap_or("lora.safetensors").to_string(), name,
            base: base_from_text(&format!("{base_model} {}", ver["name"].as_str().unwrap_or(""))).into(),
            triggers: ver["trainedWords"].as_array().map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect()).unwrap_or_default(),
            description: strip_tags(ver["model"]["description"].as_str().or(ver["description"].as_str()).unwrap_or("")).chars().take(300).collect(),
            preview_url: ver["images"].as_array().and_then(|a| a.iter().find(|i| i["type"].as_str() != Some("video"))).and_then(|i| i["url"].as_str()).map(String::from),
            license: "civitai(作者の条件に従う)".into(), source: u.to_string(),
        });
    }
    if u.ends_with(".safetensors") {
        let fname = u.rsplit('/').next().unwrap_or("lora.safetensors").to_string();
        return Ok(Resolved { download_url: u.to_string(), file_name: fname.clone(), name: fname.trim_end_matches(".safetensors").into(), base: base_from_text(&fname).into(),
                             triggers: vec![], description: String::new(), preview_url: None, license: String::new(), source: u.to_string() });
    }
    Err("対応 URL: Hugging Face(リポジトリ/直リンク)、Civitai(モデル/バージョン)、.safetensors の直リンク".into())
}

async fn download(client: &reqwest::Client, url: &str, dst: &Path, st: &LoraState) -> Result<u64, String> {
    use std::io::Write;
    let _ = std::fs::create_dir_all(dst.parent().unwrap());
    let tmp = dst.with_extension("part");
    let mut resp = client.get(url).send().await.map_err(|e| format!("接続失敗: {e}"))?;
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        return Err(if code == 401 || code == 403 { "ダウンロードに認証が要ります(設定の API キーに Civitai のキーを入れてください)".into() } else { format!("HTTP {code}") });
    }
    let total = resp.content_length().unwrap_or(0);
    st.total_mb.store((total >> 20) as usize, Relaxed);
    st.got_mb.store(0, Relaxed);
    let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    let mut got: u64 = 0;
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("DL 中断: {e}"))? {
        f.write_all(&chunk).map_err(|e| e.to_string())?;
        got += chunk.len() as u64;
        st.got_mb.store((got >> 20) as usize, Relaxed);
    }
    drop(f);
    if got < 100_000 { let _ = std::fs::remove_file(&tmp); return Err("ファイルが小さすぎます(HTML が返った可能性)".into()); }
    std::fs::rename(&tmp, dst).map_err(|e| e.to_string())?;
    Ok(got)
}

/// URL から棚へ取り込む。戻り = 棚のエントリ名(stem)
pub async fn import_url(root: &Path, client: &reqwest::Client, st: &LoraState, url: &str) -> Result<String, String> {
    if st.importing.swap(true, Relaxed) { return Err("別の LoRA を取り込み中です".into()); }
    let r = async {
        let key = crate::config::key("civitai");
        let res = resolve(client, url, key.as_deref()).await?;
        if model_for_base(&res.base).is_none() {
            return Err(format!("親モデル「{}」は内蔵の生成モデルに載りません(対応: FLUX.2 klein 4B / Z-Image / Qwen-Image)", res.base));
        }
        // 棚の名前は人が読める方(リポジトリ名 / Civitai のモデル名)。直リンクはファイル名
        let mut stem = safe_stem(if res.name.trim().is_empty() { &res.file_name } else { &res.name });
        if file_path(root, &stem).exists() { stem = format!("{stem}_{}", now_secs() % 10000); }
        *st.name.lock().unwrap() = stem.clone();
        *st.last.lock().unwrap() = format!("取得中: {}", res.name);
        let bytes = download(client, &res.download_url, &file_path(root, &stem), st).await?;
        let meta = json!({"name": res.name, "base": res.base, "triggers": res.triggers, "source": res.source, "license": res.license,
                          "description": res.description, "imported": now_secs(), "bytes": bytes});
        save_meta(root, &stem, &meta);
        if let Some(pu) = res.preview_url {
            if let Ok(b) = client.get(&pu).timeout(std::time::Duration::from_secs(20)).send().await.and_then(|r| r.error_for_status()) {
                if let Ok(data) = b.bytes().await {
                    if let Ok(img) = image::load_from_memory(&data) {
                        let th = img.thumbnail(512, 512).into_rgb8();
                        let _ = std::fs::create_dir_all(previews_dir(root));
                        let mut buf = std::io::Cursor::new(Vec::new());
                        if image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85).encode_image(&th).is_ok() {
                            let _ = std::fs::write(previews_dir(root).join(format!("{stem}_art.jpg")), buf.get_ref());
                        }
                    }
                }
            }
        }
        *st.last.lock().unwrap() = format!("取り込み完了: {stem}");
        Ok(stem)
    }.await;
    st.importing.store(false, Relaxed);
    if let Err(e) = &r { *st.last.lock().unwrap() = format!("失敗: {e}"); }
    r
}

/// アップロード(D&D)。data は .safetensors の中身
pub fn import_bytes(root: &Path, file_name: &str, data: &[u8]) -> Result<String, String> {
    if data.len() < 100_000 { return Err("ファイルが小さすぎます".into()); }
    let mut stem = safe_stem(file_name);
    if file_path(root, &stem).exists() { stem = format!("{stem}_{}", now_secs() % 10000); }
    let _ = std::fs::create_dir_all(dir(root));
    std::fs::write(file_path(root, &stem), data).map_err(|e| e.to_string())?;
    let base = base_from_text(file_name);
    save_meta(root, &stem, &json!({"name": file_name.trim_end_matches(".safetensors"), "base": base, "triggers": [], "source": "upload",
                                    "license": "", "description": "", "imported": now_secs(), "bytes": data.len()}));
    Ok(stem)
}

/// 試し描きの題(トリガー語があれば先頭に。様式 LoRA でも被写体 LoRA でも効きが見えるように 2 題)
pub fn probe_prompts(triggers: &[String]) -> Vec<String> {
    let trig = triggers.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    let pre = if trig.is_empty() { String::new() } else { format!("{trig}, ") };
    vec![
        format!("{pre}a woman walking down a city street at dusk, medium shot"),
        format!("{pre}a shiba inu running on a grass field, wide shot"),
    ]
}

pub fn save_preview(root: &Path, stem: &str, i: usize, png: &[u8]) {
    if let Ok(img) = image::load_from_memory(png) {
        let th = img.thumbnail(640, 640).into_rgb8();
        let _ = std::fs::create_dir_all(previews_dir(root));
        let mut buf = std::io::Cursor::new(Vec::new());
        if image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 86).encode_image(&th).is_ok() {
            let _ = std::fs::write(previews_dir(root).join(format!("{stem}_{i}.jpg")), buf.get_ref());
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

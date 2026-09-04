//! AI生成フォルダ(G1/G2) — 収集(crawl.rs)と同じ器で「作りたい物」を書いて▶を押すと、内蔵の画像生成AIが
//! 棚の素材を量産する。設計は docs/gen-design.md。
//!
//! エンジン = stable-diffusion.cpp(MIT, ggml)。ローカルは **sd-cli を 1 枚ごとに起動**(途中経過を `--preview proj` で
//! 各ステップ書き出し、stderr の `N/M` で進捗。常駐しないので待機中のメモリ 7GB を持たない。モデルは OS の
//! ページキャッシュに残るので 2 枚目以降のロードは数秒)。別マシンは sd-server(FG_GEN_BASE / 設定 gen.base)の
//! ネイティブ非同期ジョブ API。sd-cli が無ければ同梱の sd-server を子プロセスで起動する(vlm.rs と同型)。
//! モデル台帳: klein 4B(既定、t2i+参照編集) / Z-Image Turbo(別の絵柄・文字) / Qwen-Image-Edit-2509(構図保持の編集)。
//! 流れ: 計画(内蔵LLM)→ 参照の束ね(画像/フォルダ/データセット)→ 生成 → pHash 近重複 → 内蔵VLM 目利き → 収蔵(source="gen:<album>")。
//! 蒸留モデルは txt_cfg=1.0 固定(既定 7.0 のままだと 2 倍遅い罠)。

use crate::{enrich, llm, store};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

// ---------- モデル台帳(全部 Apache-2.0 = ストア版でも同梱可) ----------
pub struct ModelFile {
    pub role: &'static str, // diff | vae | llm | mmproj
    pub file: &'static str,
    pub url: &'static str,
    pub bytes: u64,
}
pub struct ModelSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub license: &'static str,
    pub note: &'static str,
    pub files: &'static [ModelFile],
    pub steps: u32,
    pub cfg: f32,
    pub flow_shift: f32, // 0 = 指定なし
    pub refs: bool,      // 参照画像(-r)を受けるか
}
pub const MODELS: &[ModelSpec] = &[
    ModelSpec {
        id: "flux2-klein-4b", label: "FLUX.2 klein 4B(既定・速い・参照OK)", license: "apache-2.0",
        note: "文だけの生成も参照画像の編集も同じ重み。4 steps、1024² で約27秒/枚(M3 Ultra)",
        files: &[
            ModelFile { role: "diff", file: "flux-2-klein-4b-Q8_0.gguf", url: "https://huggingface.co/leejet/FLUX.2-klein-4B-GGUF/resolve/main/flux-2-klein-4b-Q8_0.gguf", bytes: 4_300_629_440 },
            ModelFile { role: "vae", file: "flux2-vae.safetensors", url: "https://huggingface.co/Comfy-Org/flux2-klein-4B/resolve/main/split_files/vae/flux2-vae.safetensors", bytes: 336_211_292 },
            ModelFile { role: "llm", file: "Qwen3-4B-Q4_K_M.gguf", url: "https://huggingface.co/unsloth/Qwen3-4B-GGUF/resolve/main/Qwen3-4B-Q4_K_M.gguf", bytes: 2_497_281_312 },
        ],
        steps: 4, cfg: 1.0, flow_shift: 0.0, refs: true,
    },
    ModelSpec {
        id: "z-image-turbo", label: "Z-Image Turbo(別の絵柄・文字描画)", license: "apache-2.0",
        note: "6B・8 steps。参照画像は使えない。テキストエンコーダは内蔵LLMと共有(追加DLは 6.9GB)",
        files: &[
            ModelFile { role: "diff", file: "z_image_turbo-Q8_0.gguf", url: "https://huggingface.co/leejet/Z-Image-Turbo-GGUF/resolve/main/z_image_turbo-Q8_0.gguf", bytes: 6_577_440_704 },
            ModelFile { role: "vae", file: "flux1-ae.safetensors", url: "https://huggingface.co/Comfy-Org/z_image_turbo/resolve/main/split_files/vae/ae.safetensors", bytes: 335_304_388 },
            ModelFile { role: "llm", file: llm::MODEL_FILE, url: llm::MODEL_URL, bytes: 2_497_281_120 },
        ],
        steps: 8, cfg: 1.0, flow_shift: 0.0, refs: false,
    },
    ModelSpec {
        id: "qwen-image-edit-2509", label: "Qwen-Image-Edit 2509(構図保持の編集・重い)", license: "apache-2.0",
        note: "20B。参照画像の構図と人物をよく保つ(旧 atelier の教師)。19.4GB、cfg 2.5 で 2 回走るため遅い",
        files: &[
            ModelFile { role: "diff", file: "Qwen-Image-Edit-2509-Q4_K_M.gguf", url: "https://huggingface.co/QuantStack/Qwen-Image-Edit-2509-GGUF/resolve/main/Qwen-Image-Edit-2509-Q4_K_M.gguf", bytes: 13_065_746_976 },
            ModelFile { role: "vae", file: "qwen_image_vae.safetensors", url: "https://huggingface.co/Comfy-Org/Qwen-Image_ComfyUI/resolve/main/split_files/vae/qwen_image_vae.safetensors", bytes: 253_806_246 },
            ModelFile { role: "llm", file: "Qwen2.5-VL-7B-Instruct-Q4_K_M.gguf", url: "https://huggingface.co/unsloth/Qwen2.5-VL-7B-Instruct-GGUF/resolve/main/Qwen2.5-VL-7B-Instruct-Q4_K_M.gguf", bytes: 4_683_072_384 },
            ModelFile { role: "mmproj", file: "Qwen2.5-VL-7B-mmproj-F16.gguf", url: "https://huggingface.co/unsloth/Qwen2.5-VL-7B-Instruct-GGUF/resolve/main/mmproj-F16.gguf", bytes: 1_354_163_040 },
        ],
        steps: 20, cfg: 2.5, flow_shift: 3.0, refs: true,
    },
];
pub const DEFAULT_MODEL: &str = "flux2-klein-4b";
pub const DEFAULT_PORT: u16 = 8092;
const PHASH_NEAR: u32 = 4; // 生成物同士の近重複(同じ seed 近傍・同じ構図)はこれ以下で捨てる

pub fn spec(id: &str) -> &'static ModelSpec {
    MODELS.iter().find(|m| m.id == id).unwrap_or(&MODELS[0])
}
pub fn default_model_id() -> String {
    crate::config::get_str("gen.model").filter(|m| MODELS.iter().any(|s| s.id == m)).unwrap_or_else(|| DEFAULT_MODEL.into())
}

#[derive(Default)]
pub struct GenState {
    // エンジン
    pub downloading: AtomicBool,
    pub got_mb: AtomicUsize,
    pub total_mb: AtomicUsize,
    pub dl_model: Mutex<String>,
    pub starting: AtomicBool,
    pub child: Mutex<Option<std::process::Child>>, // 同梱 sd-server(cli が無い時だけ)
    pub server_model: Mutex<String>,               // 同梱 sd-server に載っているモデル
    pub last_error: Mutex<String>,
    // ジョブ(1 本直列)
    pub alive: AtomicBool,
    pub stop: AtomicBool,
    pub album: Mutex<String>,
    pub model: Mutex<String>,
    pub provider: Mutex<String>,
    pub prompt: Mutex<String>, // いま描いているプロンプト(配役カード用)
    pub last: Mutex<String>,
    pub planned: AtomicUsize,
    pub generated: AtomicUsize,
    pub rejected: AtomicUsize,
    pub ingested: AtomicUsize,
    pub errors: AtomicUsize,
    pub step: AtomicUsize,     // いま描いている 1 枚の進捗(sd-cli の N/M)
    pub steps: AtomicUsize,
    pub ms_per: AtomicU64,     // 直近の生成時間(EMA, ms)
    pub started_at: AtomicU64, // unix 秒
    pub recent: Mutex<Vec<Value>>, // [{ok, r(sha|uk), why}] 最大 14
    pub ui_hot: AtomicU64,     // 最後に UI が画像/一覧を触った unix 秒(閲覧中は道を譲る)
}

impl GenState {
    pub fn ui_recent(&self, secs: u64) -> bool {
        now_secs().saturating_sub(self.ui_hot.load(Relaxed)) < secs
    }
    pub fn status(&self) -> Value {
        let generated = self.generated.load(Relaxed);
        let ingested = self.ingested.load(Relaxed);
        let started = self.started_at.load(Relaxed);
        json!({
            "alive": self.alive.load(Relaxed), "album": self.album.lock().unwrap().clone(),
            "model": self.model.lock().unwrap().clone(), "provider": self.provider.lock().unwrap().clone(),
            "prompt": self.prompt.lock().unwrap().clone(), "last": self.last.lock().unwrap().clone(),
            "planned": self.planned.load(Relaxed), "generated": generated,
            "rejected": self.rejected.load(Relaxed), "ingested": ingested, "errors": self.errors.load(Relaxed),
            "step": self.step.load(Relaxed), "steps": self.steps.load(Relaxed),
            "pass_rate": if generated > 0 { ingested as f64 / generated as f64 } else { 0.0 },
            "secs_per": self.ms_per.load(Relaxed) as f64 / 1000.0,
            "elapsed": if started > 0 && self.alive.load(Relaxed) { now_secs().saturating_sub(started) } else { 0 },
            "spent_usd": 0.0, // 内蔵は無料。外部プロバイダ(G5)がここに積む
            "recent": self.recent.lock().unwrap().clone(),
        })
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ---------- 置き場・在り処 ----------
pub fn models_dir(root: &Path) -> PathBuf { root.join("engine/models") }
pub fn lora_dir(root: &Path) -> PathBuf { root.join("store/lora") }
pub fn preview_path(root: &Path) -> PathBuf { root.join("engine/gen_preview.png") }
fn have(p: &Path, n: u64) -> bool { p.metadata().map(|m| m.len() == n).unwrap_or(false) }
fn file_path(root: &Path, f: &ModelFile) -> PathBuf { models_dir(root).join(f.file) }
fn role_path(root: &Path, s: &ModelSpec, role: &str) -> Option<PathBuf> {
    s.files.iter().find(|f| f.role == role).map(|f| file_path(root, f))
}
pub fn model_present(root: &Path, s: &ModelSpec) -> bool {
    s.files.iter().all(|f| have(&file_path(root, f), f.bytes))
}
pub fn models_present(root: &Path) -> bool { model_present(root, spec(&default_model_id())) }
pub fn port() -> u16 {
    std::env::var("FG_GEN_PORT").ok().and_then(|p| p.parse().ok())
        .or_else(|| crate::config::value("gen.port").as_u64().map(|p| p as u16)).unwrap_or(DEFAULT_PORT)
}
pub fn base_url() -> String { format!("http://127.0.0.1:{}", port()) }
/// 外部の sd-server(別マシンの CUDA 機など)。設定画面の gen.base、開発時は FG_GEN_BASE
pub fn external_base() -> Option<String> {
    crate::config::env_or("FG_GEN_BASE", "gen.base").map(|s| s.trim_end_matches('/').to_string())
}
pub fn preview_on() -> bool { crate::config::get_bool("gen.preview", true) }

fn find_bin(root: &Path, name: &str, env: &str, cfg: &str) -> Option<PathBuf> {
    if let Some(p) = crate::config::env_or(env, cfg) {
        let p = PathBuf::from(p);
        let p = if name == "sd-cli" && p.file_name().map(|n| n == "sd-server").unwrap_or(false) { p.with_file_name("sd-cli") } else { p };
        if p.exists() { return Some(p); }
    }
    let mut cands = vec![root.join("engine/bin").join(name)];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join(name));
            cands.push(dir.join("../Resources/sd").join(name));
            cands.push(dir.join("sd").join(name));
        }
    }
    cands.push(PathBuf::from("/opt/homebrew/bin").join(name));
    cands.push(PathBuf::from("/usr/local/bin").join(name));
    if let Ok(path) = std::env::var("PATH") {
        for d in path.split(':') { cands.push(Path::new(d).join(name)); }
    }
    cands.into_iter().find(|p| p.is_file())
}
/// sd-server の在り処(優先順): FG_SD_SERVER/設定 → root/engine/bin/ → 実行ファイルの隣 → .app の Resources/sd/ → /opt/homebrew/bin → PATH
pub fn server_bin(root: &Path) -> Option<PathBuf> { find_bin(root, "sd-server", "FG_SD_SERVER", "tools.sd_server") }
/// sd-cli(ローカル生成の本命: 途中経過が出せる)。設定が sd-server を指していれば隣の sd-cli
pub fn cli_bin(root: &Path) -> Option<PathBuf> { find_bin(root, "sd-cli", "FG_SD_SERVER", "tools.sd_server") }

pub async fn health(client: &reqwest::Client, base: &str) -> bool {
    client.get(format!("{base}/v1/models")).timeout(std::time::Duration::from_secs(2)).send().await
        .map(|r| r.status().is_success()).unwrap_or(false)
}

/// エンジンの準備状況(AI 配役の「生成」行・設定画面・生成パネルのモデル選択用)
pub fn engine_status(root: &Path, st: &GenState) -> Value {
    let running = st.child.lock().unwrap().as_mut().map(|c| c.try_wait().ok().flatten().is_none()).unwrap_or(false);
    let def = default_model_id();
    let d = spec(&def);
    json!({
        "default": def, "model": d.id, "license": d.license,
        "models": MODELS.iter().map(|s| json!({
            "id": s.id, "label": s.label, "license": s.license, "note": s.note, "refs": s.refs, "steps": s.steps,
            "present": model_present(root, s), "size_mb": s.files.iter().map(|f| f.bytes).sum::<u64>() >> 20,
            "files": s.files.iter().map(|f| json!({"file": f.file, "role": f.role, "size_mb": f.bytes >> 20, "present": have(&file_path(root, f), f.bytes)})).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "size_mb": d.files.iter().map(|f| f.bytes).sum::<u64>() >> 20,
        "present": model_present(root, d),
        "bin": server_bin(root).map(|p| p.display().to_string()), "cli": cli_bin(root).map(|p| p.display().to_string()),
        "preview": preview_on(),
        "downloading": st.downloading.load(Relaxed), "dl_model": st.dl_model.lock().unwrap().clone(),
        "got_mb": st.got_mb.load(Relaxed), "total_mb": st.total_mb.load(Relaxed),
        "starting": st.starting.load(Relaxed), "running": running, "server_model": st.server_model.lock().unwrap().clone(),
        "port": port(), "base": base_url(), "external": external_base(), "last_error": st.last_error.lock().unwrap().clone(),
    })
}

async fn download(client: &reqwest::Client, url: &str, p: &Path, expect: u64, st: &GenState) -> Result<(), String> {
    use std::io::Write;
    if have(p, expect) { return Ok(()); }
    std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
    let tmp = p.with_extension("part");
    let mut resp = client.get(url).send().await.map_err(|e| format!("生成モデル DL 接続失敗: {e}"))?
        .error_for_status().map_err(|e| format!("生成モデル DL 失敗: {e}"))?;
    let total = resp.content_length().unwrap_or(expect);
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
    if got != total {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("DL サイズ不一致({got}/{total})"));
    }
    std::fs::rename(&tmp, p).map_err(|e| e.to_string())?;
    println!("🪄 {} 取得完了({}MB)", p.file_name().unwrap().to_string_lossy(), got >> 20);
    Ok(())
}

/// モデル 1 式を初回だけ DL(小さい物から)。進捗は engine_status() に出る
pub async fn ensure_models(root: &Path, client: &reqwest::Client, st: &GenState, s: &ModelSpec) -> Result<(), String> {
    if model_present(root, s) { return Ok(()); }
    if st.downloading.swap(true, Relaxed) { return Err("生成モデルを DL 中です(もう少し待ってください)".into()); }
    *st.dl_model.lock().unwrap() = s.id.into();
    let r = async {
        let mut files: Vec<&ModelFile> = s.files.iter().collect();
        files.sort_by_key(|f| f.bytes);
        for f in files {
            download(client, f.url, &file_path(root, f), f.bytes, st).await?;
        }
        Ok(())
    }.await;
    st.downloading.store(false, Relaxed);
    r
}

/// sd-server を起動して /v1/models が通るまで待つ(sd-cli が無い環境の保険)。親が死んだら道連れ
pub async fn start_server(root: &Path, client: &reqwest::Client, st: &GenState, s: &ModelSpec) -> Result<String, String> {
    let base = base_url();
    if health(client, &base).await && *st.server_model.lock().unwrap() == s.id { return Ok(base); }
    stop_engine(st); // 別モデルが載っていたら入れ替える
    let bin = server_bin(root).ok_or_else(|| "sd-server が見つかりません(stable-diffusion.cpp の公式リリースを engine/bin/ か .app 同梱の Resources/sd に置く、または設定の外部ツール)".to_string())?;
    if !model_present(root, s) { return Err("生成モデル未取得(AI配役の「取得」で DL)".into()); }
    if st.starting.swap(true, Relaxed) { return Err("生成エンジン起動中です".into()); }
    let r: Result<String, String> = async {
        let log = root.join("engine/sd-server.log");
        let _ = std::fs::create_dir_all(lora_dir(root));
        let parent = std::process::id();
        let mut args = format!("--diffusion-model \"{}\" --vae \"{}\" --llm \"{}\"",
            role_path(root, s, "diff").unwrap().display(), role_path(root, s, "vae").unwrap().display(), role_path(root, s, "llm").unwrap().display());
        if let Some(mm) = role_path(root, s, "mmproj") { args += &format!(" --llm_vision \"{}\"", mm.display()); }
        if s.flow_shift > 0.0 { args += &format!(" --flow-shift {}", s.flow_shift); }
        let sh = format!(
            "\"{}\" {} --lora-model-dir \"{}\" --diffusion-fa --listen-ip 127.0.0.1 --listen-port {} >> \"{}\" 2>&1 & pid=$!; while kill -0 {} 2>/dev/null; do sleep 3; done; kill $pid 2>/dev/null",
            bin.display(), args, lora_dir(root).display(), port(), log.display(), parent);
        let child = std::process::Command::new("/bin/sh").arg("-c").arg(sh)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .spawn().map_err(|e| format!("sd-server 起動失敗: {e}"))?;
        *st.child.lock().unwrap() = Some(child);
        *st.server_model.lock().unwrap() = s.id.into();
        for _ in 0..180 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if health(client, &base).await {
                println!("🪄 生成エンジン(sd-server)稼働: {base} ({})", s.id);
                return Ok(base);
            }
            if let Some(c) = st.child.lock().unwrap().as_mut() {
                if c.try_wait().ok().flatten().is_some() { return Err("sd-server が終了しました(engine/sd-server.log を確認)".into()); }
            }
        }
        Err("sd-server の起動待ちがタイムアウト(180秒)".into())
    }.await;
    st.starting.store(false, Relaxed);
    if let Err(e) = &r { *st.last_error.lock().unwrap() = e.clone(); }
    r
}

pub fn stop_engine(st: &GenState) {
    if let Some(mut c) = st.child.lock().unwrap().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
    st.server_model.lock().unwrap().clear();
}

// ---------- 参照画像(画像/フォルダ/データセット) ----------
/// ▶ 時に main.rs が解決して渡す。fixed=毎回必ず付ける sha、pools=(候補 sha 群, 1 枚あたり k 枚)を毎回抽選
#[derive(Default, Clone)]
pub struct RefPool {
    pub fixed: Vec<String>,
    pub pools: Vec<(Vec<String>, usize)>,
    pub notes: Vec<String>, // 計画 LLM に見せる説明([REF: caption] 等)
}
impl RefPool {
    pub fn is_empty(&self) -> bool { self.fixed.is_empty() && self.pools.iter().all(|(v, _)| v.is_empty()) }
}

/// 参照 sha → 1024px JPEG の一時ファイル(engine/refs/<sha>.jpg にキャッシュ)
fn ref_file(root: &Path, sha: &str) -> Option<PathBuf> {
    let dir = root.join("engine/refs");
    let out = dir.join(format!("{sha}.jpg"));
    if out.exists() { return Some(out); }
    let m = store::load_meta(root, sha)?;
    let ext = m["ext"].as_str().unwrap_or("jpg");
    let img = image::open(store::image_path(root, sha, ext)).ok()?;
    let th = img.thumbnail(1024, 1024).into_rgb8();
    let _ = std::fs::create_dir_all(&dir);
    let mut buf = std::io::Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 92).encode_image(&th).ok()?;
    std::fs::write(&out, buf.get_ref()).ok()?;
    Some(out)
}
/// 参照画像の説明(サイドカーの VLM caption)。計画 LLM の主語合わせ用
pub fn ref_caption(root: &Path, sha: &str) -> String {
    store::load_meta(root, sha).and_then(|m| m["vlm"]["caption"].as_str().or(m["caption"].as_str()).map(String::from)).unwrap_or_default()
}

// ---------- 生成 ----------
pub struct GenJob {
    pub prompt: String,
    pub w: u32,
    pub h: u32,
    pub steps: u32,
    pub seed: u64,
    pub lora: Vec<(String, f32)>, // (store/lora の stem, 強さ)
}
/// sd-cli 用: プロンプト末尾に `<lora:stem:scale>` を付ける(--lora-model-dir が store/lora)
fn prompt_with_lora(job: &GenJob) -> String {
    let tags: String = job.lora.iter().map(|(f, s)| format!(" <lora:{f}:{s}>")).collect();
    format!("{}{}", job.prompt, tags)
}

fn parse_progress(s: &str) -> Option<(usize, usize)> {
    // sd-cli の進捗: "|=====>   | 2/4 - 2.79s/it"
    let mut out = None;
    for piece in s.split(|c| c == '\r' || c == '\n') {
        if let Some(i) = piece.find(" - ") {
            let head = piece[..i].trim_end();
            let tok = head.rsplit(|c: char| c.is_whitespace() || c == '|').next().unwrap_or("");
            if let Some((a, b)) = tok.split_once('/') {
                if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) { out = Some((a, b)); }
            }
        }
    }
    out
}

/// ローカル: sd-cli を 1 枚ごとに起動(途中経過 `--preview proj` → engine/gen_preview.png、進捗 N/M)
async fn generate_cli(root: &Path, cli: &Path, s: &ModelSpec, job: &GenJob, refs: &[PathBuf], stop: &AtomicBool, st: &GenState) -> Result<(Vec<u8>, f64), String> {
    use tokio::io::AsyncReadExt;
    let t0 = std::time::Instant::now();
    let out = root.join("engine/gen_out.png");
    let prev = preview_path(root);
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&prev);
    let _ = std::fs::create_dir_all(lora_dir(root));
    let mut c = tokio::process::Command::new(cli);
    c.arg("--diffusion-model").arg(role_path(root, s, "diff").unwrap())
        .arg("--vae").arg(role_path(root, s, "vae").unwrap())
        .arg("--llm").arg(role_path(root, s, "llm").unwrap());
    if let Some(mm) = role_path(root, s, "mmproj") { c.arg("--llm_vision").arg(mm); }
    c.arg("--lora-model-dir").arg(lora_dir(root))
        .arg("-p").arg(prompt_with_lora(job)).arg("-W").arg(job.w.to_string()).arg("-H").arg(job.h.to_string())
        .arg("--steps").arg(job.steps.to_string()).arg("--cfg-scale").arg(s.cfg.to_string())
        .arg("--sampling-method").arg("euler").arg("--diffusion-fa").arg("-s").arg(job.seed.to_string())
        .arg("-o").arg(&out);
    if s.flow_shift > 0.0 { c.arg("--flow-shift").arg(s.flow_shift.to_string()); }
    if preview_on() { c.arg("--preview").arg("proj").arg("--preview-path").arg(&prev).arg("--preview-interval").arg("1"); }
    for r in refs { c.arg("-r").arg(r); }
    c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    let mut child = c.spawn().map_err(|e| format!("sd-cli 起動失敗: {e}"))?;
    let mut err = child.stderr.take().unwrap();
    let mut tail: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    st.step.store(0, Relaxed);
    st.steps.store(job.steps as usize, Relaxed);
    loop {
        tokio::select! {
            n = err.read(&mut buf) => {
                match n {
                    Ok(0) => break,
                    Ok(n) => {
                        let s = String::from_utf8_lossy(&buf[..n]);
                        if let Some((a, b)) = parse_progress(&s) { st.step.store(a, Relaxed); st.steps.store(b, Relaxed); }
                        tail.extend_from_slice(&buf[..n]);
                        if tail.len() > 8000 { let cut = tail.len() - 8000; tail.drain(..cut); }
                    }
                    Err(_) => break,
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(300)) => {
                if stop.load(Relaxed) { let _ = child.kill().await; return Err("stopped".into()); }
            }
        }
    }
    let status = child.wait().await.map_err(|e| e.to_string())?;
    if stop.load(Relaxed) { return Err("stopped".into()); }
    if !status.success() {
        let t = String::from_utf8_lossy(&tail);
        let last = t.lines().rev().find(|l| l.contains("error") || l.contains("Error") || l.contains("failed")).unwrap_or("").chars().take(160).collect::<String>();
        return Err(format!("sd-cli 失敗({}): {last}", status.code().unwrap_or(-1)));
    }
    let png = std::fs::read(&out).map_err(|_| "sd-cli が出力を書きませんでした")?;
    Ok((png, t0.elapsed().as_secs_f64()))
}

/// capabilities の既定 sample_params を土台に steps と txt_cfg だけ上書き(蒸留モデルは cfg 1.0)
async fn sample_params(client: &reqwest::Client, base: &str, s: &ModelSpec, steps: u32) -> Value {
    static CACHE: Mutex<Option<(String, Value)>> = Mutex::new(None);
    let cached = CACHE.lock().unwrap().as_ref().filter(|(b, _)| b == base).map(|(_, v)| v.clone());
    let mut sp = match cached {
        Some(v) => v,
        None => {
            let v = client.get(format!("{base}/sdcpp/v1/capabilities")).timeout(std::time::Duration::from_secs(5))
                .send().await.ok().and_then(|r| r.error_for_status().ok());
            let v = match v { Some(r) => r.json::<Value>().await.unwrap_or(json!({})), None => json!({}) };
            let sp = v["defaults"]["sample_params"].clone();
            let sp = if sp.is_object() { sp } else { json!({"guidance": {}}) };
            *CACHE.lock().unwrap() = Some((base.to_string(), sp.clone()));
            sp
        }
    };
    sp["sample_steps"] = json!(steps);
    sp["sample_method"] = json!("euler");
    if !sp["guidance"].is_object() { sp["guidance"] = json!({}); }
    sp["guidance"]["txt_cfg"] = json!(s.cfg);
    if s.flow_shift > 0.0 { sp["flow_shift"] = json!(s.flow_shift); }
    sp
}

/// sd-server(別マシン / 同梱の保険): ネイティブ非同期ジョブ。参照は b64 同梱。stop でキャンセル
pub async fn generate_server(client: &reqwest::Client, base: &str, s: &ModelSpec, job: &GenJob, refs: &[PathBuf], stop: &AtomicBool) -> Result<(Vec<u8>, f64), String> {
    use base64::Engine;
    let t0 = std::time::Instant::now();
    let ref_b64: Vec<String> = refs.iter().filter_map(|p| std::fs::read(p).ok()).map(|b| base64::engine::general_purpose::STANDARD.encode(b)).collect();
    let mut body = json!({"prompt": job.prompt, "width": job.w, "height": job.h, "seed": job.seed,
                          "sample_params": sample_params(client, base, s, job.steps).await});
    if !ref_b64.is_empty() { body["ref_images"] = json!(ref_b64); }
    // sd-server はプロンプト埋め込みの <lora:> を受けない(api.md)。lora 配列で渡す(path はサーバ側の --lora-model-dir 相対)
    if !job.lora.is_empty() { body["lora"] = json!(job.lora.iter().map(|(f, s)| json!({"path": format!("{f}.safetensors"), "multiplier": s})).collect::<Vec<_>>()); }
    let sub: Value = client.post(format!("{base}/sdcpp/v1/img_gen")).json(&body)
        .timeout(std::time::Duration::from_secs(60)).send().await.map_err(|e| format!("生成依頼失敗: {e}"))?
        .error_for_status().map_err(|e| format!("生成依頼が拒否: {e}"))?
        .json().await.map_err(|e| format!("生成依頼の応答壊れ: {e}"))?;
    let id = sub["id"].as_str().ok_or_else(|| format!("ジョブ ID なし: {}", sub))?.to_string();
    for _ in 0..3600 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if stop.load(Relaxed) {
            let _ = client.post(format!("{base}/sdcpp/v1/jobs/{id}/cancel")).timeout(std::time::Duration::from_secs(5)).send().await;
            return Err("stopped".into());
        }
        let j: Value = match client.get(format!("{base}/sdcpp/v1/jobs/{id}")).timeout(std::time::Duration::from_secs(10)).send().await {
            Ok(r) => r.json().await.unwrap_or(json!({})),
            Err(_) => continue,
        };
        match j["status"].as_str().unwrap_or("") {
            "completed" | "done" | "succeeded" => {
                let b64 = j["result"]["images"][0]["b64_json"].as_str().ok_or("結果に画像なし")?;
                let png = base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| format!("b64 壊れ: {e}"))?;
                return Ok((png, t0.elapsed().as_secs_f64()));
            }
            "failed" | "cancelled" => return Err(format!("生成失敗: {}", j["error"].as_str().unwrap_or("?"))),
            _ => {}
        }
    }
    Err("生成がタイムアウト(30分)".into())
}

/// 1 枚だけ描く(LoRA の試し描き / 将来の「1 枚だけ」ボタン)。プロバイダ選択は run() と同じ
pub async fn generate_one(root: &Path, client: &reqwest::Client, st: &GenState, s: &ModelSpec, job: &GenJob, refs: &[PathBuf]) -> Result<Vec<u8>, String> {
    if let Some(b) = external_base() {
        if !health(client, &b).await { return Err(format!("外部の生成エンジン({b})に繋がりません")); }
        return generate_server(client, &b, s, job, refs, &st.stop).await.map(|(p, _)| p);
    }
    ensure_models(root, client, st, s).await?;
    if let Some(cli) = cli_bin(root) {
        return generate_cli(root, &cli, s, job, refs, &st.stop, st).await.map(|(p, _)| p);
    }
    let b = start_server(root, client, st, s).await?;
    generate_server(client, &b, s, job, refs, &st.stop).await.map(|(p, _)| p)
}

// ---------- 計画(目標文 → 英語プロンプト N 本) ----------
fn parse_prompts(text: &str) -> Vec<String> {
    let (Some(a), Some(b)) = (text.find('['), text.rfind(']')) else { return vec![] };
    serde_json::from_str::<Value>(&text[a..=b]).ok()
        .and_then(|v| v.as_array().map(|arr| arr.iter().filter_map(|x| x.as_str().map(|s| s.trim().to_string())).filter(|s| s.len() > 8).collect()))
        .unwrap_or_default()
}

/// 内蔵 LLM が多様なプロンプトを設計(atelier genraw の 24 本方式)。参照があれば「編集指示」の形で。失敗時は決定的テンプレ
pub async fn plan(root: &Path, client: &reqwest::Client, llm_st: &llm::LlmState, goal: &str, used: &[String], n: usize, ref_notes: &[String], triggers: &[String]) -> Vec<String> {
    let avoid: Vec<&String> = used.iter().rev().take(12).collect();
    let trig = triggers.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
    let lora_block = if trig.is_empty() { String::new() } else {
        format!("A LoRA (style/subject adapter) is attached. Its trigger words are: \"{trig}\". Start EVERY prompt with these trigger words verbatim, \
                 and let the LoRA decide the style: do not add 'photorealistic photograph' or other style words that fight it unless the goal asks.\n")
    };
    let ref_block = if ref_notes.is_empty() { String::new() } else {
        format!("REFERENCE IMAGES will be attached to the image model for every generation:\n{}\n\
                 Therefore write EDIT INSTRUCTIONS, not scene descriptions: each prompt must keep the main subject of the reference \
                 (same identity, breed, face, clothing, colors, materials) and change what the goal asks (scene, pose, lighting, \
                 composition, season, weather, background). Start each with an imperative such as 'Keep the same ... from the reference image and place it ...'. \
                 Never describe the subject as something else.\n",
                ref_notes.iter().enumerate().map(|(i, c)| format!("- [REF{}] {}", i + 1, c)).collect::<Vec<_>>().join("\n"))
    };
    let user = format!(
        "GOAL (may be Japanese): 「{goal}」\n{ref_block}\
         {lora_block}\
         Write {n} English text-to-image prompts for building an image DATASET for this goal.\n\
         Rules:\n\
         - Write entirely in English: translate every Japanese word in the goal (e.g. 柴犬 → shiba inu, 病斑 → disease lesion). No Japanese characters in the output.\n\
         - Each prompt is ONE sentence, concrete and visual. Vary them strongly: individual/variant of the subject, \
           composition (close-up to wide), viewpoint (eye level / top-down / low angle), lighting (day, night, backlight, artificial), \
           background/place, season/weather, action.\n\
         - If the goal wants photos (or does not say) and no LoRA is attached, use 'photorealistic photograph' wording and end with 'sharp focus, natural colors'. \
           If the goal explicitly wants illustration/anime/painting/sprite sheet/pixel art etc., use exactly that wording instead.\n\
         - Never name real people, brands, or copyrighted characters. People are unspecified people.\n\
         - Keep every constraint written in the goal (e.g. 'no people', 'must show the whole body').\n\
         {}\
         Reply with ONLY a JSON array of {n} strings.",
        if avoid.is_empty() { String::new() } else { format!("- Do NOT repeat these already used prompts: {avoid:?}\n") }
    );
    let mut out = match llm::chat_t(root, client, llm_st, "You design text-to-image prompts. Reply with ONLY a JSON array of strings.", &user, 1400, 0.7).await {
        Ok(t) => parse_prompts(&t),
        Err(e) => { println!("🪄 計画 LLM 不可({e}) — テンプレで続行"); vec![] }
    };
    out.retain(|p| !used.iter().any(|u| u.eq_ignore_ascii_case(p)));
    out.dedup();
    if out.is_empty() {
        let subj = goal.lines().next().unwrap_or(goal).trim();
        let vars = ["natural daylight", "at golden hour", "close-up, shallow depth of field", "wide shot showing the surroundings",
                    "indoors, soft window light", "on a rainy day", "at night with artificial lights", "top-down view",
                    "low angle view", "in winter", "in summer", "backlit by the sun"];
        out = vars.iter().filter(|v| !used.iter().any(|u| u.contains(*v)))
            .take(n).map(|v| if !trig.is_empty() {
                format!("{trig}, {subj}, {v}")
            } else if ref_notes.is_empty() {
                format!("photorealistic photograph of {subj}, {v}, sharp focus, natural colors")
            } else {
                format!("Keep the same subject from the reference image and show it {v}, {subj}, photorealistic, sharp focus")
            }).collect();
    }
    out.truncate(n.max(1));
    out
}

// ---------- 目利き(内蔵 VLM): 目標に合うか+生成物の破綻 ----------
fn judge_prompt(goal: &str) -> String {
    format!(
        "GOAL: {goal}\n\
         This image was AI-generated as a candidate sample for a training dataset.\n\
         Accept ONLY if it clearly depicts the goal AND is a clean usable sample: no deformed anatomy or extra/missing limbs, \
         no garbled or nonsense text, no heavy artifacts, no watermark, no collage/grid, and it respects every constraint in the goal.\n\
         Reply ONLY JSON: {{\"match\": true|false, \"quality\": 1-10}}"
    )
}

async fn judge(client: &reqwest::Client, img: &image::DynamicImage, goal: &str) -> Result<(bool, i64), String> {
    use base64::Engine;
    let th = img.thumbnail(896, 896).into_rgb8();
    let mut buf = std::io::Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85).encode_image(&th).map_err(|e| e.to_string())?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(buf.get_ref());
    let text = if let Some(base) = enrich::local_vlm_base() {
        enrich::describe_openai_compat(client, &base, "vlm", &b64, &judge_prompt(goal), 0.0, 200, Some(enrich::judge_schema())).await?
    } else {
        let v: Value = client.post(format!("{}/api/generate", enrich::OLLAMA))
            .json(&json!({"model": enrich::BUILTIN_MODEL, "prompt": judge_prompt(goal), "images": [b64],
                          "stream": false, "format": "json", "options": {"temperature": 0.0}}))
            .timeout(std::time::Duration::from_secs(120)).send().await.map_err(|e| e.to_string())?
            .json().await.map_err(|e| e.to_string())?;
        v["response"].as_str().unwrap_or("").to_string()
    };
    let (Some(a), Some(b)) = (text.find('{'), text.rfind('}')) else { return Err("judge JSON壊れ".into()) };
    let p: Value = serde_json::from_str(&text[a..=b]).map_err(|_| "judge JSON壊れ")?;
    Ok((p["match"].as_bool().unwrap_or(false), p["quality"].as_i64().unwrap_or(0)))
}

// ---------- 台帳(store/gen_ledger/<album>.json): 使ったプロンプトと成績 ----------
fn ledger_path(root: &Path, album: &str) -> PathBuf { root.join("store/gen_ledger").join(format!("{album}.json")) }
pub fn load_ledger(root: &Path, album: &str) -> Value {
    std::fs::read_to_string(ledger_path(root, album)).ok().and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({"prompts": []}))
}
fn save_ledger(root: &Path, album: &str, v: &Value) {
    let p = ledger_path(root, album);
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let _ = std::fs::write(p, serde_json::to_string_pretty(v).unwrap_or_default());
}

fn push_recent(st: &GenState, ok: bool, r: &str, why: &str) {
    let mut v = st.recent.lock().unwrap();
    v.insert(0, json!({"ok": ok, "r": r, "why": why}));
    v.truncate(14);
}

fn xorshift(seed: &mut u64) -> u64 {
    let mut x = *seed;
    x ^= x << 13; x ^= x >> 7; x ^= x << 17;
    *seed = x;
    x
}

pub struct Limits {
    pub max_n: usize,     // この枚数収蔵したら終了
    pub max_secs: u64,    // 実行時間上限
    pub w: u32,
    pub h: u32,
    pub steps: u32,       // 0 = モデルの既定
    pub min_quality: i64, // VLM 目利きの品質閾値(VLM が無ければ素通し)
}

// ---------- 本体 ----------
#[allow(clippy::too_many_arguments)]
pub async fn run(
    root: PathBuf,
    client: reqwest::Client,
    st: Arc<GenState>,
    llm_st: Arc<llm::LlmState>,
    enrich_st: Arc<enrich::EnrichState>,
    album: String,
    goal: String,
    model_id: String,
    refs: RefPool,
    lora: Vec<(String, f32)>,
    limits: Limits,
) {
    let started = std::time::Instant::now();
    let set_last = |m: String| *st.last.lock().unwrap() = m;
    let s = spec(&model_id);
    *st.model.lock().unwrap() = s.id.into();
    // プロバイダ: 外部 sd-server > ローカル sd-cli(途中経過あり) > 同梱 sd-server
    let external = external_base();
    let cli = if external.is_none() { cli_bin(&root) } else { None };
    let provider = if external.is_some() { "sdcpp" } else if cli.is_some() { "builtin-cli" } else { "builtin-server" };
    *st.provider.lock().unwrap() = provider.into();
    set_last(format!("生成エンジンを準備中…({})", if model_present(&root, s) { s.label } else { "初回はモデルの DL" }));
    let base: Option<String> = match (&external, &cli) {
        (Some(b), _) => {
            if !health(&client, b).await { set_last(format!("中止: 外部の生成エンジン({b})に繋がりません")); finish(&root, &st, &album); return; }
            Some(b.clone())
        }
        (None, Some(_)) => {
            if let Err(e) = ensure_models(&root, &client, &st, s).await { set_last(format!("中止: {e}")); finish(&root, &st, &album); return; }
            None
        }
        (None, None) => {
            match async { ensure_models(&root, &client, &st, s).await?; start_server(&root, &client, &st, s).await }.await {
                Ok(b) => Some(b),
                Err(e) => { set_last(format!("中止: 生成エンジン不可({e})")); finish(&root, &st, &album); return; }
            }
        }
    };
    if !refs.is_empty() && !s.refs {
        set_last(format!("注意: {} は参照画像を使えません — 文だけで生成します", s.label));
    }
    let db = rusqlite::Connection::open(root.join("store/index.sqlite")).unwrap();
    store::ensure_schema(&db);
    let mut phashes: Vec<String> = db
        .prepare("SELECT phash FROM images WHERE phash IS NOT NULL")
        .and_then(|mut sq| sq.query_map([], |r| r.get::<_, String>(0)).map(|rs| rs.filter_map(Result::ok).collect()))
        .unwrap_or_default();
    phashes.extend(store::never_again_phashes(&root)); // 捨てられた子の近傍は二度と作らない
    let mut ledger = load_ledger(&root, &album);
    if !ledger["prompts"].is_array() { ledger["prompts"] = json!([]); }
    let mut used: Vec<String> = ledger["prompts"].as_array().map(|a| a.iter().filter_map(|p| p["text"].as_str().map(String::from)).collect()).unwrap_or_default();
    // 目利き: 内蔵 VLM が居れば使う。居なければ pHash だけで収蔵(生成物は権利ゴミが無いので止めない=収集と違う点)
    let vlm_on = enrich::any_vlm_available(&client).await;
    if !vlm_on {
        set_last("目利き無し(内蔵VLM 未稼働): 近重複だけ弾いて収蔵します".into());
    }
    let ref_notes = if s.refs { refs.notes.clone() } else { vec![] };
    // LoRA のトリガー語(棚の json)。計画 LLM に「必ず先頭に置け」と渡す
    let triggers: Vec<String> = lora.iter().flat_map(|(f, _)| {
        crate::lora::load_meta(&root, f)["triggers"].as_array().map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect::<Vec<_>>()).unwrap_or_default()
    }).collect();
    let mut pool: Vec<String> = vec![];
    let mut consec_err = 0usize;
    let mut seed: u64 = now_secs() ^ 0x9E37_79B9_7F4A_7C15 ^ ((std::process::id() as u64) << 32);
    let per_plan = 8usize;
    let steps = if limits.steps == 0 { s.steps } else { limits.steps };
    'outer: loop {
        if st.stop.load(Relaxed) || started.elapsed().as_secs() > limits.max_secs || st.ingested.load(Relaxed) >= limits.max_n {
            break;
        }
        if consec_err >= 8 {
            set_last("連続失敗 8 回で自動停止(engine/sd-server.log か稼働ボードを確認)".into());
            break;
        }
        if pool.is_empty() {
            set_last("プロンプトを設計中…(内蔵LLM)".into());
            pool = plan(&root, &client, &llm_st, &goal, &used, per_plan, &ref_notes, &triggers).await;
            st.planned.fetch_add(pool.len(), Relaxed);
            for p in &pool {
                ledger["prompts"].as_array_mut().unwrap().push(json!({"text": p, "ok": 0, "ng": 0, "ts": now_secs()}));
                used.push(p.clone());
            }
            save_ledger(&root, &album, &ledger);
            if pool.is_empty() {
                set_last("プロンプトが作れませんでした".into());
                break;
            }
        }
        let prompt = pool.remove(0);
        // 同じプロンプトでも seed と参照の抽選で別物になる=1 プロンプト最大 2 枚まで描く
        for _ in 0..2 {
            if st.stop.load(Relaxed) || st.ingested.load(Relaxed) >= limits.max_n || started.elapsed().as_secs() > limits.max_secs {
                break 'outer;
            }
            // 閲覧中は道を譲る(GPU を取り合わない)。夜間の量産では誰も触らないので止まらない
            enrich_st.user_priority(10);
            while st.ui_recent(8) && !st.stop.load(Relaxed) {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            // 参照の束ね: 固定 + 各フォルダ/データセットから k 枚抽選(モデルが参照非対応なら空)
            let mut ref_shas: Vec<String> = vec![];
            if s.refs {
                ref_shas.extend(refs.fixed.iter().cloned());
                for (cands, k) in &refs.pools {
                    let mut c = cands.clone();
                    for _ in 0..(*k).min(c.len()) {
                        let i = (xorshift(&mut seed) as usize) % c.len();
                        ref_shas.push(c.swap_remove(i));
                    }
                }
                ref_shas.truncate(4);
            }
            let ref_paths: Vec<PathBuf> = ref_shas.iter().filter_map(|sha| ref_file(&root, sha)).collect();
            *st.prompt.lock().unwrap() = prompt.clone();
            set_last(format!("生成中: {}", prompt.chars().take(80).collect::<String>()));
            let job = GenJob { prompt: prompt.clone(), w: limits.w, h: limits.h, steps, seed: xorshift(&mut seed) % 4_000_000_000, lora: lora.clone() };
            let r = match (&base, &cli) {
                (Some(b), _) => generate_server(&client, b, s, &job, &ref_paths, &st.stop).await,
                (None, Some(c)) => generate_cli(&root, c, s, &job, &ref_paths, &st.stop, &st).await,
                (None, None) => Err("生成手段なし".into()),
            };
            let (png, secs) = match r {
                Ok(v) => v,
                Err(e) if e == "stopped" => break 'outer,
                Err(e) => {
                    st.errors.fetch_add(1, Relaxed);
                    consec_err += 1;
                    set_last(format!("生成エラー: {e}"));
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    continue;
                }
            };
            consec_err = 0;
            st.generated.fetch_add(1, Relaxed);
            let prev = st.ms_per.load(Relaxed);
            let ms = (secs * 1000.0) as u64;
            st.ms_per.store(if prev == 0 { ms } else { (prev * 7 + ms * 3) / 10 }, Relaxed);
            let Ok(img) = image::load_from_memory(&png) else {
                st.errors.fetch_add(1, Relaxed);
                set_last("生成物のデコード失敗".into());
                continue;
            };
            let uk = format!("gen_{:08x}", xorshift(&mut seed) as u32);
            let ph = store::phash64(&img);
            if phashes.iter().any(|p| crate::crawl::hamming(p, &ph) <= PHASH_NEAR) {
                st.rejected.fetch_add(1, Relaxed);
                crate::crawl::save_reject_thumb(&root, &uk, &img);
                push_recent(&st, false, &uk, "そっくり(既存・近重複)");
                ledger_mark(&mut ledger, &prompt, false);
                continue;
            }
            let (gate, quality) = if vlm_on {
                set_last(format!("目利き中: {}", prompt.chars().take(60).collect::<String>()));
                match judge(&client, &img, &goal).await {
                    Ok((true, q)) if q >= limits.min_quality => ("vlm", q),
                    Ok((m, q)) => {
                        st.rejected.fetch_add(1, Relaxed);
                        crate::crawl::save_reject_thumb(&root, &uk, &img);
                        let why = if m { format!("品質 q{q} < {}", limits.min_quality) } else { "目標に合わない/破綻(内蔵VLM)".to_string() };
                        push_recent(&st, false, &uk, &why);
                        ledger_mark(&mut ledger, &prompt, false);
                        continue;
                    }
                    Err(e) => { set_last(format!("目利き不可({e}) — 近重複だけで収蔵")); ("none", 0) }
                }
            } else { ("none", 0) };
            let extra = json!({
                "rights": format!("generated:{}", s.license),
                "gen": {"provider": provider, "model": s.id, "file": role_path(&root, s, "diff").and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
                        "prompt": prompt, "seed": job.seed, "steps": job.steps, "cfg": s.cfg, "w": job.w, "h": job.h,
                        "refs": ref_shas, "lora": lora.iter().map(|(f, s)| json!({"file": f, "scale": s})).collect::<Vec<_>>(),
                        "secs": (secs * 10.0).round() / 10.0, "gate": gate, "quality": quality, "album": album},
                "cost": {"usd": 0.0, "by": provider},
                "quality": if quality > 0 { json!(quality) } else { Value::Null },
            });
            match store::ingest_bytes(&root, &db, &png, "png", &format!("gen:{album}"), &extra) {
                Ok(sha) => {
                    phashes.push(ph);
                    st.ingested.fetch_add(1, Relaxed);
                    push_recent(&st, true, &sha, &format!("採用 {:.0}秒{}", secs, if quality > 0 { format!(" q{quality}") } else { String::new() }));
                    ledger_mark(&mut ledger, &prompt, true);
                }
                Err(e) => {
                    st.rejected.fetch_add(1, Relaxed);
                    push_recent(&st, false, &uk, &format!("収蔵できず({e})"));
                }
            }
            save_ledger(&root, &album, &ledger);
        }
    }
    save_ledger(&root, &album, &ledger);
    let n = st.ingested.load(Relaxed);
    set_last(format!("おわり: {n}枚収蔵 / 生成{} / 却下{} ({:.0}秒/枚)", st.generated.load(Relaxed), st.rejected.load(Relaxed), st.ms_per.load(Relaxed) as f64 / 1000.0));
    println!("🪄 生成おわり: {album} +{n}枚");
    finish(&root, &st, &album);
}

fn ledger_mark(ledger: &mut Value, prompt: &str, ok: bool) {
    if let Some(arr) = ledger["prompts"].as_array_mut() {
        if let Some(p) = arr.iter_mut().find(|p| p["text"].as_str() == Some(prompt)) {
            let k = if ok { "ok" } else { "ng" };
            p[k] = json!(p[k].as_u64().unwrap_or(0) + 1);
        }
    }
}

/// アルバムに last_run を刻んで alive を下ろす(エージェントの調子が見える)
fn finish(root: &Path, st: &GenState, album: &str) {
    st.alive.store(false, Relaxed); // last_run に alive:true が残らないように先に下ろす
    let _ = std::fs::remove_file(preview_path(root));
    let ap = root.join("store/albums").join(format!("{album}.json"));
    if let Ok(t) = std::fs::read_to_string(&ap) {
        if let Ok(mut a) = serde_json::from_str::<Value>(&t) {
            a["last_run"] = st.status();
            a["last_run"]["ts"] = json!(now_secs() as f64);
            a["last_run"]["kind"] = json!("gen");
            let _ = std::fs::write(&ap, serde_json::to_string_pretty(&a).unwrap_or_default());
        }
    }
    *st.prompt.lock().unwrap() = String::new();
    st.step.store(0, Relaxed);
}

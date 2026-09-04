//! 内蔵VLM — llama.cpp の llama-server を子プロセスとして持ち、Qwen3-VL-4B(Apache-2.0)を初回自動DL。
//! アプリからは OpenAI 互換(画像入力 + json_schema)で叩く(enrich.rs describe_openai_compat)。
//! Mac 既定はこれ。ollama は「あれば第2候補」。Linux でも CUDA ビルドの llama-server を同じ GGUF で使える。
//!
//! 実測(M3 Ultra, COCO 30枚, 画像先+schema): JSON 30/30, people 26/30, animal 30/30, 1.8s/枚。
//! 2B は 1.25s/枚だが精度が落ち、Qwen2.5-VL-7B は 2.5s/枚で 4B と同等 → 既定は 4B。
//! 制約(json_schema)なしだと 7B でも 3 割が caption だけ/暴走で壊れる → 必ず schema 付き。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;

pub const MODEL_FILE: &str = "Qwen3VL-4B-Instruct-Q4_K_M.gguf";
pub const MODEL_URL: &str =
    "https://huggingface.co/Qwen/Qwen3-VL-4B-Instruct-GGUF/resolve/main/Qwen3VL-4B-Instruct-Q4_K_M.gguf";
const MODEL_BYTES: u64 = 2_497_281_664;
pub const MMPROJ_FILE: &str = "mmproj-Qwen3VL-4B-Instruct-F16.gguf";
pub const MMPROJ_URL: &str =
    "https://huggingface.co/Qwen/Qwen3-VL-4B-Instruct-GGUF/resolve/main/mmproj-Qwen3VL-4B-Instruct-F16.gguf";
const MMPROJ_BYTES: u64 = 836_180_256;
pub const DEFAULT_PORT: u16 = 8081;

#[derive(Default)]
pub struct VlmState {
    pub downloading: AtomicBool,
    pub got_mb: AtomicUsize,
    pub total_mb: AtomicUsize,
    pub starting: AtomicBool,
    pub child: Mutex<Option<std::process::Child>>,
    pub last_error: Mutex<String>,
}

pub fn model_path(root: &Path) -> PathBuf { root.join("engine/models").join(MODEL_FILE) }
pub fn mmproj_path(root: &Path) -> PathBuf { root.join("engine/models").join(MMPROJ_FILE) }
pub fn models_present(root: &Path) -> bool {
    let ok = |p: PathBuf, n: u64| p.metadata().map(|m| m.len() == n).unwrap_or(false);
    ok(model_path(root), MODEL_BYTES) && ok(mmproj_path(root), MMPROJ_BYTES)
}
pub fn port() -> u16 {
    std::env::var("FG_VLM_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT)
}
pub fn base_url() -> String { format!("http://127.0.0.1:{}/v1", port()) }

/// llama-server の在り処(優先順): FG_LLAMA_SERVER → root/engine/bin/ → 実行ファイルの隣(Tauri サイドカー)
/// → .app の Resources/llama/ → /opt/homebrew/bin → PATH
pub fn server_bin(root: &Path) -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FG_LLAMA_SERVER") {
        let p = PathBuf::from(p);
        if p.exists() { return Some(p); }
    }
    let mut cands = vec![root.join("engine/bin/llama-server")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join("llama-server"));
            cands.push(dir.join("../Resources/llama/llama-server"));
            cands.push(dir.join("llama/llama-server"));
        }
    }
    cands.push(PathBuf::from("/opt/homebrew/bin/llama-server"));
    cands.push(PathBuf::from("/usr/local/bin/llama-server"));
    if let Ok(path) = std::env::var("PATH") {
        for d in path.split(':') { cands.push(Path::new(d).join("llama-server")); }
    }
    cands.into_iter().find(|p| p.is_file())
}

pub async fn health(client: &reqwest::Client) -> bool {
    let url = format!("http://127.0.0.1:{}/health", port());
    client.get(url).timeout(std::time::Duration::from_secs(2)).send().await
        .map(|r| r.status().is_success()).unwrap_or(false)
}

pub fn status(root: &Path, st: &VlmState) -> Value {
    let running = st.child.lock().unwrap().as_mut().map(|c| c.try_wait().ok().flatten().is_none()).unwrap_or(false);
    json!({
        "model": MODEL_FILE, "mmproj": MMPROJ_FILE, "size_mb": (MODEL_BYTES + MMPROJ_BYTES) >> 20,
        "present": models_present(root), "bin": server_bin(root).map(|p| p.display().to_string()),
        "downloading": st.downloading.load(Relaxed), "got_mb": st.got_mb.load(Relaxed), "total_mb": st.total_mb.load(Relaxed),
        "starting": st.starting.load(Relaxed), "running": running, "port": port(), "base": base_url(),
        "external": std::env::var("FG_VLM_BASE").ok().filter(|s| !s.is_empty()),
        "last_error": st.last_error.lock().unwrap().clone(),
    })
}

async fn download(client: &reqwest::Client, url: &str, p: &Path, expect: u64, st: &VlmState) -> Result<(), String> {
    use std::io::Write;
    if p.metadata().map(|m| m.len() == expect).unwrap_or(false) { return Ok(()); }
    std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
    let tmp = p.with_extension("part");
    let mut resp = client.get(url).send().await.map_err(|e| format!("VLM DL接続失敗: {e}"))?
        .error_for_status().map_err(|e| format!("VLM DL失敗: {e}"))?;
    let total = resp.content_length().unwrap_or(expect);
    st.total_mb.store((total >> 20) as usize, Relaxed);
    st.got_mb.store(0, Relaxed);
    let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    let mut got: u64 = 0;
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("DL中断: {e}"))? {
        f.write_all(&chunk).map_err(|e| e.to_string())?;
        got += chunk.len() as u64;
        st.got_mb.store((got >> 20) as usize, Relaxed);
    }
    drop(f);
    if got != total {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("DLサイズ不一致({got}/{total})"));
    }
    std::fs::rename(&tmp, p).map_err(|e| e.to_string())?;
    println!("👁 {} 取得完了({}MB)", p.file_name().unwrap().to_string_lossy(), got >> 20);
    Ok(())
}

/// モデル2本を初回だけDL(3.3GB)。進捗は status() に出る
pub async fn ensure_models(root: &Path, client: &reqwest::Client, st: &VlmState) -> Result<(), String> {
    if models_present(root) { return Ok(()); }
    if st.downloading.swap(true, Relaxed) { return Err("内蔵VLMをDL中です(もう少し待ってください)".into()); }
    let r = async {
        download(client, MODEL_URL, &model_path(root), MODEL_BYTES, st).await?;
        download(client, MMPROJ_URL, &mmproj_path(root), MMPROJ_BYTES, st).await
    }.await;
    st.downloading.store(false, Relaxed);
    r
}

/// 子プロセスを起動して /health が通るまで待つ。親が死んだら道連れにする(sh の見張りで PPID を監視)
pub async fn start(root: &Path, client: &reqwest::Client, st: &VlmState) -> Result<String, String> {
    if health(client).await { return Ok(base_url()); }
    let bin = server_bin(root).ok_or_else(|| "llama-server が見つかりません(Mac: brew install llama.cpp か、.app 同梱の Resources/llama、または FG_LLAMA_SERVER=パス)".to_string())?;
    if !models_present(root) { return Err("内蔵VLMのモデル未取得(AI配役の「取得」で 3.3GB をDL)".into()); }
    if st.starting.swap(true, Relaxed) { return Err("内蔵VLM起動中です".into()); }
    let r: Result<String, String> = async {
        let log = root.join("engine/llama-server.log");
        let parent = std::process::id();
        let sh = format!(
            "\"{}\" -m \"{}\" --mmproj \"{}\" -ngl 99 -c 8192 --port {} --host 127.0.0.1 -a vlm --temp 0.1 --no-webui >> \"{}\" 2>&1 & pid=$!; while kill -0 {} 2>/dev/null; do sleep 3; done; kill $pid 2>/dev/null",
            bin.display(), model_path(root).display(), mmproj_path(root).display(), port(), log.display(), parent);
        let child = std::process::Command::new("/bin/sh").arg("-c").arg(sh)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .spawn().map_err(|e| format!("llama-server 起動失敗: {e}"))?;
        *st.child.lock().unwrap() = Some(child);
        for _ in 0..180 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if health(client).await {
                println!("👁 内蔵VLM 稼働: {} ({})", base_url(), MODEL_FILE);
                return Ok(base_url());
            }
            if let Some(c) = st.child.lock().unwrap().as_mut() {
                if c.try_wait().ok().flatten().is_some() { return Err("llama-server が終了しました(engine/llama-server.log を確認)".into()); }
            }
        }
        Err("llama-server の起動待ちがタイムアウト(180秒)".into())
    }.await;
    st.starting.store(false, Relaxed);
    if let Err(e) = &r { *st.last_error.lock().unwrap() = e.clone(); }
    r
}

/// 属性付け/目利きが使う内蔵VLMの OpenAI 互換 base を返す。外部指定(FG_VLM_BASE)が最優先、次に同梱の子プロセス
pub async fn ensure(root: &Path, client: &reqwest::Client, st: &VlmState) -> Result<String, String> {
    if let Ok(b) = std::env::var("FG_VLM_BASE") {
        if !b.is_empty() { return Ok(b.trim_end_matches('/').to_string()); }
    }
    if health(client).await { return Ok(base_url()); }
    server_bin(root).ok_or_else(|| "llama-server 無し".to_string())?;
    ensure_models(root, client, st).await?;
    start(root, client, st).await
}

pub fn stop(st: &VlmState) {
    if let Some(mut c) = st.child.lock().unwrap().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

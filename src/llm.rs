//! 本当の内蔵LLM — llama.cppをバイナリに直リンクし、GGUFを初回自動DL。ollama不要・API代ゼロ。
//! モデル=Qwen3-4B-Instruct(Apache 2.0/日英強い/2.4GB)。CUDAがあれば全層GPU、無ければCPU。
//! 推論は専用スレッド1本(モデル常駐・リクエストはチャネル直列) — Send境界も同時実行も問題にしない。
//! 使い分けの思想: 日常の生成(クエリ/翻訳/整理)は内蔵で無料、難しい計画だけClaude/GPT(コスト階段)。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::OnceLock;

pub const MODEL_FILE: &str = "Qwen3-4B-Instruct-2507-Q4_K_M.gguf";
pub const MODEL_URL: &str =
    "https://huggingface.co/unsloth/Qwen3-4B-Instruct-2507-GGUF/resolve/main/Qwen3-4B-Instruct-2507-Q4_K_M.gguf";
const MODEL_BYTES: u64 = 2_497_281_120; // 途中で切れたDLの検出用(サイズ一致で完了扱い)

#[derive(Default)]
pub struct LlmState {
    pub downloading: AtomicBool,
    pub got_mb: AtomicUsize,
    pub total_mb: AtomicUsize,
    pub ready: AtomicBool, // モデルがロード済み(1回でも生成に成功)
    pub busy: AtomicBool,
}

impl LlmState {
    pub fn status(&self, root: &Path) -> Value {
        json!({
            "model": MODEL_FILE,
            "present": model_path(root).exists(),
            "downloading": self.downloading.load(Relaxed),
            "got_mb": self.got_mb.load(Relaxed), "total_mb": self.total_mb.load(Relaxed),
            "ready": self.ready.load(Relaxed), "busy": self.busy.load(Relaxed),
        })
    }
}

pub fn model_path(root: &Path) -> PathBuf {
    root.join("engine/models").join(MODEL_FILE)
}

/// GGUFを初回だけDL(ストリーミング+進捗+.part→rename)。既にあれば即Ok。
pub async fn ensure_model(root: &Path, client: &reqwest::Client, st: &LlmState) -> Result<PathBuf, String> {
    let p = model_path(root);
    if p.exists() && p.metadata().map(|m| m.len() == MODEL_BYTES).unwrap_or(false) {
        return Ok(p);
    }
    if st.downloading.swap(true, Relaxed) {
        return Err("モデルDL中です(もう少し待ってください)".into());
    }
    let r = download(root, client, st, &p).await;
    st.downloading.store(false, Relaxed);
    r
}

async fn download(root: &Path, client: &reqwest::Client, st: &LlmState, p: &Path) -> Result<PathBuf, String> {
    use std::io::Write;
    std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
    let tmp = root.join("engine/models").join(format!("{MODEL_FILE}.part"));
    let mut resp = client
        .get(MODEL_URL)
        .send()
        .await
        .map_err(|e| format!("モデルDL接続失敗: {e}"))?
        .error_for_status()
        .map_err(|e| format!("モデルDL失敗: {e}"))?;
    let total = resp.content_length().unwrap_or(MODEL_BYTES);
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
    Ok(p.to_path_buf())
}

// ---------- 推論エンジン(専用スレッド・モデル常駐) ----------

struct Req {
    prompt: String,
    max_tokens: usize,
    temp: f32,
    resp: tokio::sync::oneshot::Sender<Result<String, String>>,
}

static TX: OnceLock<std::sync::mpsc::Sender<Req>> = OnceLock::new();

fn engine_tx(root: PathBuf) -> std::sync::mpsc::Sender<Req> {
    TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Req>();
        std::thread::spawn(move || engine_thread(root, rx));
        tx
    })
    .clone()
}

#[allow(deprecated)] // token_to_str/Special: 上流の移行先APIが安定したら追従
fn engine_thread(root: PathBuf, rx: std::sync::mpsc::Receiver<Req>) {
    use llama_cpp_2::context::params::LlamaContextParams;
    use llama_cpp_2::llama_backend::LlamaBackend;
    use llama_cpp_2::llama_batch::LlamaBatch;
    use llama_cpp_2::model::params::LlamaModelParams;
    use llama_cpp_2::model::{AddBos, LlamaModel, Special};
    use llama_cpp_2::sampling::LlamaSampler;

    let backend = match LlamaBackend::init() {
        Ok(b) => b,
        Err(e) => {
            for req in rx {
                let _ = req.resp.send(Err(format!("llama backend init失敗: {e}")));
            }
            return;
        }
    };
    // CUDAビルドなら全層GPU(失敗時はllama.cpp側がCPUへ落とす)
    let mparams = LlamaModelParams::default().with_n_gpu_layers(999);
    let model = match LlamaModel::load_from_file(&backend, model_path(&root), &mparams) {
        Ok(m) => m,
        Err(e) => {
            for req in rx {
                let _ = req.resp.send(Err(format!("モデルロード失敗: {e}")));
            }
            return;
        }
    };
    println!("🧠 内蔵LLMロード完了: {MODEL_FILE}");
    for req in rx {
        let out = (|| -> Result<String, String> {
            let cparams = LlamaContextParams::default()
                .with_n_ctx(std::num::NonZeroU32::new(4096));
            let mut ctx = model.new_context(&backend, cparams).map_err(|e| e.to_string())?;
            let tokens = model.str_to_token(&req.prompt, AddBos::Never).map_err(|e| e.to_string())?;
            if tokens.is_empty() || tokens.len() > 3800 {
                return Err(format!("プロンプト長が不正({})", tokens.len()));
            }
            let mut batch = LlamaBatch::new(4096, 1);
            let last = tokens.len() - 1;
            for (i, t) in tokens.iter().enumerate() {
                batch.add(*t, i as i32, &[0], i == last).map_err(|e| e.to_string())?;
            }
            ctx.decode(&mut batch).map_err(|e| e.to_string())?;
            let mut sampler = LlamaSampler::chain_simple([
                LlamaSampler::temp(req.temp),
                LlamaSampler::dist(42),
            ]);
            let mut out = String::new();
            let mut n_cur = tokens.len() as i32;
            for _ in 0..req.max_tokens {
                let token = sampler.sample(&ctx, batch.n_tokens() - 1);
                sampler.accept(token);
                if model.is_eog_token(token) {
                    break;
                }
                out.push_str(&model.token_to_str(token, Special::Tokenize).unwrap_or_default());
                batch.clear();
                batch.add(token, n_cur, &[0], true).map_err(|e| e.to_string())?;
                n_cur += 1;
                ctx.decode(&mut batch).map_err(|e| e.to_string())?;
            }
            Ok(out)
        })();
        let _ = req.resp.send(out);
    }
}

/// チャット1発(ChatML)。モデル未DLならDLから面倒を見る。
pub async fn chat(
    root: &Path,
    client: &reqwest::Client,
    st: &LlmState,
    system: &str,
    user: &str,
    max_tokens: usize,
) -> Result<String, String> {
    chat_t(root, client, st, system, user, max_tokens, 0.3).await // 既定は低温(構造化タスク向き・語の発明を抑える)
}

#[allow(clippy::too_many_arguments)]
pub async fn chat_t(
    root: &Path,
    client: &reqwest::Client,
    st: &LlmState,
    system: &str,
    user: &str,
    max_tokens: usize,
    temp: f32,
) -> Result<String, String> {
    ensure_model(root, client, st).await?;
    let prompt = format!(
        "<|im_start|>system\n{system}<|im_end|>\n<|im_start|>user\n{user}<|im_end|>\n<|im_start|>assistant\n"
    );
    let (tx, rx) = tokio::sync::oneshot::channel();
    st.busy.store(true, Relaxed);
    engine_tx(root.to_path_buf())
        .send(Req { prompt, max_tokens, temp, resp: tx })
        .map_err(|_| "内蔵LLMスレッド死亡")?;
    let r = rx.await.map_err(|_| "内蔵LLM応答なし".to_string())?;
    st.busy.store(false, Relaxed);
    if r.is_ok() {
        st.ready.store(true, Relaxed);
    }
    r
}

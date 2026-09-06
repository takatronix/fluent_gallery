//! ONNX をどこで走らせるか(Execution Provider)の一点集中。
//!
//! OS ごとの分岐をここだけに閉じ込める(docs/model-placement-design.md §6 P2)。
//! Linux/CUDA と Mac/CoreML で名前も作法も違うが、呼ぶ側は `build(name, gpu)` だけ知っていればよい。
//!
//! GPU に載せる価値はモデルごとに全く違う(4090実測: SAM2 は 56倍、GroundingDINO は横ばい)。
//! だから「アプリ全体をGPUへ」ではなく、モデル単位で gpu=true/false を渡す設計にしてある。

use std::path::Path;

/// この版で GPU に載せられるか(ビルド時のフィーチャで決まる)
pub fn gpu_available() -> bool {
    cfg!(feature = "cuda") || cfg!(feature = "metal")
}

/// GPU の呼び名(UIの表示用)
pub fn gpu_label() -> &'static str {
    if cfg!(feature = "cuda") { "CUDA" } else if cfg!(feature = "metal") { "CoreML" } else { "なし" }
}

/// セッションを1本作る。gpu=true でも載せられなければ CPU に落ちる(落ちても動く方を優先)
pub fn build(path: &Path, threads: usize, gpu: bool, what: &str) -> Result<ort::session::Session, String> {
    let mut b = ort::session::Session::builder().map_err(|e| e.to_string())?;
    if gpu && gpu_available() {
        let eps: Vec<ort::ep::ExecutionProviderDispatch> = {
            #[cfg(feature = "cuda")]
            { vec![ort::ep::CUDA::default().build().error_on_failure()] }
            #[cfg(all(feature = "metal", not(feature = "cuda")))]
            { vec![ort::ep::CoreML::default().build().error_on_failure()] }
            #[cfg(not(any(feature = "cuda", feature = "metal")))]
            { vec![] }
        };
        // 失敗しても致命的ではない。CPU で動く方が「動かない」よりよいので、
        // 元の builder を作り直して続ける
        b = match b.with_execution_providers(eps) {
            Ok(nb) => { println!("🧠 {what}: {} で実行", gpu_label()); nb }
            Err(e) => {
                println!("⚠ {what}: {} を使えないので CPU({e})", gpu_label());
                ort::session::Session::builder().map_err(|e| e.to_string())?
            }
        };
    }
    let mut b = b.with_intra_threads(threads).map_err(|e| e.to_string())?;
    b.commit_from_file(path).map_err(|e| e.to_string())
}

/// ONNX の GPU 実行に要る共有ライブラリ(CUDA なら cublas/cudart/cuDNN)を engine/cuda/lib に
/// 置いておくと、そこを通して自分を起動し直す。
///
/// 動的リンカのパスはプロセス開始前に決まってしまうので、後から設定しても効かない。
/// かといって起動方法(deploy.sh / .app / systemd)ごとに面倒を見るのは漏れる。
/// **置いてあれば勝手に使う**のが一番間違いが少ないので、ここで1回だけ再実行する。
/// 無ければ何もしない(CPU で動く)。二重起動しないよう印を環境変数に残す。
pub fn adopt_gpu_libs(root: &Path) {
    const MARK: &str = "FG_GPU_LIBS_ADOPTED";
    if std::env::var(MARK).is_ok() { return; }
    let dir = root.join("engine/cuda/lib");
    if !dir.is_dir() { return; }
    let var = if cfg!(target_os = "macos") { "DYLD_LIBRARY_PATH" } else { "LD_LIBRARY_PATH" };
    let cur = std::env::var(var).unwrap_or_default();
    let want = dir.display().to_string();
    if cur.split(':').any(|p| p == want) { return; } // もう通っている
    let Ok(exe) = std::env::current_exe() else { return };
    let joined = if cur.is_empty() { want } else { format!("{want}:{cur}") };
    println!("🧠 GPU用ライブラリを見つけたので読み込み直します: {}", dir.display());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(exe)
            .args(std::env::args().skip(1))
            .env(var, joined)
            .env(MARK, "1")
            .exec(); // 成功すればここには戻らない
        println!("⚠ 起動し直せませんでした({err}) — CPU で続けます");
    }
}

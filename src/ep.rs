//! ONNX をどこで走らせるか(Execution Provider)の一点集中。
//!
//! OS ごとの分岐をここだけに閉じ込める(docs/model-placement-design.md §6 P2)。
//! Linux/CUDA と Mac/CoreML で名前も作法も違うが、呼ぶ側は `build(name, gpu)` だけ知っていればよい。
//!
//! GPU に載せる価値はモデルごとに全く違う(4090実測: SAM2 は 56倍、GroundingDINO は横ばい)。
//! だから「アプリ全体をGPUへ」ではなく、モデル単位で gpu=true/false を渡す設計にしてある。

use std::path::Path;

/// CPU で走らせるときのスレッド数。4固定だと 20コア級の機械で損をする
/// (Mac 実測: SAM2 が 4スレッド 0.99秒 → 全スレッド 0.71秒)。
/// ただし全部使うと収集中に UI まで重くなるので、半分だけ使う(最低2・最大16)
pub fn threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).div_ceil(2).clamp(2, 16)
}

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
    build_cached(path, threads, gpu, what, None)
}

/// cache_dir は CoreML のコンパイル結果の置き場(Mac のみ意味がある。初回40秒→2回目7.7秒)
pub fn build_cached(path: &Path, threads: usize, gpu: bool, what: &str, cache_dir: Option<std::path::PathBuf>) -> Result<ort::session::Session, String> {
    let _ = &cache_dir; // CUDA ビルドでは使わない
    let mut b = ort::session::Session::builder().map_err(|e| e.to_string())?;
    if gpu && gpu_available() {
        let eps: Vec<ort::ep::ExecutionProviderDispatch> = {
            #[cfg(feature = "cuda")]
            { vec![ort::ep::CUDA::default().build().error_on_failure()] }
            // CoreML は既定(NeuralNetwork形式)だと速くならない。M3 Ultra 実測で
            // CPU 0.71秒 → NeuralNetwork 0.88秒(遅い) → MLProgram 0.060秒(12倍)。
            // 初回コンパイルが40秒かかるので、キャッシュ置き場を渡して2回目以降 7.7秒にする。
            // ComputeUnits::All が最速(ANE単体 0.276秒 は GPU 0.060秒 より遅い)
            #[cfg(all(feature = "metal", not(feature = "cuda")))]
            {
                use ort::ep::coreml::{ComputeUnits, ModelFormat};
                use ort::ep::ArbitrarilyConfigurableExecutionProvider; // with_arbitrary_config はこのトレイト経由
                let mut ep = ort::ep::CoreML::default()
                    .with_model_format(ModelFormat::MLProgram)
                    .with_compute_units(ComputeUnits::All)
                    .with_static_input_shapes(true);
                if let Some(dir) = cache_dir {
                    let _ = std::fs::create_dir_all(&dir);
                    ep = ep.with_arbitrary_config("ModelCacheDirectory", &dir.display().to_string());
                }
                vec![ep.build().error_on_failure()]
            }
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

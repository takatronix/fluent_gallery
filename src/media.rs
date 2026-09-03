//! 動画取り込み — ffmpegで1秒1フレーム抽出。ml-hubの教訓を移植:
//! HDR動画(PQ/HLG)を素朴に8bit化すると白飛びする→ffprobeで検出してzscaleトーンマップ。
//! 「無駄なフレームは入れない」= 直前の採用フレームとpHashがほぼ同じならスキップ(静止場面の連写防止)。

use std::path::{Path, PathBuf};
use std::process::Command;

pub const VIDEO_EXTS: [&str; 6] = ["mp4", "mov", "webm", "mkv", "avi", "m4v"];

fn ffbin(name: &str) -> String {
    // 静的ffmpegは~/.local/bin(LD_LIBRARY_PATH汚染に注意: 空で呼ぶのが作法)
    let home = std::env::var("HOME").unwrap_or_default();
    let p = format!("{home}/.local/bin/{name}");
    if Path::new(&p).exists() { p } else { name.to_string() }
}

fn is_hdr(path: &Path) -> bool {
    Command::new(ffbin("ffprobe"))
        .env("LD_LIBRARY_PATH", "")
        .args(["-v", "quiet", "-select_streams", "v:0", "-show_entries", "stream=color_transfer",
               "-of", "default=nw=1:nk=1"])
        .arg(path)
        .output()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout).to_lowercase();
            s.contains("smpte2084") || s.contains("arib-std-b67")
        })
        .unwrap_or(false)
}

/// 動画バイト列→フレームJPEG群(1fps)。一時ファイル経由、終わったら掃除。
pub fn extract_frames(scratch: &Path, data: &[u8], fps: f32) -> Result<Vec<Vec<u8>>, String> {
    let _ = std::fs::create_dir_all(scratch);
    let vid = scratch.join("upload_video.bin");
    std::fs::write(&vid, data).map_err(|e| e.to_string())?;
    let out_dir = scratch.join("frames");
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    let vf = if is_hdr(&vid) {
        // HDR→SDR: リニア化→hableトーンマップ→bt709(ml-hub video_frames.pyの芯)
        // npl=203: BT.2408のHLG基準白アンカー。100だと明るすぎ/1000だと暗く平坦(ml-hub実測 2026-08)
        format!("fps={fps},zscale=transfer=linear:npl=203,tonemap=hable,zscale=transfer=bt709:matrix=bt709:primaries=bt709")
    } else {
        format!("fps={fps}")
    };
    let st = Command::new(ffbin("ffmpeg"))
        .env("LD_LIBRARY_PATH", "")
        .args(["-y", "-v", "error", "-i"])
        .arg(&vid)
        .args(["-vf", &vf, "-q:v", "3"])
        .arg(out_dir.join("f_%05d.jpg"))
        .status()
        .map_err(|e| format!("ffmpeg起動失敗: {e}"))?;
    let _ = std::fs::remove_file(&vid);
    if !st.success() {
        return Err("ffmpegがフレーム抽出に失敗(壊れた動画?)".into());
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&out_dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .collect();
    files.sort();
    let out = files.iter().filter_map(|p| std::fs::read(p).ok()).collect();
    let _ = std::fs::remove_dir_all(&out_dir);
    Ok(out)
}

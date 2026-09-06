//! ブラウザ内蔵クローラー(`crawler/` = Node + Playwright、docs/browser-crawler-spec.md)との連携。
//! - 呼び口 `/api/browse`: クローラーサービス(既定 127.0.0.1:8796)にジョブを投げる。未起動なら
//!   `crawler/server.js` を子プロセスで起こす(llama-server と同じ「親が死んだら道連れ」の見張り sh 方式)。
//! - 受け口 `/api/deliver`: クローラーが「人目線で関係ありそう」と選んだ画像を出典つきで受け取り、
//!   目利き(内蔵VLM)にかけてから `crawl:<album>` に収蔵する(判断は gallery 側=このファイルの外、main.rs)。
//! クローラー自身は判断せず、gallery の verdict(accepted/rejected/pending)を数えるだけ。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;

pub const DEFAULT_PORT: u16 = 8796;

#[derive(Default)]
pub struct BrowseState {
    pub child: Mutex<Option<std::process::Child>>,
    pub starting: AtomicBool,
    pub last_job: Mutex<String>,
    pub last_album: Mutex<String>,
    pub last_error: Mutex<String>,
    // gallery 側で数える成績(クローラーの目の良さ = accepted ÷ delivered)
    pub delivered: AtomicUsize,
    pub accepted: AtomicUsize,
    pub rejected: AtomicUsize,
    pub pending: AtomicUsize,
    pub dup: AtomicUsize,
    pub bad: AtomicUsize,
    pub recent: Mutex<Vec<Value>>, // 直近の判定ストリップ [{ok, r(sha|uk), why}] 最大 14(収集の recent と同型)
}

impl BrowseState {
    pub fn reset(&self) {
        for a in [&self.delivered, &self.accepted, &self.rejected, &self.pending, &self.dup, &self.bad] {
            a.store(0, Relaxed);
        }
        self.recent.lock().unwrap().clear();
    }
    pub fn push_recent(&self, ok: bool, r: &str, why: &str) {
        let mut v = self.recent.lock().unwrap();
        v.insert(0, json!({"ok": ok, "r": r, "why": why}));
        v.truncate(14);
    }
    pub fn status(&self) -> Value {
        let d = self.delivered.load(Relaxed);
        let a = self.accepted.load(Relaxed);
        json!({
            "job": self.last_job.lock().unwrap().clone(), "album": self.last_album.lock().unwrap().clone(),
            "delivered": d, "accepted": a, "rejected": self.rejected.load(Relaxed),
            "pending": self.pending.load(Relaxed), "dup": self.dup.load(Relaxed), "bad": self.bad.load(Relaxed),
            "pass_rate": if d > 0 { a as f64 / d as f64 } else { 0.0 },
            "recent": self.recent.lock().unwrap().clone(),
            "last_error": self.last_error.lock().unwrap().clone(),
            "service": {"base": base(), "own_child": self.child.lock().unwrap().is_some()},
        })
    }
}

pub fn port() -> u16 {
    std::env::var("FG_CRAWLER_PORT").ok().and_then(|p| p.parse().ok())
        .unwrap_or(crate::config::get_u64("crawler.port", DEFAULT_PORT as u64) as u16)
}
/// クローラーサービスの base。外部指定(FG_CRAWLER_BASE / crawler.base)が最優先、無ければ自前の子プロセス
pub fn base() -> String {
    crate::config::env_or("FG_CRAWLER_BASE", "crawler.base")
        .map(|b| b.trim_end_matches('/').to_string())
        .unwrap_or_else(|| format!("http://127.0.0.1:{}", port()))
}
fn external() -> bool { crate::config::env_or("FG_CRAWLER_BASE", "crawler.base").is_some() }

/// crawler/ の在り処(優先順): FG_CRAWLER_DIR / crawler.dir → root/crawler → 実行ファイルの隣 → .app の Resources/crawler → cwd/crawler
pub fn dir(root: &Path) -> Option<PathBuf> {
    let mut cands = vec![];
    if let Some(p) = crate::config::env_or("FG_CRAWLER_DIR", "crawler.dir") { cands.push(PathBuf::from(p)); }
    cands.push(root.join("crawler"));
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            cands.push(d.join("crawler"));
            cands.push(d.join("../Resources/crawler"));
        }
    }
    cands.push(PathBuf::from("crawler"));
    cands.into_iter().find(|d| d.join("server.js").exists())
}

pub async fn health(client: &reqwest::Client) -> Option<Value> {
    client.get(format!("{}/health", base())).timeout(std::time::Duration::from_secs(2))
        .send().await.ok()?.json::<Value>().await.ok().filter(|v| v["ok"].as_bool().unwrap_or(false))
}

fn node_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FG_NODE") { let p = PathBuf::from(p); if p.exists() { return Some(p); } }
    let mut cands: Vec<PathBuf> = vec!["/opt/homebrew/bin/node", "/usr/local/bin/node", "/usr/bin/node"].into_iter().map(PathBuf::from).collect();
    if let Some(home) = std::env::var_os("HOME") {
        // nvm / volta / fnm の既定置き場も見る(GUI アプリは PATH が貧しい)
        for pat in [".volta/bin/node", ".fnm/aliases/default/bin/node"] { cands.push(PathBuf::from(&home).join(pat)); }
        if let Ok(rd) = std::fs::read_dir(PathBuf::from(&home).join(".nvm/versions/node")) {
            let mut vs: Vec<PathBuf> = rd.flatten().map(|e| e.path().join("bin/node")).collect();
            vs.sort();
            cands.extend(vs.into_iter().rev());
        }
    }
    if let Some(p) = cands.into_iter().find(|p| p.exists()) { return Some(p); }
    std::env::var("PATH").ok()?.split(':').map(|d| Path::new(d).join("node")).find(|p| p.exists())
}

/// サービスが応答するまで面倒を見る。外部指定なら起動はせず疎通だけ確かめる
pub async fn ensure(root: &Path, client: &reqwest::Client, st: &BrowseState) -> Result<String, String> {
    if health(client).await.is_some() { return Ok(base()); }
    if external() { return Err(format!("クローラーサービス {} が応答しません", base())); }
    let dir = dir(root).ok_or_else(|| "crawler/server.js が見つかりません(リポジトリの crawler/ で npm install && npx playwright install chromium、または FG_CRAWLER_DIR=パス)".to_string())?;
    if !dir.join("node_modules/playwright").exists() {
        return Err(format!("{} に node_modules が無い: cd {} && npm install && npx playwright install chromium", dir.display(), dir.display()));
    }
    let node = node_bin().ok_or_else(|| "node が見つかりません(brew install node、または FG_NODE=パス)".to_string())?;
    if st.starting.swap(true, Relaxed) { return Err("クローラー起動中です".into()); }
    let r: Result<String, String> = async {
        let log = root.join("engine/crawler.log");
        let _ = std::fs::create_dir_all(log.parent().unwrap());
        let parent = std::process::id();
        let sh = format!(
            "cd \"{}\" && \"{}\" server.js --port {} >> \"{}\" 2>&1 & pid=$!; while kill -0 {} 2>/dev/null; do sleep 3; done; kill $pid 2>/dev/null",
            dir.display(), node.display(), port(), log.display(), parent);
        use std::os::unix::process::CommandExt;
        let child = std::process::Command::new("/bin/sh").arg("-c").arg(sh)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn().map_err(|e| format!("crawler 起動失敗: {e}"))?;
        *st.child.lock().unwrap() = Some(child);
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if let Some(h) = health(client).await {
                println!("🕵 ブラウザクローラー稼働: {} ({} {})", base(), h["impl"].as_str().unwrap_or("?"), h["browser"].as_str().unwrap_or(""));
                return Ok(base());
            }
            if let Some(c) = st.child.lock().unwrap().as_mut() {
                if c.try_wait().ok().flatten().is_some() { return Err("crawler が終了しました(engine/crawler.log を確認)".into()); }
            }
        }
        Err("crawler の起動待ちがタイムアウト(20秒)".into())
    }.await;
    st.starting.store(false, Relaxed);
    if let Err(e) = &r { *st.last_error.lock().unwrap() = e.clone(); }
    r
}

pub fn stop(st: &BrowseState) {
    if let Some(mut c) = st.child.lock().unwrap().take() {
        let _ = std::process::Command::new("/bin/kill").args(["-TERM", "--", &format!("-{}", c.id())]).status();
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// 画像バイト列の拡張子(マジックナンバー)
pub fn ext_of(data: &[u8]) -> &'static str {
    if data.starts_with(b"\x89PNG") { "png" } else if data.starts_with(b"RIFF") { "webp" } else if data.starts_with(b"GIF8") { "gif" } else { "jpg" }
}

/// クローラーの meta.crawl を収蔵用に整える(engine/album/tags の欠けを埋め、長すぎる文脈を切る)
pub fn normalize_crawl(mut c: Value, album: &str, goal: &str, landing_host: &str) -> Value {
    if !c.is_object() { c = json!({}); }
    let o = c.as_object_mut().unwrap();
    if o.get("engine").and_then(|v| v.as_str()).unwrap_or("").is_empty() { o.insert("engine".into(), json!("browser")); }
    o.insert("album".into(), json!(album));
    if o.get("query").and_then(|v| v.as_str()).unwrap_or("").is_empty() { o.insert("query".into(), json!(goal)); }
    let mut tags: Vec<String> = o.get("tags").and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|t| t.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
    if !landing_host.is_empty() && !tags.iter().any(|t| t == landing_host) { tags.push(landing_host.to_string()); }
    tags.truncate(8);
    o.insert("tags".into(), json!(tags));
    for k in ["context", "caption", "alt", "title"] {
        if let Some(s) = o.get(k).and_then(|v| v.as_str()) {
            if s.chars().count() > 400 { let cut: String = s.chars().take(400).collect(); o.insert(k.into(), json!(cut)); }
        }
    }
    c
}

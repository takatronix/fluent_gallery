//! M5 クローラ — AIフォルダ(goal付きアルバム)の▶で走る収集エージェントv0。
//! ml-hub collectで実証済みの芯を移植: クエリ生成(LLM)→検索(DDG/Openverse)→
//! クエリ/URL台帳→DL検査(バイト/デコード/短辺/アスペクト)→sha1/pHash重複→VLM意味ゲート→収蔵。
//! 教訓: ゴミは門前払い(収蔵前に落とす)、キーワード一致でなく「目標の意味」で判定、
//! 動作リミット(枚数/時間/連続エラー)無しで走らせない。

use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;

use crate::{enrich, llm, store};

const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36";
const MIN_BYTES: usize = 8_192; // これ未満=トラッキングピクセル/壊れの疑い
const MIN_SIDE: u32 = 200; // 短辺これ未満=サムネ/アイコン
const MAX_ASPECT: u32 = 4; // 縦横比これ超=バナー/スリバー
const PHASH_NEAR: u32 = 6; // ハミング距離これ以下=ほぼ同じ絵
// 動画フレームは同じ場面が延々続く(カメラが少し動くだけでpHash6を超える)ので門を広めに取る。
// 「そっくりフレームだらけになる」問題の舵はここ(2026-09-03)
const PHASH_NEAR_VIDEO: u32 = 11;
const MAX_KEEP_PER_VIDEO: usize = 6; // 1本の動画から採る上限(多様性の強制)
// 静的ffmpeg(~/.local/bin)はOSのCA束を知らずTLS検証で死ぬ(--download-sectionsのDLはffmpeg担当)。
// yt-dlp経由のffmpegにCA束を教える(2026-09-03 YouTube全滅の真因)
const CA_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";

/// 静的ffmpeg向けのCA束指定(Linux)。無い環境(Mac)では何もしない
fn ca_env(c: &mut std::process::Command) {
    if std::path::Path::new(CA_BUNDLE).exists() {
        c.env("SSL_CERT_FILE", CA_BUNDLE);
    }
}

/// 外部コマンドを秒数つきで実行(macOSには timeout コマンドが無いので自前)。期限で kill、成功可否を返す
pub(crate) fn status_timeout(c: &mut std::process::Command, secs: u64) -> bool {
    let Ok(mut child) = c.spawn() else { return false };
    let t0 = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return st.success(),
            Ok(None) if t0.elapsed().as_secs() >= secs => { let _ = child.kill(); let _ = child.wait(); return false; }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(200)),
            Err(_) => return false,
        }
    }
}

/// 同上・標準出力を返す版
pub(crate) fn output_timeout(c: &mut std::process::Command, secs: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut child = c.stdout(std::process::Stdio::piped()).spawn()?;
    let mut so = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || { let mut b = Vec::new(); let _ = so.read_to_end(&mut b); b });
    let t0 = std::time::Instant::now();
    loop {
        match child.try_wait()? {
            Some(_) => break,
            None if t0.elapsed().as_secs() >= secs => { let _ = child.kill(); let _ = child.wait(); break; }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    }
    Ok(reader.join().unwrap_or_default())
}

#[derive(Default)]
pub struct CrawlState {
    pub alive: AtomicBool,
    pub stop: AtomicBool,
    pub found: AtomicUsize,     // 検索で見つけた候補
    pub checked: AtomicUsize,   // DLして検査した数
    pub rejected: AtomicUsize,  // 検査/重複/意味ゲートで落ちた数
    pub ingested: AtomicUsize,  // 収蔵できた数
    pub errors: AtomicUsize,
    pub album: Mutex<String>,
    pub query: Mutex<String>,
    pub last: Mutex<String>,
    pub spent_cents: AtomicUsize, // 概算コスト(USDセント、Grok等の非厳密分)
    pub uusd: AtomicUsize,        // 実測コスト(マイクロUSD、Claude usage実測分)
    pub utok: AtomicUsize,        // 実測トークン数(Claude usage: input+output)
    pub recent: Mutex<Vec<Value>>, // 直近の検査結果ストリップ [{ok, r(sha|uk), why}] 最大14
    pub next_query: Mutex<String>, // クエリパイプラインが先読み中の次クエリ(空=先読みなし)
    pub ui_hot: std::sync::atomic::AtomicU64, // 最後にUIが画像/一覧を触ったunix秒(内蔵VLM判定の遠慮判断)
}

impl CrawlState {
    /// 直近secs秒以内にユーザーがUIを触ったか(内蔵VLM判定=CPU16コア級はこの間パースする)
    pub fn ui_recent(&self, secs: u64) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now.saturating_sub(self.ui_hot.load(Relaxed)) < secs
    }
}

impl CrawlState {
    pub fn status(&self) -> Value {
        let checked = self.checked.load(Relaxed);
        let ingested = self.ingested.load(Relaxed);
        json!({
            "alive": self.alive.load(Relaxed), "album": self.album.lock().unwrap().clone(),
            "query": self.query.lock().unwrap().clone(), "last": self.last.lock().unwrap().clone(),
            "found": self.found.load(Relaxed), "checked": checked,
            "rejected": self.rejected.load(Relaxed), "ingested": ingested,
            "errors": self.errors.load(Relaxed),
            "pass_rate": if checked > 0 { ingested as f64 / checked as f64 } else { 0.0 },
            "spent_usd": self.spent_cents.load(Relaxed) as f64 / 100.0 + self.uusd.load(Relaxed) as f64 / 1e6,
            "spent_tokens": self.utok.load(Relaxed),
            "recent": self.recent.lock().unwrap().clone(),
            "next_query": self.next_query.lock().unwrap().clone(),
        })
    }
}

pub struct Limits {
    pub max_n: usize,       // この枚数収蔵したら終了
    pub max_secs: u64,      // 実行時間上限
    pub max_errors: usize,  // 連続エラーでauto-stop
    pub min_quality: i64,   // VLM品質ゲート
    pub boost: bool,        // 💰ブースト: クエリ/目利きを外部AIに格上げ+並列判定(有料)
    pub max_cents: usize,   // ブーストの予算上限(超えたら内蔵AIに戻して続行)
    pub judge_model: String, // 目利きモデル(フォルダ設定>settings既定。スラッシュ入り=OpenRouter)
}

// ---------- SSRF対策: 内部アドレスへのDLを拒否 ----------
pub(crate) async fn is_safe_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.split('@').last().unwrap_or(host); // userinfo除去
    let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    if host.is_empty() {
        return false;
    }
    let Ok(addrs) = tokio::net::lookup_host((host, 80)).await else { return false };
    let mut any = false;
    for a in addrs {
        any = true;
        let ip = a.ip();
        let bad = match ip {
            std::net::IpAddr::V4(v) => {
                v.is_private() || v.is_loopback() || v.is_link_local() || v.is_unspecified()
                    || v.is_broadcast() || v.octets()[0] == 100 && (64..128).contains(&(v.octets()[1]))
            }
            std::net::IpAddr::V6(v) => v.is_loopback() || v.is_unspecified() || (v.segments()[0] & 0xfe00) == 0xfc00,
        };
        if bad {
            return false;
        }
    }
    any
}

// ---------- 検索(キーレス2系統) ----------
#[derive(Clone)]
struct Cand {
    url: String,
    title: String,
    license: String,
    landing: String,
    engine: &'static str,
}

fn find_between(hay: &str, pre: &str, post: char) -> Option<String> {
    let i = hay.find(pre)? + pre.len();
    let rest = &hay[i..];
    let j = rest.find(post)?;
    Some(rest[..j].to_string())
}

// 空=画像検索系5つ。YouTubeは似たフレームが増えやすいので既定OFF、X(Grok)も従量のためオプトイン
const DEFAULT_ENGINES: [&str; 5] = ["ddg", "openverse", "wikimedia", "pexels", "pixabay"];

type SearchHandle = tokio::task::JoinHandle<Result<Vec<Cand>, String>>;
struct ImgSearch {
    ddg: Option<SearchHandle>,
    ov: Option<SearchHandle>,
    wm: Option<SearchHandle>,
    pxb: Option<SearchHandle>,
    pex: Option<SearchHandle>,
}

/// 画像5エンジンの検索を同時に発射して手綱を返す。クエリパイプラインの部品:
/// 「判定(VLM)している間に次クエリの検索を先読み」で検索待ちを壁時計から消す(2026-09-03)。
/// 各エンジン同時1本の原則は保たれる(前クエリの検索完了後にしか次を発射しない)= DDG BAN回避。
fn spawn_img_searches(client: &reqwest::Client, engines: &[String], q: &str, allow_nsfw: bool) -> ImgSearch {
    let en = |name: &str| {
        if engines.is_empty() { DEFAULT_ENGINES.contains(&name) } else { engines.iter().any(|e| e == name) }
    };
    let sp = |fut: std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Cand>, String>> + Send>>| tokio::spawn(fut);
    ImgSearch {
        ddg: en("ddg").then(|| {
            let (c, q2) = (client.clone(), q.to_string());
            sp(Box::pin(async move { search_ddg(&c, &q2, 500, allow_nsfw).await }))
        }),
        ov: en("openverse").then(|| {
            let (c, q2) = (client.clone(), q.to_string());
            sp(Box::pin(async move { search_openverse(&c, &q2, 150).await }))
        }),
        wm: en("wikimedia").then(|| {
            let (c, q2) = (client.clone(), q.to_string());
            sp(Box::pin(async move { search_wikimedia(&c, &q2, 50).await }))
        }),
        pxb: en("pixabay").then(|| crate::enrich::mlhub_key("pixabay_api_key")).flatten().map(|k| {
            let (c, q2) = (client.clone(), q.to_string());
            sp(Box::pin(async move { search_pixabay(&c, &k, &q2, 150, allow_nsfw).await }))
        }),
        pex: en("pexels").then(|| crate::enrich::mlhub_key("pexels_api_key")).flatten().map(|k| {
            let (c, q2) = (client.clone(), q.to_string());
            sp(Box::pin(async move { search_pexels(&c, &k, &q2, 80).await }))
        }),
    }
}

async fn search_ddg(client: &reqwest::Client, query: &str, limit: usize, allow_nsfw: bool) -> Result<Vec<Cand>, String> {
    let html = client
        .post("https://duckduckgo.com/")
        .header("User-Agent", BROWSER_UA)
        .form(&[("q", query)])
        .send()
        .await
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let vqd = find_between(&html, "vqd=\"", '"')
        .or_else(|| find_between(&html, "vqd='", '\''))
        .or_else(|| find_between(&html, "vqd=", '&'))
        .ok_or("vqd取得不可(DDGに絞られてる可能性、少し待つ)")?;
    let mut out = vec![];
    // セーフサーチ: 目標が成人向けのフォルダはp=-1でOFF(ONだと「全然探せない」ml-hubと同じ教訓)
    let mut url = format!(
        "https://duckduckgo.com/i.js?l=us-en&o=json&q={}&vqd={}&f=,,,,,&p={}",
        urlenc(query), urlenc(&vqd), if allow_nsfw { "-1" } else { "1" }
    );
    for _ in 0..20 {
        if out.len() >= limit {
            break;
        }
        let r = client
            .get(&url)
            .header("User-Agent", BROWSER_UA)
            .header("Referer", "https://duckduckgo.com/")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !r.status().is_success() {
            break;
        }
        let Ok(data) = r.json::<Value>().await else { break };
        for it in data["results"].as_array().unwrap_or(&vec![]) {
            if let Some(u) = it["image"].as_str() {
                out.push(Cand {
                    url: u.into(),
                    title: it["title"].as_str().unwrap_or("").into(),
                    license: "unknown".into(),
                    landing: it["url"].as_str().unwrap_or("").into(),
                    engine: "ddg",
                });
            }
        }
        match data["next"].as_str() {
            Some(n) => url = format!("https://duckduckgo.com/{n}"),
            None => break,
        }
    }
    out.truncate(limit);
    Ok(out)
}

async fn search_openverse(client: &reqwest::Client, query: &str, limit: usize) -> Result<Vec<Cand>, String> {
    // 匿名はpage_size上限20 — ページングで深掘る(ml-hub方式)
    let mut out = vec![];
    for page in 1..=10 {
        if out.len() >= limit {
            break;
        }
        let Ok(resp) = client
            .get("https://api.openverse.org/v1/images/")
            .query(&[("q", query), ("page_size", "20"), ("page", &page.to_string()), ("mature", "false")])
            .header("User-Agent", "fluent_gallery/0.2 crawler")
            .send()
            .await
        else { break };
        let Ok(v) = resp.json::<Value>().await else { break };
        let results = v["results"].as_array().cloned().unwrap_or_default();
        if results.is_empty() {
            break;
        }
        for it in &results {
            if let Some(u) = it["url"].as_str() {
                out.push(Cand {
                    url: u.into(),
                    title: it["title"].as_str().unwrap_or("").into(),
                    license: it["license"].as_str().unwrap_or("cc").into(),
                    landing: it["foreign_landing_url"].as_str().unwrap_or("").into(),
                    engine: "openverse",
                });
            }
        }
        if v["page_count"].as_i64().map(|pc| page as i64 >= pc).unwrap_or(false) {
            break;
        }
    }
    out.truncate(limit);
    Ok(out)
}

/// Wikimedia Commons(キー不要・公式API・CC/PD・ライセンスメタ付き)
async fn search_wikimedia(client: &reqwest::Client, query: &str, limit: usize) -> Result<Vec<Cand>, String> {
    let v: Value = client
        .get("https://commons.wikimedia.org/w/api.php")
        .query(&[("action", "query"), ("format", "json"), ("generator", "search"),
                 ("gsrsearch", query), ("gsrnamespace", "6"), ("gsrlimit", "40"),
                 ("prop", "imageinfo"), ("iiprop", "url|extmetadata")])
        .header("User-Agent", "fluent_gallery/0.2 crawler")
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let mut out = vec![];
    if let Some(pages) = v["query"]["pages"].as_object() {
        for p in pages.values() {
            let info = &p["imageinfo"][0];
            let Some(url) = info["url"].as_str() else { continue };
            let low = url.to_lowercase();
            if !(low.ends_with(".jpg") || low.ends_with(".jpeg") || low.ends_with(".png") || low.ends_with(".webp")) {
                continue;
            }
            out.push(Cand {
                url: url.into(),
                title: p["title"].as_str().unwrap_or("").replace("File:", ""),
                license: info["extmetadata"]["LicenseShortName"]["value"].as_str().unwrap_or("cc").into(),
                landing: info["descriptionurl"].as_str().unwrap_or("").into(),
                engine: "wikimedia",
            });
        }
    }
    out.truncate(limit);
    Ok(out)
}

/// Pixabay(無料キー・商用可ライセンス)
async fn search_pixabay(client: &reqwest::Client, key: &str, query: &str, limit: usize, allow_nsfw: bool) -> Result<Vec<Cand>, String> {
    let v: Value = client
        .get("https://pixabay.com/api/")
        .query(&[("key", key), ("q", query), ("per_page", "150"), ("image_type", "photo"),
                 ("safesearch", if allow_nsfw { "false" } else { "true" })])
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let mut out = vec![];
    for h in v["hits"].as_array().unwrap_or(&vec![]) {
        if let Some(u) = h["largeImageURL"].as_str().or_else(|| h["webformatURL"].as_str()) {
            out.push(Cand {
                url: u.into(),
                title: h["tags"].as_str().unwrap_or("").into(),
                license: "pixabay".into(),
                landing: h["pageURL"].as_str().unwrap_or("").into(),
                engine: "pixabay",
            });
        }
    }
    out.truncate(limit);
    Ok(out)
}

/// Pexels(無料キー・商用可ライセンス)
async fn search_pexels(client: &reqwest::Client, key: &str, query: &str, limit: usize) -> Result<Vec<Cand>, String> {
    let v: Value = client
        .get("https://api.pexels.com/v1/search")
        .query(&[("query", query), ("per_page", "80")])
        .header("Authorization", key)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let mut out = vec![];
    for p in v["photos"].as_array().unwrap_or(&vec![]) {
        if let Some(u) = p["src"]["large2x"].as_str().or_else(|| p["src"]["original"].as_str()) {
            out.push(Cand {
                url: u.into(),
                title: p["alt"].as_str().unwrap_or("").into(),
                license: "pexels".into(),
                landing: p["url"].as_str().unwrap_or("").into(),
                engine: "pexels",
            });
        }
    }
    out.truncate(limit);
    Ok(out)
}

/// X(Grok Agent Tools): x_searchでメディア付きポストURLを探す(ml-hub実証方式の移植)。
/// 画像直リンクは取れないので、返ったポストURLをyt-dlpでメディアDL→フレーム化する
async fn x_post_urls(client: &reqwest::Client, key: &str, query: &str, limit: usize) -> Result<Vec<String>, String> {
    let prompt = format!(
        "学習用の画像データを集めたい。「{query}」の被写体が映像として実際に映っている(画像/動画メディア付き)\
         Xの投稿を最大{limit}件さがして。いま話題・拡散されている投稿(直近で反応の多いもの)を優先しつつ、\
         足りなければ過去の人気投稿でもよい。各投稿を1行1件で、行の先頭に投稿URL(x.com/.../status/...)を書いて。\
         メディアの無い投稿・関連の薄いものは除外して。"
    );
    let v: Value = client
        .post("https://api.x.ai/v1/responses")
        .bearer_auth(key)
        .json(&json!({
            "model": "grok-4-fast",
            "input": [{"role": "user", "content": prompt}],
            "tools": [{"type": "x_search"}]
        }))
        .timeout(std::time::Duration::from_secs(90))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    if !v["error"].is_null() {
        return Err(format!("Grok: {}", v["error"]));
    }
    // citationsだけを見る(応答全文からの乱獲はGrokが本文に書く例示URL=捏造ID
    // 「0987654321」等まで拾ってyt-dlpが空振りする。ml-hub x_grok_searchと同じ教訓 2026-09-03)
    let mut urls: Vec<String> = vec![];
    let mut push = |u: &str| {
        let ok = (u.starts_with("https://x.com/") || u.starts_with("https://twitter.com/"))
            && u.split("/status/")
                .nth(1)
                .and_then(|s| s.split(['?', '/', '#']).next())
                .map(|id| id.len() >= 8 && id.chars().all(|c| c.is_ascii_digit()))
                .unwrap_or(false);
        if ok && !urls.iter().any(|x| x == u) {
            urls.push(u.to_string());
        }
    };
    if let Some(cs) = v["citations"].as_array() {
        for c in cs {
            if let Some(u) = c.as_str().or_else(|| c["url"].as_str()).or_else(|| c["uri"].as_str()) {
                push(u);
            }
        }
    }
    if let Some(outs) = v["output"].as_array() {
        for o in outs {
            for ct in o["content"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                for a in ct["annotations"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                    if let Some(u) = a["url"].as_str() {
                        push(u);
                    }
                }
            }
        }
    }
    urls.truncate(limit);
    Ok(urls)
}

/// X投稿の画像を配信CDN(syndication)からキーレスで取る(画像だけのポストはyt-dlpで取れない問題の根治)
async fn x_syndication_photos(client: &reqwest::Client, post_url: &str) -> Vec<String> {
    let Some(id) = post_url.split("/status/").nth(1).and_then(|s| s.split(['?', '/', '#']).next()) else {
        return vec![];
    };
    let Ok(resp) = client
        .get(format!("https://cdn.syndication.twimg.com/tweet-result?id={id}&lang=en&token=a"))
        .header("User-Agent", BROWSER_UA)
        .timeout(std::time::Duration::from_secs(12))
        .send()
        .await
    else { return vec![] };
    let Ok(v) = resp.json::<Value>().await else { return vec![] };
    let mut out = vec![];
    for m in v["mediaDetails"].as_array().unwrap_or(&vec![]) {
        if m["type"] == "photo" {
            if let Some(u) = m["media_url_https"].as_str() {
                out.push(format!("{u}?name=large"));
            }
        }
    }
    out
}

/// 任意URL群(X投稿等)のメディアをyt-dlpでDL→フレーム化。(bytes, 出典URL, タイトル)を返す。
/// yt-dlpはtimeoutで強制打ち切り(ログイン壁等での無限ハング根絶)
pub(crate) fn media_frames_from_urls(scratch: &Path, urls: &[String]) -> Vec<(Vec<u8>, String, String)> {
    let ytdlp = crate::media::tool_bin("yt-dlp");
    let dir = scratch.join("xmedia");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    let mut out = vec![];
    for (i, u) in urls.iter().enumerate() {
        let f = dir.join(format!("m{i}.mp4"));
        let mut c = std::process::Command::new(&ytdlp);
        c.env("LD_LIBRARY_PATH", "")
            // 映像だけあればよい(音声不要)。新しいYouTubeは進行形式(b)が無いことがあるので bv* を先に
            .args(["-f", "bv*[height<=720]/b[height<=720]/bv*/b", "--max-filesize", "60M", "--no-playlist",
                   "--socket-timeout", "15", "--quiet", "--no-warnings", "-o"])
            .arg(&f)
            .arg(u);
        ca_env(&mut c); // 静的ffmpegのTLS検証失敗(code251でYT収穫0)の根治 2026-09-03
        let ok = status_timeout(&mut c, 120);
        if !ok {
            continue; // 画像のみのポストはyt-dlpで取れないことがある(ml-hubと同じ割り切り)
        }
        if let Ok(data) = std::fs::read(&f) {
            if let Ok(frames) = crate::media::extract_frames(scratch, &data, 0.5) {
                for fr in frames {
                    out.push((fr, u.clone(), String::new()));
                }
            }
        }
        let _ = std::fs::remove_file(&f);
    }
    let _ = std::fs::remove_dir_all(&dir);
    out
}

/// クエリとタイトルの関連度(0..1)。ml-hub video_crawl.relevance_scoreの移植:
/// 空白区切りの語はタイトル内ヒット率、CJK等スペース無し1語は部分一致で1/0。
fn yt_relevance(title: &str, query: &str) -> f64 {
    let t = title.to_lowercase();
    let terms: Vec<String> = query.to_lowercase().split_whitespace().map(String::from).collect();
    if terms.is_empty() || t.is_empty() {
        return 0.0;
    }
    if terms.len() == 1 {
        return if t.contains(&terms[0]) { 1.0 } else { 0.0 };
    }
    terms.iter().filter(|w| t.contains(w.as_str())).count() as f64 / terms.len() as f64
}

/// YouTube: メタ検索(DLなし)→タイトル関連度+再生数+長さで選定→上位だけDL→0.5fpsフレーム抽出。
/// 「検索上位2本を無条件DL」だとMV/広告/無関係が混ざる — 選んでから落とす(2026-09-03)
fn youtube_frames(scratch: &Path, query: &str, videos: usize) -> Result<Vec<(Vec<u8>, String, String)>, String> {
    let ytdlp = crate::media::tool_bin("yt-dlp");
    let dir = scratch.join("yt");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // ① メタだけ12件(数秒・DLなし)
    let mut mc = std::process::Command::new(&ytdlp);
    mc.env("LD_LIBRARY_PATH", "")
        .args([&format!("ytsearch12:{query}"), "--flat-playlist", "-j", "--quiet", "--no-warnings"]);
    let out = output_timeout(&mut mc, 60).map_err(|e| format!("yt-dlp起動失敗: {e}"))?;
    let mut cands: Vec<(String, String, String, f64)> = vec![]; // (id,url,title,score)
    for l in String::from_utf8_lossy(&out).lines() {
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        let (Some(id), Some(title)) = (v["id"].as_str(), v["title"].as_str()) else { continue };
        let dur = v["duration"].as_f64().unwrap_or(0.0);
        // ゴミ抜き: 30秒未満(ジングル/宣伝)と30分超(配信アーカイブ=同じ画の山)は見ない。0=不明は通す
        if dur > 0.0 && !(30.0..=1800.0).contains(&dur) {
            continue;
        }
        let views = v["view_count"].as_f64().unwrap_or(0.0);
        // 関連度が主・再生数は同点の並べ替え(log圧縮で桁の暴力を抑える)
        let score = yt_relevance(title, query) * 10.0 + (views + 1.0).log10();
        let url = v["url"].as_str().map(String::from)
            .unwrap_or_else(|| format!("https://www.youtube.com/watch?v={id}"));
        cands.push((id.to_string(), url, title.to_string(), score));
    }
    if cands.is_empty() {
        return Err("yt-dlpが失敗(検索0件)".into());
    }
    cands.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
    cands.truncate(videos.max(1));
    // ② 選ばれた動画だけDL(冒頭60秒・480p)
    let mut dl = std::process::Command::new(&ytdlp);
    dl.env("LD_LIBRARY_PATH", "")
        .args([
            "-f", "bv*[height<=480]/b[height<=480]/bv*/b", // 映像のみで足りる(音声結合なし=ffmpeg合成不要)
            "--max-filesize", "80M",
            "--download-sections", "*0-60", // 冒頭60秒だけ(データ量の舵)
            "--socket-timeout", "15",
            "--ignore-errors", // 1本のDL失敗(限定公開等)で残りを道連れにしない
            "--no-playlist", "--quiet", "--no-warnings",
            "-o",
        ])
        .arg(dir.join("%(id)s.%(ext)s"));
    for (_, u, _, _) in &cands {
        dl.arg(u);
    }
    ca_env(&mut dl); // 静的ffmpegのTLS検証失敗(code251でYT収穫0)の根治 2026-09-03
    let st_ok = status_timeout(&mut dl, 180);
    let metas: Vec<(String, String, String)> =
        cands.into_iter().map(|(id, u, t, _)| (id, u, t)).collect();
    // 失敗判定はファイル実在で(exit codeは--ignore-errorsでも一部失敗で非0になる)
    let got_any = std::fs::read_dir(&dir)
        .map(|rd| rd.flatten().any(|e| e.path().extension().map(|x| x != "txt").unwrap_or(false)))
        .unwrap_or(false);
    if !got_any {
        return Err(format!("yt-dlpが失敗(DL不可 ok={st_ok})"));
    }
    let mut out = vec![];
    for (id, url, title) in metas {
        let Some(f) = std::fs::read_dir(&dir).ok().and_then(|rd| {
            rd.flatten().map(|e| e.path()).find(|p| {
                p.file_stem().map(|s| s.to_string_lossy() == id).unwrap_or(false)
                    && p.extension().map(|e| e != "txt").unwrap_or(false)
            })
        }) else { continue };
        if let Ok(data) = std::fs::read(&f) {
            if let Ok(frames) = crate::media::extract_frames(scratch, &data, 0.5) {
                for fr in frames {
                    out.push((fr, url.clone(), title.clone()));
                }
            }
        }
        let _ = std::fs::remove_file(&f);
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(out)
}

fn urlenc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

// ---------- クエリ生成(コスト階段: ①内蔵LLM=無料 → ②Claude → ③素朴) ----------
fn parse_queries(text: &str) -> Vec<String> {
    let (Some(a), Some(b)) = (text.find('{'), text.rfind('}')) else { return vec![] };
    serde_json::from_str::<Value>(&text[a..=b])
        .ok()
        .and_then(|p| {
            p["queries"]
                .as_array()
                .map(|a| a.iter().filter_map(|q| q.as_str().map(String::from)).filter(|q| !q.trim().is_empty()).collect())
        })
        .unwrap_or_default()
}

/// クエリ浄化: 小型LLMが吐く文字化け(U+FFFD)・短すぎ・使用済みを弾く。
/// 台帳に化けたクエリが溜まる→検索が全部ゴミ、の再発防止(2026-09-03の実害)
fn clean_queries(qs: Vec<String>, done: &[String]) -> Vec<String> {
    qs.into_iter()
        .map(|q| q.trim().to_string())
        .filter(|q| !q.contains('\u{FFFD}'))
        .filter(|q| q.chars().count() >= 3)
        .filter(|q| !done.contains(q))
        .collect()
}

async fn claude_text(client: &reqwest::Client, key: &str, user: &str, max_tokens: u32) -> String {
    // thinkingブロック対策: type=="text"を選ぶ(教訓)
    let r = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .json(&json!({"model": "claude-sonnet-5", "max_tokens": max_tokens,
            "messages": [{"role": "user", "content": user}]}))
        .send()
        .await;
    if let Ok(resp) = r {
        if let Ok(v) = resp.json::<Value>().await {
            return v["content"]
                .as_array()
                .and_then(|a| a.iter().find(|b| b["type"] == "text"))
                .and_then(|b| b["text"].as_str())
                .unwrap_or("")
                .to_string();
        }
    }
    String::new()
}

async fn claude_queries(client: &reqwest::Client, key: &str, user: &str) -> Vec<String> {
    parse_queries(&claude_text(client, key, user, 500).await)
}

/// 目標文の正規化ブリーフ: 被写体の正体(正式表記/現地語表記/カテゴリ/メンバー名)をAIで一度だけ展開し
/// 台帳にキャッシュする。「エスパ カリナ ニンニン ジゼル」を内蔵7Bが "Esper Kira Nina Jinzer" と
/// 誤ローマ字化して検索も判定も全滅した実害の根治(2026-09-03)。入口が狂うと全部ゴミになるので、
/// この1回だけはブーストOFFでもAnthropicキーが在れば使う(数円・台帳キャッシュで再課金なし)。
async fn ensure_brief(root: &Path, client: &reqwest::Client, llm_st: &llm::LlmState,
                      goal: &str, hints: &[String], key: Option<&str>, cached: &str) -> String {
    if !cached.trim().is_empty() {
        return cached.to_string();
    }
    let user = format!(
        "画像収集の目標文を解釈する。目標:「{goal}」 ヒント:{hints:?}\n\
         この目標の被写体が何者/何かを特定し、検索と画像判定に役立つ背景を書け:\n\
         - 固有名詞の正式名称(英語表記・ハングル等の現地表記・通称)\n\
         - カテゴリ(例: 韓国の女性K-POPアイドルグループ、犬種、料理名)\n\
         - 人物グループならメンバー名の正式表記\n\
         3行以内・事実のみ・不明な点は書かない。JSONのみ: {{\"brief\": \"...\"}}"
    );
    let parse = |t: &str| -> String {
        let (Some(a), Some(b)) = (t.find('{'), t.rfind('}')) else { return String::new() };
        serde_json::from_str::<Value>(&t[a..=b])
            .ok()
            .and_then(|v| v["brief"].as_str().map(String::from))
            .unwrap_or_default()
    };
    if let Some(k) = key {
        let b = parse(&claude_text(client, k, &user, 400).await);
        if !b.is_empty() {
            return b;
        }
    }
    if let Ok(t) = llm::chat(root, client, llm_st, "Reply with ONLY the requested JSON.", &user, 300).await {
        let b = parse(&t);
        // 7Bの化け/幻覚ガード: 化け文字入り・短すぎは捨てる(無いほうがマシ)
        if !b.contains('\u{FFFD}') && b.chars().count() >= 8 {
            return b;
        }
    }
    String::new()
}

/// 実績テンプレの即席クエリ(2026-09-03指示「決まったケースは最初から強く」)。
/// briefが合流済みのgoalからカテゴリを見て、実戦で当たり続けているパターンを決定的に生成。
/// 1ラウンド目はこれだけで走る(LLM呼び出し不要・$0・即時)。2ラウンド目以降のLLMは
/// 「まだ試していない角度」の生成に専念する。テンプレは実戦の採用実績から随時追記する台帳。
fn seed_queries(album: &str, goal: &str, done: &[String]) -> Vec<String> {
    let a = album.trim();
    let g = goal;
    let mut out: Vec<String> = vec![];
    if g.contains("K-POP") || g.contains("KPOP") || g.contains("アイドル") || g.contains("케이팝") {
        // K-POP実績パターン(2026-09-03実測: 직캠/fancam系が最高採用率)
        out.extend([
            format!("{a} 직캠 4K"),
            format!("{a} fancam"),
            format!("{a} 高画質 実写"),
            format!("{a} photo HD"),
            format!("{a} 사진"),
        ]);
    } else if g.contains("イラスト") || g.contains("アニメ") || g.contains("漫画") {
        out.extend([
            format!("{a} イラスト"),
            format!("{a} fanart"),
            format!("{a} 公式 画像"),
            format!("{a} art"),
        ]);
    } else {
        out.extend([
            format!("{a} 写真"),
            format!("{a} photo"),
            format!("{a} high resolution"),
        ]);
    }
    out.retain(|q| !done.contains(q));
    out
}

async fn gen_queries(
    album: &str,
    root: &Path,
    client: &reqwest::Client,
    llm_st: &llm::LlmState,
    goal: &str,
    hints: &[String],
    done: &[String],
    boost: bool,
) -> (Vec<String>, usize) {
    // 初回ラウンドは実績テンプレで即走(LLM不要)。カテゴリが合えば当たりのパターンから始まる
    if done.is_empty() {
        let seeds = clean_queries(seed_queries(album, goal, done), done);
        if seeds.len() >= 3 {
            return (seeds, 0);
        }
    }
    let hint_line = if hints.is_empty() {
        String::new()
    } else {
        format!("ユーザーのヒント(必ず検索語に反映): {hints:?}\n")
    };
    let user = format!(
        "画像収集の検索クエリを作る。目標:「{goal}」\n{hint_line}\
         使用済み(重複禁止): {done:?}\n\
         Web画像検索で目標に合う画像が引ける具体的なクエリを8個。日本語4個+英語4個。\n\
         ルール: 目標とヒントに含まれる語彙だけで組む(無関係な語・属性を発明しない)。\
         人名・グループ名は正式な綴りに直し、別表記(ハングル/正式ローマ字/日本語表記)も使ってよい。\
         一般語だけのクエリ禁止(「写真」「image」単体等)。JSONのみ: {{\"queries\": [\"...\"]}}"
    );
    let key = enrich::mlhub_key("anthropic_api_key");
    // 💰ブースト: 最初からClaude(綴り訂正・別名展開ができる=検索の入口が良くなる)
    if boost {
        if let Some(k) = &key {
            let qs = clean_queries(claude_queries(client, k, &user).await, done);
            if !qs.is_empty() {
                return (qs, 2); // 概算2セント
            }
        }
    }
    // ①内蔵LLM(llama.cpp直リンク・無料)。モデル未DLなら初回だけDLが走る
    match llm::chat(root, client, llm_st, "Reply with ONLY the requested JSON.", &user, 400).await {
        Ok(text) => {
            let qs = clean_queries(parse_queries(&text), done);
            // 半分以上生き残ったら採用。化け混じりならClaudeに聞き直す
            if qs.len() >= 3 {
                return (qs, 0);
            }
        }
        Err(e) => println!("🧠 内蔵LLMクエリ生成不可({e}) → Claudeへ"),
    }
    // ②Claude(キーがあれば)
    if let Some(k) = &key {
        let qs = clean_queries(claude_queries(client, k, &user).await, done);
        if !qs.is_empty() {
            return (qs, 2);
        }
    }
    // ③素朴フォールバック: 目標文そのまま+定番言い換え
    let base = vec![
        goal.to_string(),
        format!("{goal} 写真"),
        format!("{goal} photo"),
        format!("{goal} high resolution"),
    ];
    (clean_queries(base, done), 0)
}

// ---------- VLM意味ゲート(目利き) ----------
// 無料=内蔵qwen2.5vl(7B)。グッズEC画面やトレカ販売写真を「TWICE関連」で通してしまう弱点があるので、
// NGリストとユーザーヒントをプロンプトに焼き込む。💰ブースト時はClaude(sonnet-5)が目利き=人物同定も効く。
fn judge_prompt(goal: &str, hints: &[String], strict_person: bool) -> String {
    let hint_line = if hints.is_empty() {
        String::new()
    } else {
        format!("USER REQUIREMENTS (every one must hold): {hints:?}\n")
    };
    // 人物同定: Claude(強い目)だけ厳密に。内蔵7Bに「本人と確認できたら」を課すと
    // 確認できない→全部却下になる(wonyong X検索が収蔵0になった実害 2026-09-03)
    let person_line = if strict_person {
        "If the goal names a specific person or group, accept only if the pictured person is really them.\n"
    } else {
        "If the goal names a specific person or group, reject only clear mismatches \
         (obviously a different kind of person); do NOT reject just because you cannot verify identity.\n"
    };
    format!(
        "COLLECTION GOAL: {goal}\n{hint_line}\
         Accept ONLY a real photograph that clearly serves the goal as training/collection material.\n\
         REJECT if ANY of these: product or merchandise listing, price tags, online-shop or app screenshot, \
         collage/grid of small cards or thumbnails, photocard/goods sale photo, text-heavy promo or news graphic, \
         logo-only image. Reject drawings/anime/CG unless the goal explicitly asks for them.\n\
         Reject when the main subject is a different category than the goal \
         (an animal when the goal is a person, an object, random bystanders, generic stock-photo models).\n\
         {person_line}\
         Reply ONLY JSON: {{\"match\": true|false, \"quality\": 1-10}}"
    )
}

fn parse_judge(text: &str) -> Result<(bool, i64), String> {
    let (Some(a), Some(b)) = (text.find('{'), text.rfind('}')) else { return Err("judge JSON壊れ".into()) };
    let p: Value = serde_json::from_str(&text[a..=b]).map_err(|_| "judge JSON壊れ")?;
    Ok((p["match"].as_bool().unwrap_or(false), p["quality"].as_i64().unwrap_or(0)))
}

/// 内蔵VLM(無料)。原寸を送ると転送/前処理が重いので896pxに縮めて送る(判定精度は落ちない)
async fn judge_builtin(client: &reqwest::Client, img: &image::DynamicImage, goal: &str, hints: &[String]) -> Result<(bool, i64), String> {
    use base64::Engine;
    let th = img.thumbnail(896, 896).into_rgb8();
    let mut buf = std::io::Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85)
        .encode_image(&th)
        .map_err(|e| e.to_string())?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(buf.get_ref());
    let v: Value = client
        .post(format!("{}/api/generate", enrich::OLLAMA))
        .json(&json!({"model": enrich::BUILTIN_MODEL, "prompt": judge_prompt(goal, hints, false), "images": [b64],
                      "stream": false, "format": "json", "options": {"temperature": 0.0}}))
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    parse_judge(v["response"].as_str().unwrap_or(""))
}

/// 💰目利きのモデル/プロバイダ設定(ml-hub settings.json)。安いAPIへの切替口(2026-09-03指示):
///   gallery_judge_model: "claude-haiku-4-5"(Anthropic・1/3コスト) or "google/gemini-..."等(スラッシュ入り=OpenRouter)
///   openrouter_api_key: OpenRouter利用時に必須
/// 未設定なら従来どおり claude-sonnet-5。
/// 既定の目利きモデル(settings.json gallery_judge_model)。フォルダ側agent.judge_modelが優先
pub fn default_judge_model() -> String {
    crate::enrich::mlhub_key("gallery_judge_model")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "claude-sonnet-5".into())
}

/// モデル名→(入力$/M, 出力$/M)。OpenRouterは主要候補だけ実額、他は保守的な概算($0.5/M均し)
fn judge_rates(model: &str) -> (f64, f64) {
    if model.contains("qwen2.5-vl-72b") {
        (0.25, 0.75) // OpenRouter実額(2026-09調べ)
    } else if model.contains("gemini-2.5-flash-lite") {
        (0.10, 0.40) // OpenRouter実額(2026-09調べ)
    } else if model.contains('/') {
        (0.5, 0.5) // OpenRouter概算(実額はOpenRouterのダッシュボード参照)
    } else if model.contains("haiku") {
        (1.0, 5.0)
    } else {
        (3.0, 15.0) // sonnet系
    }
}

/// OpenRouter(OpenAI互換 chat/completions)でのvision判定。モデル名にスラッシュがある時だけ使う
async fn judge_openrouter(client: &reqwest::Client, key: &str, model: &str, b64: &str,
                          goal: &str, hints: &[String]) -> Result<(bool, i64, f64, u64), String> {
    let v: Value = client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(key)
        .json(&json!({"model": model, "max_tokens": 100, "temperature": 0,
            "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": format!("data:image/jpeg;base64,{b64}")}},
                {"type": "text", "text": judge_prompt(goal, hints, true)},
            ]}]}))
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    if !v["error"].is_null() {
        return Err(format!("OpenRouter: {}", v["error"]["message"].as_str().unwrap_or("?")));
    }
    let text = v["choices"][0]["message"]["content"].as_str().unwrap_or("");
    let tok = (v["usage"]["prompt_tokens"].as_f64().unwrap_or(0.0)
        + v["usage"]["completion_tokens"].as_f64().unwrap_or(0.0)) as u64;
    let (ri, ro) = judge_rates(v["model"].as_str().unwrap_or(""));
    let usd = v["usage"]["prompt_tokens"].as_f64().unwrap_or(0.0) * ri / 1e6
        + v["usage"]["completion_tokens"].as_f64().unwrap_or(0.0) * ro / 1e6;
    let (m, q) = parse_judge(text)?;
    Ok((m, q, usd, tok))
}

/// 💰目利き(vision)。既定=claude-sonnet-5、settingsで安いモデルに切替可(judge_model参照)。
/// 画像は1024pxに縮小して送る(帯域/コスト)。戻り値は(match, quality, 実測USD, トークン数)。
async fn judge_claude(
    client: &reqwest::Client,
    key: &str,
    img: &image::DynamicImage,
    goal: &str,
    hints: &[String],
    model: &str,
) -> Result<(bool, i64, f64, u64), String> {
    use base64::Engine;
    let th = img.thumbnail(1024, 1024).into_rgb8();
    let mut buf = std::io::Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85)
        .encode_image(&th)
        .map_err(|e| e.to_string())?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(buf.get_ref());
    // スラッシュ入りモデル名(例: google/gemini-2.5-flash)=OpenRouter経由の格安VLM
    if model.contains('/') {
        if let Some(ork) = crate::enrich::mlhub_key("openrouter_api_key") {
            return judge_openrouter(client, &ork, model, &b64, goal, hints).await;
        }
        return Err("gallery_judge_modelがOpenRouter形式ですがopenrouter_api_keyが未設定です".into());
    }
    let v: Value = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .json(&json!({"model": model, "max_tokens": 100,
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": b64}},
                {"type": "text", "text": judge_prompt(goal, hints, true)},
            ]}]}))
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let text = v["content"]
        .as_array()
        .and_then(|a| a.iter().find(|b| b["type"] == "text"))
        .and_then(|b| b["text"].as_str())
        .unwrap_or("");
    let tok = (v["usage"]["input_tokens"].as_f64().unwrap_or(0.0)
        + v["usage"]["output_tokens"].as_f64().unwrap_or(0.0)) as u64;
    let (ri, ro) = judge_rates(model); // sonnet系$3/$15・haiku$1/$5
    let usd = v["usage"]["input_tokens"].as_f64().unwrap_or(0.0) * ri / 1e6
        + v["usage"]["output_tokens"].as_f64().unwrap_or(0.0) * ro / 1e6;
    let (m, q) = parse_judge(text)?;
    Ok((m, q, usd, tok))
}

// ---------- 台帳(クエリ/URLの既読管理、runを跨いで効く) ----------
fn ledger_path(root: &Path, album: &str) -> PathBuf {
    root.join("store/crawl").join(format!("{album}.ledger.json"))
}

fn load_ledger(root: &Path, album: &str) -> (Vec<String>, std::collections::HashSet<String>, String) {
    let v: Value = std::fs::read_to_string(ledger_path(root, album))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(json!({}));
    let qs = v["queries"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let urls = v["urls"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let brief = v["brief"].as_str().unwrap_or("").to_string();
    (qs, urls, brief)
}

fn save_ledger(root: &Path, album: &str, queries: &[String], urls: &std::collections::HashSet<String>, brief: &str) {
    let p = ledger_path(root, album);
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let _ = std::fs::write(&p, serde_json::to_string(&json!({"queries": queries, "urls": urls, "brief": brief})).unwrap());
}

fn url_key(u: &str) -> String {
    hex::encode(Sha1::digest(u.as_bytes()))[..16].to_string()
}

/// 途中経過ストリップ: 却下画像の小サムネを保存(最大40枚・古い物から捨てる)。「なぜ落ちたか」を目で見える化
fn save_reject_thumb(root: &Path, uk: &str, img: &image::DynamicImage) {
    let dir = root.join("store/crawl/rejects");
    let _ = std::fs::create_dir_all(&dir);
    let th = img.thumbnail(320, 320).into_rgb8();
    let mut buf = std::io::Cursor::new(Vec::new());
    if image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 70).encode_image(&th).is_ok() {
        let _ = std::fs::write(dir.join(format!("{uk}.jpg")), buf.get_ref());
    }
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let mut files: Vec<_> = rd.flatten().filter_map(|e| e.metadata().ok().and_then(|m| m.modified().ok()).map(|t| (t, e.path()))).collect();
        if files.len() > 40 {
            files.sort_by_key(|(t, _)| *t);
            for (_, p) in files.iter().take(files.len() - 40) {
                let _ = std::fs::remove_file(p);
            }
        }
    }
}

fn push_recent(st: &CrawlState, ok: bool, r: &str, why: &str) {
    let mut v = st.recent.lock().unwrap();
    v.insert(0, json!({"ok": ok, "r": r, "why": why}));
    v.truncate(14);
}

fn hamming(a: &str, b: &str) -> u32 {
    match (u64::from_str_radix(a, 16), u64::from_str_radix(b, 16)) {
        (Ok(x), Ok(y)) => (x ^ y).count_ones(),
        _ => 64,
    }
}

// 蒸留ゲート(2026-09-03): このフォルダで過去に採用された画像群=目利き(Claude)判定の蓄積を
// CLIP埋め込みで参照し、明白な傾向外(猫/無関係人物等)をLLMに聞かずに門前払いする。
// 却下側だけを蒸留する(採用の最終判定は目利きに残す=誤採用を増やさない)。
const DISTILL_MIN_SAMPLES: usize = 16; // 採用実績これ未満の若いフォルダではゲートを開かない
const DISTILL_LO: f32 = 0.40; // 採用群とのtop3平均cosineがこれ未満=傾向外。上げるほど強く間引く(舵)

fn emb_from_blob(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// 採用群との類似度: 正規化済み埋め込み同士のdot(=cosine)のtop3平均
fn distill_sim(embs: &[Vec<f32>], e: &[f32]) -> f32 {
    let mut sims: Vec<f32> = embs
        .iter()
        .map(|a| a.iter().zip(e).map(|(x, y)| x * y).sum::<f32>())
        .collect();
    sims.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let k = sims.len().min(3).max(1);
    sims[..k].iter().sum::<f32>() / k as f32
}

/// フィルタ用タグ: フォルダ名+クエリの固有名詞っぽい語。karina/aespa等でメタデータ検索
/// できるようにする(2026-09-03指示)。一般語/数字だけの語は除外、最大6個。
fn crawl_tags(album: &str, query: &str) -> Vec<String> {
    const STOP: [&str; 22] = ["photo", "photos", "image", "images", "picture", "pictures", "real",
        "full", "body", "face", "closeup", "close", "up", "fancam", "facecam", "4k", "hd", "写真",
        "画像", "実写", "直カム", "高画質"];
    let mut out: Vec<String> = vec![album.trim().to_lowercase()];
    for t in query.split_whitespace() {
        let t = t.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
        if t.chars().count() < 2 || t.chars().all(|c| c.is_ascii_digit()) || STOP.contains(&t.as_str()) {
            continue;
        }
        if !out.contains(&t) {
            out.push(t);
        }
        if out.len() >= 6 {
            break;
        }
    }
    out
}

/// 動画フレームのゴミ抜き(ml-hub video_framesと同趣旨): 真っ暗/白飛び/明白なブレだけ門前払い。
/// Laplacian分散は平滑被写体で誤検出する既知の限界があるので閾値は保守的に。最終判定はVLM。
fn frame_looks_ok(img: &image::DynamicImage) -> Result<(), &'static str> {
    let g = img.thumbnail(128, 128).into_luma8();
    let (w, h) = (g.width() as i64, g.height() as i64);
    if w < 3 || h < 3 {
        return Err("小さすぎ");
    }
    let px = |x: i64, y: i64| g.get_pixel(x as u32, y as u32).0[0] as f64;
    let n_all = (w * h) as f64;
    let mean: f64 = g.pixels().map(|p| p.0[0] as f64).sum::<f64>() / n_all;
    if mean < 18.0 {
        return Err("真っ暗");
    }
    if mean > 237.0 {
        return Err("白飛び");
    }
    // コントラスト(ml-hub video_framesのmin_contrast相当): ほぼ単色のフレードイン/暗転を捨てる
    let std = (g.pixels().map(|p| (p.0[0] as f64 - mean).powi(2)).sum::<f64>() / n_all).sqrt();
    if std < 10.0 {
        return Err("コントラスト無し");
    }
    let mut sum = 0.0f64;
    let mut sq = 0.0f64;
    let n = ((w - 2) * (h - 2)) as f64;
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let v = px(x - 1, y) + px(x + 1, y) + px(x, y - 1) + px(x, y + 1) - 4.0 * px(x, y);
            sum += v;
            sq += v * v;
        }
    }
    let var = sq / n - (sum / n) * (sum / n);
    if var < 12.0 {
        return Err("ブレ/ピンぼけ");
    }
    Ok(())
}

// ---------- 本体 ----------
#[allow(clippy::too_many_arguments)]
pub async fn run(
    root: PathBuf,
    client: reqwest::Client,
    st: std::sync::Arc<CrawlState>,
    llm_st: std::sync::Arc<llm::LlmState>,
    enrich_st: std::sync::Arc<enrich::EnrichState>, // ▶は人の指示: 判定のたびバックフィルに道を譲らせる
    album: String,
    goal: String,
    keywords: Vec<String>,
    engines: Vec<String>, // 空=全部。フォルダ毎に「どこから探すか」を選べる
    limits: Limits,
) {
    let started = std::time::Instant::now();
    let set_last = |m: String| *st.last.lock().unwrap() = m;
    // 目利き(内蔵VLM)が居ないと意味ゲートが利かない=ゴミが素通りするので走らせない
    if let Err(e) = enrich::ensure_builtin(&client).await {
        set_last(format!("中止: 内蔵VLM不可({e}) — 意味ゲート無しでは走らせない方針"));
        st.alive.store(false, Relaxed);
        return;
    }
    let db = rusqlite::Connection::open(root.join("store/index.sqlite")).unwrap();
    store::ensure_schema(&db);
    // 既存pHash一覧(近重複ゲート用)
    let mut phashes: Vec<String> = db
        .prepare("SELECT phash FROM images WHERE phash IS NOT NULL")
        .and_then(|mut s| s.query_map([], |r| r.get::<_, String>(0)).map(|rs| rs.filter_map(Result::ok).collect()))
        .unwrap_or_default();
    // 捨てられた子は二度と拾わない(DELは「これは要らん」の学習データ)
    phashes.extend(store::never_again_phashes(&root));
    // 蒸留ゲート用: このフォルダの採用実績のCLIP埋め込み(新しい順512枚)
    let folder_embs: Vec<Vec<f32>> = db
        .prepare("SELECT e.emb FROM embs e JOIN images i ON i.sha1=e.sha1 WHERE i.source=?1 AND length(e.emb)>0 ORDER BY i.ingested DESC LIMIT 512")
        .and_then(|mut s| {
            s.query_map([format!("crawl:{album}")], |r| r.get::<_, Vec<u8>>(0))
                .map(|rs| rs.filter_map(Result::ok).map(|b| emb_from_blob(&b)).collect())
        })
        .unwrap_or_default();
    let distill_on = folder_embs.len() >= DISTILL_MIN_SAMPLES;
    // 顔ID(docs/face-id-design.md): 登録メンバーがいるフォルダは顔照合ゲートが有効になる
    let face_refs = store::face_refs(&db, &album);
    let face_on = cfg!(feature = "faceid") && !face_refs.is_empty();
    if face_on {
        set_last(format!("顔ID有効: {}人の参照で本人照合します", face_refs.len()));
    }
    let (mut done_queries, mut seen_urls, brief_cached) = load_ledger(&root, &album);
    let mut consec_err = 0usize;
    // 💰ブースト: キーがあればClaudeがクエリ生成+目利き(並列)。予算超過で内蔵AIに戻る
    let boost_key = if limits.boost { enrich::mlhub_key("anthropic_api_key") } else { None };
    // 目標の正規化(1回だけ・台帳キャッシュ)。以後のクエリ生成/目利きは正規化済みの目標を見る
    set_last("目標を解釈中…".into());
    let brief = ensure_brief(&root, &client, &llm_st, &goal, &keywords,
                             enrich::mlhub_key("anthropic_api_key").as_deref(), &brief_cached).await;
    if brief != brief_cached {
        save_ledger(&root, &album, &done_queries, &seen_urls, &brief);
    }
    let goal = if brief.is_empty() { goal } else { format!("{goal}\n背景情報(AI解釈): {brief}") };
    // 成人向け目標のフォルダはDDG/Pixabayのセーフサーチを外す(ONだと壊滅的に探せない)
    let nsfw_hint = format!("{goal} {}", keywords.join(" ")).to_lowercase();
    let allow_nsfw = ["エロ", "アダルト", "nsfw", "セクシー", "18禁", "ヌード", "裸", "av女優", "グラビア", "おっぱい"]
        .iter()
        .any(|w| nsfw_hint.contains(w));
    if limits.boost && boost_key.is_none() {
        set_last("💰ブースト指定だがAnthropicキー未設定 — 無料モードで続行".into());
    }
    // 予算ガードは概算(spent_cents)+実測(uusd)の合算をμUSDで比較(実測分が漏れて上限を素通りしたバグ修正 2026-09-03)
    let boost_live = |st: &CrawlState| -> bool {
        st.spent_cents.load(Relaxed) * 10_000 + st.uusd.load(Relaxed) < limits.max_cents * 10_000
    };

    'outer: loop {
        if st.stop.load(Relaxed) || started.elapsed().as_secs() > limits.max_secs
            || st.ingested.load(Relaxed) >= limits.max_n {
            break;
        }
        // キーワード欄の解釈: カンマ区切り(複数)=そのまま検索語として最優先 /
        // 自由文(1件)=LLMへのヒント(書式をユーザーに覚えさせない — AIが翻訳する)
        let verbatim: Vec<String> = if keywords.len() > 1 {
            keywords.iter().filter(|k| !done_queries.contains(k)).cloned().collect()
        } else {
            vec![]
        };
        let (queries, cents) = if !verbatim.is_empty() {
            (verbatim, 0)
        } else {
            set_last("クエリ生成中…(初回は内蔵LLMのDLが走ることがあります)".into());
            gen_queries(&album, &root, &client, &llm_st, &goal, &keywords, &done_queries,
                        boost_key.is_some() && boost_live(&st)).await
        };
        st.spent_cents.fetch_add(cents, Relaxed);
        if queries.is_empty() {
            set_last("クエリが尽きました(台帳リセットで再走可能)".into());
            break;
        }
        let mut qiter = queries.into_iter().peekable();
        let mut prefetched: Option<(String, ImgSearch)> = None; // クエリパイプライン: 次クエリの検索先読み
        while let Some(q) = qiter.next() {
            if st.stop.load(Relaxed) || started.elapsed().as_secs() > limits.max_secs
                || st.ingested.load(Relaxed) >= limits.max_n {
                break 'outer;
            }
            *st.query.lock().unwrap() = q.clone();
            done_queries.push(q.clone());
            let en = |name: &str| {
                if engines.is_empty() { DEFAULT_ENGINES.contains(&name) } else { engines.iter().any(|e| e == name) }
            };
            // ★検索の並列化(2026-09-03指示): エンジンは全部ネットワーク待ち(GPU不使用)なので同時に投げ、
            //   クエリ1本の壁時計を「各エンジンの合計」から「最長の1つ」へ縮める。
            //   ゲート/判定/収蔵は従来どおり直列(順序と台帳の整合が大事)。
            let run_ok = !st.stop.load(Relaxed) && st.ingested.load(Relaxed) < limits.max_n
                && started.elapsed().as_secs() < limits.max_secs;
            let h_yt = (en("youtube") && run_ok).then(|| {
                let sc = root.join("store/.yt_tmp");
                let qq = q.clone();
                tokio::task::spawn_blocking(move || youtube_frames(&sc, &qq, 2))
            });
            let h_grok = if en("x") && run_ok {
                enrich::mlhub_key("xai_api_key").map(|k| {
                    st.spent_cents.fetch_add(3, Relaxed); // Grok x_searchの概算
                    let c = client.clone();
                    let q2 = q.clone();
                    tokio::spawn(async move { x_post_urls(&c, &k, &q2, 8).await })
                })
            } else {
                None
            };
            // 画像5エンジン: 前クエリの判定中に先読み済みならそれを回収、無ければここで発射
            let ImgSearch { ddg: h_ddg, ov: h_ov, wm: h_wm, pxb: h_pxb, pex: h_pex } =
                match prefetched.take() {
                    Some((pq, h)) if pq == q => h,
                    _ => spawn_img_searches(&client, &engines, &q, allow_nsfw),
                };
            st.next_query.lock().unwrap().clear();
            // 動画系(YouTube/X): メディア→フレームを候補として同じ門をくぐらせる(オプトイン)
            let mut media: Vec<(Vec<u8>, String, String, &'static str)> = vec![];
            if let Some(h) = h_yt {
                set_last(format!("YouTube検索応答待ち「{q}」…"));
                match h.await.unwrap_or_else(|_| Err("yt join".into())) {
                    Ok(list) => media.extend(list.into_iter().map(|(d, u, t)| (d, u, t, "youtube"))),
                    Err(e) => set_last(format!("YouTube: {e}")),
                }
            }
            if let Some(h) = h_grok {
                {
                    set_last(format!("X(Grok)検索応答待ち「{q}」…"));
                    match h.await.unwrap_or_else(|_| Err("x join".into())) {
                        Ok(urls) => {
                            let fresh: Vec<String> =
                                urls.into_iter().filter(|u| !seen_urls.contains(&url_key(u))).collect();
                            if !fresh.is_empty() {
                                // 画像ポスト=配信CDNから直接(速い・確実)。動画だけyt-dlpへ
                                let mut video_posts = vec![];
                                for pu in &fresh {
                                    let photos = x_syndication_photos(&client, pu).await;
                                    if photos.is_empty() {
                                        video_posts.push(pu.clone());
                                        continue;
                                    }
                                    set_last(format!("Xポスト画像{}枚を取得中…", photos.len()));
                                    for purl in photos {
                                        if let Ok(resp) = client.get(&purl).header("User-Agent", BROWSER_UA)
                                            .timeout(std::time::Duration::from_secs(20)).send().await
                                        {
                                            if let Ok(b) = resp.bytes().await {
                                                media.push((b.to_vec(), pu.clone(), String::new(), "x"));
                                            }
                                        }
                                    }
                                }
                                if !video_posts.is_empty() {
                                    set_last(format!("Xポスト動画{}件を取得中…", video_posts.len()));
                                    let sc = root.join("store/.yt_tmp");
                                    let list = tokio::task::spawn_blocking(move || media_frames_from_urls(&sc, &video_posts))
                                        .await
                                        .unwrap_or_default();
                                    media.extend(list.into_iter().map(|(d, u, t)| (d, u, t, "x")));
                                }
                            }
                        }
                        Err(e) => set_last(format!("X(Grok): {e}")),
                    }
                }
            }
            {
                {
                    {
                        let list = media;
                        st.found.fetch_add(list.len(), Relaxed);
                        let mut done_videos: std::collections::HashSet<String> = std::collections::HashSet::new();
                        // 1本の動画からの採用数(多様性の強制)。「同じような画像だらけ」の根治は
                        // ①この上限 ②動画専用の広いpHash門 ③ブレ/露出の門前払い の三段(2026-09-03)
                        let mut kept_per_video: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
                        for (i, (data, vurl, title, eng)) in list.into_iter().enumerate() {
                            if st.stop.load(Relaxed) || st.ingested.load(Relaxed) >= limits.max_n {
                                break;
                            }
                            let vk = url_key(&vurl);
                            if seen_urls.contains(&vk) {
                                continue; // 前のrunで見た動画
                            }
                            done_videos.insert(vk.clone());
                            st.checked.fetch_add(1, Relaxed);
                            let Ok(img) = image::load_from_memory(&data) else {
                                st.rejected.fetch_add(1, Relaxed);
                                continue;
                            };
                            let (w, h) = (img.width(), img.height());
                            let fk = url_key(&format!("{vurl}#{i}"));
                            if w.min(h) < MIN_SIDE || w.max(h) > MAX_ASPECT * w.min(h).max(1) {
                                st.rejected.fetch_add(1, Relaxed);
                                continue;
                            }
                            // ml-hub video_frames と同趣旨のゴミ抜き: 真っ暗/白飛び/明白なブレだけ門前払い
                            // (Laplacianは平滑被写体で誤検出する既知の限界 → 閾値は保守的に。最終判定はVLM)
                            if let Err(why) = frame_looks_ok(&img) {
                                st.rejected.fetch_add(1, Relaxed);
                                save_reject_thumb(&root, &fk, &img);
                                push_recent(&st, false, &fk, why);
                                continue;
                            }
                            if *kept_per_video.get(&vk).unwrap_or(&0) >= MAX_KEEP_PER_VIDEO {
                                st.rejected.fetch_add(1, Relaxed);
                                push_recent(&st, false, &fk, "この動画からは採用上限");
                                continue;
                            }
                            let ph = store::phash64(&img);
                            if phashes.iter().any(|p| hamming(p, &ph) <= PHASH_NEAR_VIDEO) {
                                st.rejected.fetch_add(1, Relaxed);
                                save_reject_thumb(&root, &fk, &img);
                                push_recent(&st, false, &fk, "そっくりフレーム");
                                continue; // そっくりフレーム/既所持
                            }
                            // 蒸留ゲート: 採用群と傾向が違いすぎる子はLLMに聞かず門前払い(無料・数十ms)
                            if distill_on {
                                if let Some(e) = crate::onnx::embed(&root, &img) {
                                    if distill_sim(&folder_embs, &e) < DISTILL_LO {
                                        st.rejected.fetch_add(1, Relaxed);
                                        save_reject_thumb(&root, &fk, &img);
                                        push_recent(&st, false, &fk, "傾向外(蒸留ゲート・無料)");
                                        continue;
                                    }
                                }
                            }
                            // 顔ゲート: 登録メンバーと照合(無料・決定的)。不一致=門前払い、一致=本人タグ、
                            // 顔なし/中間帯=従来どおりLLMへ。feature "faceid" 無効ビルドでは常に素通し
                            let face_who: Option<String> = match face_gate(face_on, &face_refs, &img) {
                                Ok(w) => w,
                                Err(()) => {
                                    st.rejected.fetch_add(1, Relaxed);
                                    save_reject_thumb(&root, &fk, &img);
                                    push_recent(&st, false, &fk, "登録メンバー不一致(顔ID・無料)");
                                    continue;
                                }
                            };
                            let verdict = match boost_key.as_ref().filter(|_| boost_live(&st)) {
                                Some(bk) => {
                                    let mut r = judge_claude(&client, bk, &img, &goal, &keywords, &limits.judge_model).await;
                                    if r.is_err() {
                                        r = judge_claude(&client, bk, &img, &goal, &keywords, &limits.judge_model).await; // 一過性エラーは1回だけ再試行
                                    }
                                    match r {
                                        Ok(v) => {
                                            st.uusd.fetch_add((v.2 * 1e6) as usize, Relaxed);
                                    st.utok.fetch_add(v.3 as usize, Relaxed);
                                            st.utok.fetch_add(v.3 as usize, Relaxed);
                                            Ok(v)
                                        }
                                        // Claudeが落ちても7Bに採用を肩代わりさせない(狼がIVEに入った実害 2026-09-03)
                                        Err(e) => Err(format!("Claude目利き不可・この1枚は見送り: {e}")),
                                    }
                                }
                                None => {
                                    enrich_st.user_priority(10);
                                    while st.ui_recent(8) && !st.stop.load(Relaxed) {
                                        tokio::time::sleep(std::time::Duration::from_secs(1)).await; // 閲覧中は譲る
                                    }
                                    judge_builtin(&client, &img, &goal, &keywords).await.map(|(m, qq)| (m, qq, 0.0, 0u64))
                                }
                            };
                            match verdict {
                                Ok((true, quality, jc, _)) if quality >= limits.min_quality => {
                                    let mut ctags = crawl_tags(&album, &q);
                                    if let Some(w) = &face_who {
                                        if !ctags.contains(w) {
                                            ctags.push(w.clone()); // 顔IDの本人タグ(出自タグと違い顔を見た根拠付き)
                                        }
                                    }
                                    let mut extra = json!({"rights": "unknown",
                                        "crawl": {"url": vurl, "landing": vurl, "title": title,
                                                  "query": q, "engine": eng, "album": album,
                                                  "tags": ctags}});
                                    if let Some(w) = &face_who {
                                        extra["face_ids"] = json!([w]); // 顔IDで本人確認済み(サイドカー正本に明示)
                                    }
                                    if jc > 0.0 {
                                        extra["cost"] = json!({"usd": (jc * 10000.0).round() / 10000.0, "by": format!("boost:{}", limits.judge_model)});
                                    }
                                    if let Ok(sha) = store::ingest_bytes(&root, &db, &data, "jpg", &format!("crawl:{album}"), &extra) {
                                        phashes.push(ph);
                                        *kept_per_video.entry(vk.clone()).or_insert(0) += 1;
                                        st.ingested.fetch_add(1, Relaxed);
                                        push_recent(&st, true, &sha, &format!("採用 q{quality} ({eng})"));
                                    } else {
                                        st.rejected.fetch_add(1, Relaxed);
                                    }
                                }
                                Ok((matched, quality, _, _)) => {
                                    st.rejected.fetch_add(1, Relaxed);
                                    save_reject_thumb(&root, &fk, &img);
                                    let why = if !matched { "目標と不一致".to_string() } else { format!("品質低 q{quality}") };
                                    push_recent(&st, false, &fk, &why);
                                }
                                Err(e) => {
                                    st.errors.fetch_add(1, Relaxed);
                                    save_reject_thumb(&root, &fk, &img);
                                    push_recent(&st, false, &fk, "判定不可(見送り)");
                                    set_last(format!("judge: {e}"));
                                }
                            }
                        }
                        seen_urls.extend(done_videos);
                    }
                }
            }
            let mut cands = vec![];
            if let Some(h) = h_ddg {
                match h.await.unwrap_or_else(|_| Err("ddg join".into())) {
                    Ok(mut v) => { cands.append(&mut v); consec_err = 0; }
                    Err(e) => { st.errors.fetch_add(1, Relaxed); consec_err += 1; set_last(format!("DDG: {e}")); }
                }
            }
            if let Some(h) = h_ov {
                if let Ok(Ok(mut v)) = h.await { cands.append(&mut v); }
            }
            if let Some(h) = h_wm {
                if let Ok(Ok(mut v)) = h.await { cands.append(&mut v); }
            }
            if let Some(h) = h_pxb {
                if let Ok(Ok(mut v)) = h.await { cands.append(&mut v); }
            }
            if let Some(h) = h_pex {
                if let Ok(Ok(mut v)) = h.await { cands.append(&mut v); }
            }
            st.found.fetch_add(cands.len(), Relaxed);
            // ★クエリパイプライン: ここからのゲート/VLM判定(重い)の間に次クエリの検索を先読み。
            //   現クエリの検索は全て回収済みなので各エンジン同時1本は崩れない(DDG BAN回避)
            if let Some(nq) = qiter.peek() {
                if !st.stop.load(Relaxed) && started.elapsed().as_secs() < limits.max_secs
                    && st.ingested.load(Relaxed) < limits.max_n {
                    *st.next_query.lock().unwrap() = nq.clone();
                    prefetched = Some((nq.clone(), spawn_img_searches(&client, &engines, nq, allow_nsfw)));
                }
            }
            if consec_err >= limits.max_errors {
                set_last("連続エラーで自動停止(検索が絞られてる可能性)".into());
                break 'outer;
            }
            // 未見だけに絞ってから8並列でDL(ml-hub並みのスクレイピング速度)。ゲート/判定は直列のまま
            let fresh: Vec<Cand> = cands
                .into_iter()
                .filter(|c| {
                    let uk = url_key(&c.url);
                    if seen_urls.contains(&uk) {
                        false
                    } else {
                        seen_urls.insert(uk);
                        true
                    }
                })
                .collect();
            for chunk in fresh.chunks(8) {
                if st.stop.load(Relaxed) || started.elapsed().as_secs() > limits.max_secs
                    || st.ingested.load(Relaxed) >= limits.max_n {
                    break 'outer;
                }
                let handles: Vec<_> = chunk
                    .iter()
                    .cloned()
                    .map(|c| {
                        let client = client.clone();
                        tokio::spawn(async move {
                            if !is_safe_url(&c.url).await {
                                return None;
                            }
                            let resp = client
                                .get(&c.url)
                                .header("User-Agent", BROWSER_UA)
                                .timeout(std::time::Duration::from_secs(20))
                                .send()
                                .await
                                .ok()?;
                            let ctype = resp.headers().get("content-type")
                                .and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                            if !ctype.starts_with("image/") {
                                return None;
                            }
                            let data = resp.bytes().await.ok()?;
                            Some((c, data))
                        })
                    })
                    .collect();
                // 第1段: 決定的ゲート(サイズ/縦横比/近重複)を通った候補を貯める
                let mut passed: Vec<(Cand, Vec<u8>, image::DynamicImage, String, String, Option<String>)> = vec![];
                for h in handles {
                let Ok(Some((c, data))) = h.await else { continue };
                if st.stop.load(Relaxed) || started.elapsed().as_secs() > limits.max_secs
                    || st.ingested.load(Relaxed) >= limits.max_n {
                    break 'outer;
                }
                let uk = url_key(&c.url);
                st.checked.fetch_add(1, Relaxed);
                if data.len() < MIN_BYTES {
                    st.rejected.fetch_add(1, Relaxed);
                    continue;
                }
                let Ok(img) = image::load_from_memory(&data) else {
                    st.rejected.fetch_add(1, Relaxed);
                    continue;
                };
                let (w, h) = (img.width(), img.height());
                if w.min(h) < MIN_SIDE || w.max(h) > MAX_ASPECT * w.min(h).max(1) {
                    st.rejected.fetch_add(1, Relaxed);
                    save_reject_thumb(&root, &uk, &img);
                    push_recent(&st, false, &uk, if w.min(h) < MIN_SIDE { "小さすぎ" } else { "縦横比" });
                    continue;
                }
                // 重複(sha1完全一致は台帳、近重複はpHash)
                let ph = store::phash64(&img);
                if phashes.iter().any(|p| hamming(p, &ph) <= PHASH_NEAR) {
                    st.rejected.fetch_add(1, Relaxed);
                    save_reject_thumb(&root, &uk, &img);
                    push_recent(&st, false, &uk, "ほぼ同じ絵を所持");
                    continue;
                }
                // 蒸留ゲート: 採用群と傾向が違いすぎる子はLLM判定に回さない(無料・数十ms)
                if distill_on {
                    if let Some(e) = crate::onnx::embed(&root, &img) {
                        if distill_sim(&folder_embs, &e) < DISTILL_LO {
                            st.rejected.fetch_add(1, Relaxed);
                            save_reject_thumb(&root, &uk, &img);
                            push_recent(&st, false, &uk, "傾向外(蒸留ゲート・無料)");
                            continue;
                        }
                    }
                }
                // 顔ゲート: 登録メンバーと照合(無料・決定的)。不一致=門前払い、一致=本人タグ、
                // 顔なし/中間帯=従来どおりLLMへ。feature "faceid" 無効ビルドでは常に素通し
                let face_who: Option<String> = match face_gate(face_on, &face_refs, &img) {
                    Ok(w) => w,
                    Err(()) => {
                        st.rejected.fetch_add(1, Relaxed);
                        save_reject_thumb(&root, &uk, &img);
                        push_recent(&st, false, &uk, "登録メンバー不一致(顔ID・無料)");
                        continue;
                    }
                };
                passed.push((c, data.to_vec(), img, uk, ph, face_who));
                }
                // 第2段: 意味ゲート(目標の意味で判定 — キーワード一致で全ゴミだった教訓)。
                // チャンク丸ごと並列投入: 💰=Claude並列、無料=ollamaへパイプライン
                // (ollamaが直列でも送信/前処理が重なって隙間が消える。判定直列が収集の律速だった)
                let use_claude = boost_key.is_some() && boost_live(&st);
                if !use_claude && !passed.is_empty() {
                    enrich_st.user_priority(20); // バックフィルにチャンク分まとめて道を譲らせる
                    // 内蔵VLM判定はCPUを全部食う。閲覧中(直近8秒)はパースしてUIを窒息させない
                    // (2026-09-03: IVE収集の内蔵フォールバックでllamaが16コア占有→グループ切替不能の実害)
                    while st.ui_recent(8) && !st.stop.load(Relaxed) {
                        set_last("閲覧中はAI判定を一時停止(道を譲っています)…".into());
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
                let mut judge_tasks = vec![];
                for (i, (_c, _data, img, _uk, _ph, _fw)) in passed.iter().enumerate() {
                    let client = client.clone();
                    let key = boost_key.clone();
                    let goal = goal.clone();
                    let hints = keywords.clone();
                    let img = img.clone();
                    let jm = limits.judge_model.clone();
                    judge_tasks.push(tokio::spawn(async move {
                        let r = match (use_claude, key) {
                            (true, Some(k)) => judge_claude(&client, &k, &img, &goal, &hints, &jm).await,
                            _ => judge_builtin(&client, &img, &goal, &hints).await.map(|(m, q)| (m, q, 0.0, 0u64)),
                        };
                        (i, r)
                    }));
                }
                let mut verdicts: Vec<Option<Result<(bool, i64, f64, u64), String>>> = (0..passed.len()).map(|_| None).collect();
                for t in judge_tasks {
                    if let Ok((i, r)) = t.await {
                        if let Ok((_, _, usd, tok)) = &r {
                            st.uusd.fetch_add((usd * 1e6) as usize, Relaxed); // APIのusageから実測計上
                            st.utok.fetch_add(*tok as usize, Relaxed);
                        }
                        verdicts[i] = Some(r);
                    }
                }
                for (i, (c, data, img, uk, ph, face_who)) in passed.into_iter().enumerate() {
                if st.stop.load(Relaxed) || started.elapsed().as_secs() > limits.max_secs
                    || st.ingested.load(Relaxed) >= limits.max_n {
                    break 'outer;
                }
                // 同一チャンク内のそっくり画像(第1段はまとめて通るので再チェック)
                if phashes.iter().any(|p| hamming(p, &ph) <= PHASH_NEAR) {
                    st.rejected.fetch_add(1, Relaxed);
                    continue;
                }
                let verdict = match verdicts[i].take() {
                    Some(Ok(v)) => Ok(v),
                    // 失敗のフォールバック: 💰時はClaudeを1回だけ再試行(7Bに採用を肩代わりさせると
                    // 狼がIVEに入る 2026-09-03の実害)。無料経路は内蔵VLMを再試行。
                    _ => {
                        if use_claude {
                            match judge_claude(&client, boost_key.as_ref().unwrap(), &img, &goal, &keywords, &limits.judge_model).await {
                                Ok(v) => {
                                    st.uusd.fetch_add((v.2 * 1e6) as usize, Relaxed);
                                    st.utok.fetch_add(v.3 as usize, Relaxed);
                                    Ok(v)
                                }
                                Err(e) => Err(format!("Claude目利き不可・この1枚は見送り: {e}")),
                            }
                        } else {
                            enrich_st.user_priority(10);
                            judge_builtin(&client, &img, &goal, &keywords).await.map(|(m, q)| (m, q, 0.0, 0u64))
                        }
                    }
                };
                match verdict {
                    Ok((true, quality, jc, _)) if quality >= limits.min_quality => {
                        let ext = match &data[..data.len().min(12)] {
                            d if d.starts_with(b"\x89PNG") => "png",
                            d if d.starts_with(b"RIFF") => "webp",
                            _ => "jpg",
                        };
                        let mut ctags = crawl_tags(&album, &q);
                        if let Some(w) = &face_who {
                            if !ctags.contains(w) {
                                ctags.push(w.clone()); // 顔IDの本人タグ
                            }
                        }
                        let mut extra = json!({
                            "rights": c.license,
                            "crawl": {"url": c.url, "landing": c.landing, "title": c.title,
                                      "query": q, "engine": c.engine, "album": album,
                                      "tags": ctags},
                        });
                        if let Some(w) = &face_who {
                            extra["face_ids"] = json!([w]); // 顔IDで本人確認済み(サイドカー正本に明示)
                        }
                        if jc > 0.0 {
                            // 画像単位の実費(ライトボックス取得費/facets累計に乗る)
                            extra["cost"] = json!({"usd": (jc * 10000.0).round() / 10000.0, "by": format!("boost:{}", limits.judge_model)});
                        }
                        let r = store::ingest_bytes(&root, &db, &data, ext, &format!("crawl:{album}"), &extra);
                        if let Ok(sha) = r {
                            phashes.push(ph);
                            st.ingested.fetch_add(1, Relaxed);
                            push_recent(&st, true, &sha, &format!("採用 q{quality}"));
                            set_last(format!("✅ {} (q{quality})", c.title.chars().take(40).collect::<String>()));
                        } else {
                            st.rejected.fetch_add(1, Relaxed);
                        }
                    }
                    Ok((matched, quality, _, _)) => {
                        st.rejected.fetch_add(1, Relaxed);
                        save_reject_thumb(&root, &uk, &img);
                        let why = if !matched { "目標と不一致".to_string() } else { format!("品質低 q{quality}") };
                        push_recent(&st, false, &uk, &why);
                    }
                    Err(e) => {
                        st.errors.fetch_add(1, Relaxed);
                        save_reject_thumb(&root, &uk, &img);
                        push_recent(&st, false, &uk, "判定不可(見送り)");
                        set_last(format!("judge: {e}"));
                    }
                }
                }
            }
            save_ledger(&root, &album, &done_queries, &seen_urls, &brief);
        }
    }
    save_ledger(&root, &album, &done_queries, &seen_urls, &brief);
    // アルバムにlast_runを刻む(エージェントの調子が見える)
    let ap = root.join("store/albums").join(format!("{album}.json"));
    if let Ok(t) = std::fs::read_to_string(&ap) {
        if let Ok(mut a) = serde_json::from_str::<Value>(&t) {
            a["last_run"] = st.status();
            a["last_run"]["ts"] = json!(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64());
            // 💰使った分をフォルダに積む(上限判定は累計に対して効く)+全体台帳にも積む
            let run_usd = st.spent_cents.load(Relaxed) as f64 / 100.0 + st.uusd.load(Relaxed) as f64 / 1e6;
            if run_usd > 0.0 {
                let prev = a["agent"]["spent_usd"].as_f64().unwrap_or(0.0);
                a["agent"]["spent_usd"] = json!(((prev + run_usd) * 1000.0).round() / 1000.0);
                // トークンも累計(表示はトークン主体・$はツールチップ 2026-09-03指示)
                let ptok = a["agent"]["spent_tok"].as_u64().unwrap_or(0);
                a["agent"]["spent_tok"] = json!(ptok + st.utok.load(Relaxed) as u64);
                let lp = root.join("store/crawl/spend.json");
                let total = std::fs::read_to_string(&lp)
                    .ok()
                    .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                    .and_then(|v| v["total_usd"].as_f64())
                    .unwrap_or(0.0)
                    + run_usd;
                let _ = std::fs::write(&lp, json!({"total_usd": (total * 1000.0).round() / 1000.0}).to_string());
            }
            let _ = std::fs::write(&ap, serde_json::to_string_pretty(&a).unwrap());
        }
    }
    set_last(format!("完了: {}枚収蔵 / {}検査 / {}分",
        st.ingested.load(Relaxed), st.checked.load(Relaxed), started.elapsed().as_secs() / 60));
    st.next_query.lock().unwrap().clear();
    st.alive.store(false, Relaxed);
}

// ---------- テスト(決定的な芯だけ: 関連度/ハミング/クエリ浄化/フレーム門) ----------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relevance_scores() {
        assert!(yt_relevance("aespa GISELLE facecam 4K", "aespa giselle") > 0.99);
        assert_eq!(yt_relevance("funny cat video", "aespa giselle"), 0.0);
        assert_eq!(yt_relevance("【エスパ】カリナ直カム", "エスパ"), 1.0); // CJK1語=部分一致
        assert_eq!(yt_relevance("", "query"), 0.0);
    }

    #[test]
    fn hamming_hex() {
        assert_eq!(hamming("0", "0"), 0);
        assert_eq!(hamming("f", "0"), 4);
        assert_eq!(hamming("zz", "0"), 64); // 壊れハッシュは最遠扱い=重複と見なさない
    }

    #[test]
    fn clean_queries_filters() {
        let qs = clean_queries(
            vec!["ab".into(), "良いクエリ".into(), "化け\u{FFFD}文字".into(), "使用済み".into()],
            &["使用済み".to_string()],
        );
        assert_eq!(qs, vec!["良いクエリ".to_string()]);
    }

    #[test]
    fn parse_queries_json() {
        assert_eq!(parse_queries(r#"前置き {"queries": ["aespa 직캠", "karina photo"]} 後置き"#),
                   vec!["aespa 직캠".to_string(), "karina photo".to_string()]);
        assert!(parse_queries("JSONなし").is_empty());
    }

    #[test]
    fn frame_gates() {
        let black = image::DynamicImage::ImageLuma8(image::GrayImage::from_pixel(64, 64, image::Luma([0])));
        assert_eq!(frame_looks_ok(&black), Err("真っ暗"));
        let white = image::DynamicImage::ImageLuma8(image::GrayImage::from_pixel(64, 64, image::Luma([255])));
        assert_eq!(frame_looks_ok(&white), Err("白飛び"));
        let flat = image::DynamicImage::ImageLuma8(image::GrayImage::from_pixel(64, 64, image::Luma([128])));
        assert_eq!(frame_looks_ok(&flat), Err("コントラスト無し"));
        let noisy = image::DynamicImage::ImageLuma8(image::GrayImage::from_fn(64, 64, |x, y| {
            image::Luma([((x * 37 + y * 91) % 251) as u8])
        }));
        assert_eq!(frame_looks_ok(&noisy), Ok(()));
    }
}

/// 顔ゲート(収集時)。Ok(Some)=本人確定タグ / Ok(None)=素通し(顔なし・中間帯・無効) / Err=別人で門前払い
#[cfg(feature = "faceid")]
fn face_gate(face_on: bool, face_refs: &[(String, Vec<Vec<f32>>)], img: &image::DynamicImage) -> Result<Option<String>, ()> {
    if !face_on {
        return Ok(None);
    }
    let ffs = crate::faceid::detect_faces(img);
    if ffs.is_empty() {
        return Ok(None);
    }
    let mut best = -1.0f32;
    let mut who: Option<String> = None;
    for f in ffs.iter().take(4) {
        if let Some(e) = crate::faceid::embed_face(img, &f.kps) {
            for (nm, rs) in face_refs {
                let sim = crate::faceid::best_sim(&e, rs);
                if sim > best {
                    best = sim;
                    who = Some(nm.clone());
                }
            }
        }
    }
    if best < crate::faceid::FACE_DIFF {
        return Err(()); // 別人
    }
    if best < crate::faceid::FACE_SAME {
        return Ok(None); // 中間帯: 本人タグは付けずLLMに任せる
    }
    Ok(who)
}
/// feature "faceid" 無効ビルド: 顔ゲートは常に素通し(販売ビルド用・insightfaceモデル非同梱)
#[cfg(not(feature = "faceid"))]
fn face_gate(_face_on: bool, _face_refs: &[(String, Vec<Vec<f32>>)], _img: &image::DynamicImage) -> Result<Option<String>, ()> {
    Ok(None)
}

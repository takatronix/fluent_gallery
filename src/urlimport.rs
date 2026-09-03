//! URL取り込み — ユーザーが指定した1ページ(または画像URL)から、そのページに載っている画像を取り込む。
//! 検索エンジンを自動巡回する収集(crawl.rs)と違い、利用者自身が場所を指定する「ブラウザで保存」相当の操作。
//! 規約でダウンロードが禁止されている媒体(YouTube/X/Instagram等)はホストで門前払い(App Store 5.2.3 対応)。

use reqwest::Url;

/// ダウンロードが規約で禁止されている/ストア審査で拒否される媒体。サブドメインも含めて拒否
const BLOCKED_HOSTS: [&str; 12] = [
    "youtube.com", "youtu.be", "x.com", "twitter.com", "twimg.com", "instagram.com", "cdninstagram.com",
    "facebook.com", "fbcdn.net", "tiktok.com", "threads.net", "pinterest.com",
];

pub fn blocked_host(host: &str) -> Option<&'static str> {
    let h = host.to_ascii_lowercase();
    BLOCKED_HOSTS.iter().copied().find(|b| h == *b || h.ends_with(&format!(".{b}")))
}

/// HTMLから画像候補URLを抜く(依存ゼロの素朴なタグ走査)。<img src/srcset/data-src>, og:image, 画像拡張子の <a href>
/// 戻り: (ページタイトル, 絶対URL一覧・出現順・重複なし)
pub fn extract(base: &Url, html: &str) -> (String, Vec<String>) {
    let mut title = String::new();
    if let Some(a) = html.find("<title") {
        if let Some(b) = html[a..].find('>') {
            let rest = &html[a + b + 1..];
            if let Some(e) = rest.find("</title") {
                title = decode_entities(rest[..e].trim());
            }
        }
    }
    let mut out: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |u: &str| {
        let u = decode_entities(u.trim());
        if u.is_empty() || u.starts_with("data:") || u.starts_with("javascript:") { return; }
        let Ok(abs) = base.join(&u) else { return };
        if !matches!(abs.scheme(), "http" | "https") { return; }
        let mut abs = abs; abs.set_fragment(None);
        let mut s = abs.to_string();
        // Wikimedia のサムネ(.../commons/thumb/a/ab/X.jpg/220px-X.jpg)は原寸(.../commons/a/ab/X.jpg)に置き換える
        if let Some(i) = s.find("/thumb/") {
            if s.contains("upload.wikimedia.org") {
                let tail = &s[i + 7..];
                if let Some(cut) = tail.rfind('/') {
                    s = format!("{}/{}", &s[..i], &tail[..cut]);
                }
            }
        }
        let lower = s.to_ascii_lowercase();
        if lower.ends_with(".svg") || lower.contains(".svg?") { return; }
        if seen.insert(s.clone()) { out.push(s); }
    };
    let mut i = 0;
    let bytes = html.as_bytes();
    while let Some(off) = html[i..].find('<') {
        let start = i + off;
        let Some(len) = html[start..].find('>') else { break };
        let tag = &html[start + 1..start + len];
        i = start + len + 1;
        let name = tag.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("").to_ascii_lowercase();
        match name.as_str() {
            "img" | "source" => {
                for k in ["src", "data-src", "data-original", "data-lazy-src"] {
                    if let Some(v) = attr(tag, k) { push(v); }
                }
                for k in ["srcset", "data-srcset"] {
                    if let Some(v) = attr(tag, k) {
                        // "url 320w, url2 640w" → 最後(最大)の候補を優先し、全部拾う
                        let mut cands: Vec<&str> = v.split(',').filter_map(|p| p.trim().split_whitespace().next()).collect();
                        cands.reverse();
                        for c in cands { push(c); }
                    }
                }
            }
            "meta" => {
                let prop = attr(tag, "property").or_else(|| attr(tag, "name")).unwrap_or("").to_ascii_lowercase();
                if prop == "og:image" || prop == "og:image:secure_url" || prop == "twitter:image" {
                    if let Some(v) = attr(tag, "content") { push(v); }
                }
            }
            "a" => {
                if let Some(v) = attr(tag, "href") {
                    let l = v.to_ascii_lowercase();
                    let path = l.split(['?', '#']).next().unwrap_or("");
                    if [".jpg", ".jpeg", ".png", ".webp", ".gif", ".bmp", ".tif", ".tiff"].iter().any(|e| path.ends_with(e)) { push(v); }
                }
            }
            _ => {}
        }
        if i >= bytes.len() { break; }
    }
    (title, out)
}

/// タグ文字列から属性値(引用符あり/なし)を取る。大文字小文字は無視
fn attr<'a>(tag: &'a str, key: &str) -> Option<&'a str> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(p) = lower[from..].find(key) {
        let at = from + p;
        let before_ok = at == 0 || !lower.as_bytes()[at - 1].is_ascii_alphanumeric() && lower.as_bytes()[at - 1] != b'-';
        let after = &lower[at + key.len()..];
        let after_t = after.trim_start();
        if before_ok && after_t.starts_with('=') {
            let vstart = at + key.len() + (after.len() - after_t.len()) + 1;
            let v = tag[vstart..].trim_start();
            let vstart = tag.len() - v.len();
            return Some(match v.chars().next() {
                Some(q @ ('"' | '\'')) => {
                    let inner = &tag[vstart + 1..];
                    let end = inner.find(q).unwrap_or(inner.len());
                    &inner[..end]
                }
                _ => v.split(|c: char| c.is_whitespace()).next().unwrap_or(""),
            });
        }
        from = at + key.len();
    }
    None
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&").replace("&quot;", "\"").replace("&#39;", "'").replace("&lt;", "<").replace("&gt;", ">")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn picks_images_and_blocks() {
        let base = Url::parse("https://example.com/a/b.html").unwrap();
        let html = r#"<html><head><title>T &amp; U</title><meta property="og:image" content="/og.jpg"></head>
            <body><img src="x.png"><img data-src='//cdn.example.org/y.jpg' srcset="s.jpg 320w, l.jpg 1024w">
            <a href="/files/z.JPG?x=1">z</a><img src="data:image/png;base64,AAA"><img src="v.svg"></body></html>"#;
        let (t, u) = extract(&base, html);
        assert_eq!(t, "T & U");
        assert_eq!(u, vec![
            "https://example.com/og.jpg", "https://example.com/a/x.png", "https://cdn.example.org/y.jpg",
            "https://example.com/a/l.jpg", "https://example.com/a/s.jpg", "https://example.com/files/z.JPG?x=1",
        ]);
        let (_, w) = extract(&base, r#"<img src="https://upload.wikimedia.org/wikipedia/commons/thumb/2/25/Siam.jpg/220px-Siam.jpg">"#);
        assert_eq!(w, vec!["https://upload.wikimedia.org/wikipedia/commons/2/25/Siam.jpg"]);
        assert_eq!(blocked_host("www.youtube.com"), Some("youtube.com"));
        assert_eq!(blocked_host("pbs.twimg.com"), Some("twimg.com"));
        assert_eq!(blocked_host("commons.wikimedia.org"), None);
    }
}

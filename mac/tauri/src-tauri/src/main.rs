//! Fluent Gallery の Mac アプリ殻(Tauri 2)。
//! 同梱した fluent_gallery サーバ(サイドカー)を Application Support 配下で起動し、
//! WKWebView の窓で http://127.0.0.1:PORT/ を表示する。⌘Q でサーバごと終了。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::webview::DownloadEvent;
use tauri::{Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) { out.push(v); i += 3; continue; }
        }
        out.push(b[i]); i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

struct Server(Mutex<Option<Child>>);

fn port_open(port: u16) -> bool {
    TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(300)).is_ok()
}

fn data_dir() -> PathBuf {
    std::env::var("FG_DATA").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Library/Application Support/FluentGallery")
    })
}

fn main() {
    let port: u16 = std::env::var("FG_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8790);
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init()) // 取得元ページ等の外部リンクを既定ブラウザで開く(WKWebViewは新規窓を黙って捨てる)
        .manage(Server(Mutex::new(None)))
        .setup(move |app| {
            let data = data_dir();
            std::fs::create_dir_all(data.join("store"))?;
            std::fs::create_dir_all(data.join("engine/models"))?;
            // サーバは root/web/index.html を毎回読む(no-store)。バンドルの Resources/web をリンクで見せる
            let res_web = app.path().resource_dir()?.join("web");
            let link = data.join("web");
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(&res_web, &link)?;
            if !port_open(port) {
                // externalBin は本体と同じ Contents/MacOS/ に置かれる
                let bin = std::env::current_exe()?.parent().unwrap().join("fluent_gallery");
                let log = std::fs::OpenOptions::new().create(true).append(true).open(data.join("fluent_gallery.log"))?;
                let child = Command::new(&bin)
                    .current_dir(&data)
                    .env("PORT", port.to_string())
                    .stdout(log.try_clone()?)
                    .stderr(log)
                    .spawn()?;
                *app.state::<Server>().0.lock().unwrap() = Some(child);
                let t0 = Instant::now();
                while !port_open(port) && t0.elapsed() < Duration::from_secs(20) {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            let url = format!("http://127.0.0.1:{port}/").parse().unwrap();
            let port_s = port.to_string();
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
                // 窓の中で外部サイトへ遷移しようとしたら既定ブラウザへ逃がす(UIは 127.0.0.1 だけ)
                .on_navigation(move |u| {
                    let local = matches!(u.host_str(), Some("127.0.0.1") | Some("localhost")) && u.port().map(|p| p.to_string()).as_deref() == Some(port_s.as_str());
                    if !local && matches!(u.scheme(), "http" | "https") {
                        let _ = tauri_plugin_opener::open_url(u.as_str(), None::<&str>);
                    }
                    local
                })
                // ダウンロード(原本/zip)は ~/Downloads へ。保存名はURL末尾(/dl/{sha}/{name} と /export/{id}/{name}.zip)
                .on_download(|_wv, ev| {
                    if let DownloadEvent::Requested { url, destination } = ev {
                        let name = url.path_segments().and_then(|mut s| s.next_back()).map(pct_decode).filter(|s| !s.is_empty()).unwrap_or_else(|| "download".into());
                        let dir = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Downloads");
                        let _ = std::fs::create_dir_all(&dir);
                        let mut p = dir.join(&name);
                        let (stem, ext) = match name.rsplit_once('.') { Some((a, b)) => (a.to_string(), format!(".{b}")), None => (name.clone(), String::new()) };
                        let mut n = 2;
                        while p.exists() { p = dir.join(format!("{stem} ({n}){ext}")); n += 1; }
                        *destination = p;
                    }
                    true
                })
                .title("Fluent Gallery")
                .inner_size(1440.0, 920.0)
                .min_inner_size(800.0, 500.0)
                .build()?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("tauri build")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                if let Some(mut c) = app.state::<Server>().0.lock().unwrap().take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
            }
        });
}

//! サンプルデータ取得 — 権利がクリア(CC0 / パブリックドメイン)な公開コレクションの API から画像を取得して収蔵する。
//! 旧「プリセット」(Linux機の私物フォルダ一括収蔵)の置き換え。キー不要・出典とライセンスはサイドカー(rights/credit/crawl)に残す。
//! 収蔵は main.rs の spawn_samples が行う(ingest ジョブの器を共用=UIの「取込」行に進捗が出る)。

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use crate::store::Progress;

pub struct SampleSet {
    pub id: &'static str,
    pub label: &'static str,
    pub license: &'static str, // サイドカー rights に入る値
    pub license_label: &'static str,
    pub note: &'static str,
    pub origin: &'static str, // real / synthetic
}

pub fn sets() -> Vec<SampleSet> {
    vec![
        SampleSet { id: "artic", label: "🖼 シカゴ美術館(絵画・版画)", license: "cc0", license_label: "CC0",
            note: "Art Institute of Chicago Open Access。パブリックドメイン作品の画像は CC0(商用可・帰属不要)", origin: "real" },
        SampleSet { id: "cma", label: "🏛 クリーブランド美術館(絵画・写真)", license: "cc0", license_label: "CC0",
            note: "Cleveland Museum of Art Open Access。cc0=1 の作品のみ取得", origin: "real" },
        SampleSet { id: "met", label: "🗽 メトロポリタン美術館", license: "cc0", license_label: "CC0",
            note: "The Met Open Access。isPublicDomain=true の作品のみ取得", origin: "real" },
        SampleSet { id: "nasa", label: "🚀 NASA 画像ライブラリ(地球・宇宙)", license: "public-domain", license_label: "パブリックドメイン(米政府)",
            note: "NASA Image and Video Library。米政府著作物は原則パブリックドメイン(一部に第三者著作物あり・出典を保持)", origin: "real" },
        SampleSet { id: "commons", label: "🌍 Wikimedia Commons(写真・CC0/PD)", license: "cc0", license_label: "CC0 / パブリックドメイン",
            note: "Wikimedia Commons の CC-Zero カテゴリと Public domain 表示の画像だけ取得(ライセンス表示を1枚ずつ確認)", origin: "real" },
        SampleSet { id: "wellcome", label: "🔬 Wellcome Collection(医学史・博物画)", license: "cc0", license_label: "CC0 / パブリックドメイン",
            note: "Wellcome Collection(英)。license が cc0 または pdm の画像だけ取得", origin: "real" },
        SampleSet { id: "smk", label: "🎨 デンマーク国立美術館 SMK", license: "public-domain", license_label: "パブリックドメイン",
            note: "Statens Museum for Kunst Open Data。public_domain=true の作品のみ", origin: "real" },
        SampleSet { id: "coco", label: "📷 COCO 2017(実写・物体検出の定番)", license: "cc-by-2.0", license_label: "画像ごとのCC(ストア版はCC BY系のみ)",
            note: "COCO 2017(val 5千枚→足りなければ train 11.8万枚)。画像ごとの Flickr ライセンス(CC BY / BY-SA / BY-NC 等)をサイドカーに記録。ストア版は CC BY 2.0 / BY-SA 2.0 / 制限なし / 米政府作品だけ(全体の約4分の1)。初回に注釈zip 253MB を取得", origin: "real" },
        SampleSet { id: "oi_faces", label: "🧑 Open Images 顔写真(Human face)", license: "cc-by-2.0", license_label: "CC BY 2.0(帰属必要)",
            note: "Open Images validation の Human face ラベル付き写真(全て Flickr CC BY 2.0)。初回に CSV 26MB を取得。実在の人物なので肖像権には別途注意", origin: "real" },
        SampleSet { id: "oi_photos", label: "📷 Open Images 実写いろいろ", license: "cc-by-2.0", license_label: "CC BY 2.0(帰属必要)",
            note: "Open Images validation(41,620枚・全て Flickr CC BY 2.0)からランダムに取得。初回に CSV 15MB を取得", origin: "real" },
        SampleSet { id: "oi_test", label: "📷 Open Images test(125,436枚・約36GB)", license: "cc-by-2.0", license_label: "CC BY 2.0(帰属必要)",
            note: "Open Images test 全量(全て Flickr CC BY 2.0・bbox注釈あり)。train(170万枚・500GB超)は対象外。「全部」で数時間", origin: "real" },
    ]
}

pub struct Item {
    pub url: String,
    pub title: String,
    pub credit: String,
    pub landing: String,
    pub rights: Option<String>, // 画像単位でライセンスが違う源(COCO)用。None=セット既定
}

const UA: &str = "fluent_gallery/0.2 (open-access sample fetch)";

// ---------- 既読台帳(源ごと) ----------
// 「100枚ずつしか取れない」の正体は、毎回同じ先頭を取り直していたこと。
// 取った物のURLを覚えて次は続きから取る。これで何度も押せば端まで集まる。
pub type Seen = std::collections::HashSet<String>;
fn seen_path(root: &Path, id: &str) -> PathBuf { root.join("store/samples").join(format!("{id}.seen.json")) }
pub fn load_seen(root: &Path, id: &str) -> Seen {
    std::fs::read_to_string(seen_path(root, id))
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}
pub fn save_seen(root: &Path, id: &str, set: &Seen) {
    let p = seen_path(root, id);
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let v: Vec<&String> = set.iter().collect();
    let _ = std::fs::write(p, serde_json::to_string(&v).unwrap_or_default());
}

async fn get_json(client: &reqwest::Client, url: &str) -> Result<Value, String> {
    client.get(url).header("User-Agent", UA).timeout(std::time::Duration::from_secs(30))
        .send().await.map_err(|e| e.to_string())?
        .error_for_status().map_err(|e| e.to_string())?
        .json::<Value>().await.map_err(|e| e.to_string())
}

fn s(v: &Value) -> String { v.as_str().unwrap_or("").to_string() }

/// n 枚ぶんの候補(重複除去済み・多めに返すことがある)。root/p/label は大きな注釈ファイルの初回取得(進捗表示)に使う
pub async fn fetch_list(client: &reqwest::Client, root: &Path, p: &Progress, label: &std::sync::Mutex<String>,
                       id: &str, n: usize, already: &Seen) -> Result<Vec<Item>, String> {
    match id {
        "coco" => return coco(client, root, p, label, n, already).await,
        "oi_faces" => return openimages(client, root, p, label, "validation", true, n, already).await,
        "oi_photos" => return openimages(client, root, p, label, "validation", false, n, already).await,
        "oi_test" => return openimages(client, root, p, label, "test", false, n, already).await,
        _ => {}
    }
    let n = if n == 0 { 20000 } else { n }; // API検索の源に「全部」は無い(2万を上限に掘る)
    let mut out: Vec<Item> = Vec::new();
    // 既に取った物を種にしておけば push が弾く。skip はAPIのページ送り(同じ先頭を読み直さない)
    let mut seen: std::collections::HashSet<String> = already.clone();
    let mut skip = already.len();
    fn push(out: &mut Vec<Item>, seen: &mut std::collections::HashSet<String>, it: Item) {
        if seen.insert(it.url.clone()) { out.push(it); }
    }
    // ページを送りながら、まだ持っていない物が n 件たまるまで掘る。
    // (1ページ目だけ見て終わると「押しても同じ100枚」になる — まとめ取りの本体はここ)
    for round in 0..40 {
        let before = out.len();
        #[allow(unused_assignments)] // 各源が自分の1周ぶんを申告する(初期値は使わない)
        let mut page_span = 0usize;
        // 深いページで404等が出るのは「もう無い」の合図。1周目だけは本物のエラーとして返す
        macro_rules! getj { ($u:expr) => { match get_json(client, &$u).await {
            Ok(v) => v,
            Err(e) => { if round == 0 { return Err(e) } else { break } }
        } } }
        match id {
            "artic" => {
                let qs = ["landscape", "portrait", "still life", "city", "animal", "flowers"];
                let per = (n / qs.len()).max(10).min(100);
                page_span = per * qs.len();
                let page = skip / (per * qs.len()) + 1;
                for q in qs {
                    let url = format!("https://api.artic.edu/api/v1/artworks/search?q={}&query%5Bterm%5D%5Bis_public_domain%5D=true&fields=id,title,image_id,artist_display,is_public_domain&limit={per}&page={page}",
                                      urlenc(q));
                    let v = getj!(url);
                    for a in v["data"].as_array().cloned().unwrap_or_default() {
                        if a["is_public_domain"] != true { continue; }
                        let img = s(&a["image_id"]);
                        if img.is_empty() { continue; }
                        push(&mut out, &mut seen, Item {
                            url: format!("https://www.artic.edu/iiif/2/{img}/full/843,/0/default.jpg"),
                            title: s(&a["title"]),
                            credit: format!("{} / Art Institute of Chicago (CC0)", s(&a["artist_display"]).lines().next().unwrap_or("")),
                            landing: format!("https://www.artic.edu/artworks/{}", a["id"].as_i64().unwrap_or(0)),
                            rights: None,
                        });
                    }
                    if out.len() >= n { break; }
                }
            }
            "cma" => {
                let types = ["Painting", "Photograph", "Print", "Drawing"];
                let per = (n / types.len()).max(10).min(100);
                page_span = per * types.len();
                for t in types {
                    let url = format!("https://openaccess-api.clevelandart.org/api/artworks/?cc0=1&has_image=1&type={t}&limit={per}&skip={}", skip / types.len());
                    let v = getj!(url);
                    for a in v["data"].as_array().cloned().unwrap_or_default() {
                        let web = s(&a["images"]["web"]["url"]);
                        if web.is_empty() || a["share_license_status"] != "CC0" { continue; }
                        let who = a["creators"].as_array().and_then(|c| c.first()).map(|c| s(&c["description"])).unwrap_or_default();
                        push(&mut out, &mut seen, Item {
                            url: web,
                            title: s(&a["title"]),
                            credit: format!("{who} / Cleveland Museum of Art (CC0)"),
                            landing: s(&a["url"]),
                            rights: None,
                        });
                    }
                    if out.len() >= n { break; }
                }
            }
            "met" => {
                let qs = ["landscape", "portrait", "flowers", "animals", "city"];
                let per = (n / qs.len()).max(10);
                page_span = per * qs.len();
                for q in qs {
                    let url = format!("https://collectionapi.metmuseum.org/public/collection/v1/search?isPublicDomain=true&hasImages=true&q={}", urlenc(q));
                    let v = getj!(url);
                    let ids: Vec<i64> = v["objectIDs"].as_array()
                        .map(|a| a.iter().filter_map(|x| x.as_i64()).skip(skip / qs.len()).take(per).collect()).unwrap_or_default();
                    for oid in ids {
                        // 1件ずつ(公式APIの作法。80req/s上限なので直列で十分速い)
                        let Ok(o) = get_json(client, &format!("https://collectionapi.metmuseum.org/public/collection/v1/objects/{oid}")).await else { continue };
                        if o["isPublicDomain"] != true { continue; }
                        let img = { let a = s(&o["primaryImageSmall"]); if a.is_empty() { s(&o["primaryImage"]) } else { a } };
                        if img.is_empty() { continue; }
                        push(&mut out, &mut seen, Item {
                            url: img,
                            title: s(&o["title"]),
                            credit: format!("{} / The Metropolitan Museum of Art (CC0)", s(&o["artistDisplayName"])),
                            landing: s(&o["objectURL"]),
                            rights: None,
                        });
                        if out.len() >= n { break; }
                    }
                    if out.len() >= n { break; }
                }
            }
            "nasa" => {
                let qs = ["earth from space", "galaxy", "nebula", "astronaut", "mars surface", "rocket launch", "moon"];
                let per = (n / qs.len()).max(10);
                page_span = per * qs.len();
                for q in qs {
                    let url = format!("https://images-api.nasa.gov/search?q={}&media_type=image&page={}", urlenc(q), skip / (per * qs.len()) + 1);
                    let v = getj!(url);
                    for it in v["collection"]["items"].as_array().cloned().unwrap_or_default().into_iter().take(per) {
                        let href = it["links"].as_array().and_then(|l| l.first()).map(|l| s(&l["href"])).unwrap_or_default();
                        if href.is_empty() { continue; }
                        let d = it["data"].as_array().and_then(|d| d.first()).cloned().unwrap_or(Value::Null);
                        let nid = s(&d["nasa_id"]);
                        let who = { let a = s(&d["secondary_creator"]); if a.is_empty() { s(&d["center"]) } else { a } };
                        push(&mut out, &mut seen, Item {
                            url: href.replace("~thumb.", "~medium."),
                            title: s(&d["title"]),
                            credit: format!("{who} / NASA (public domain)"),
                            landing: format!("https://images.nasa.gov/details/{nid}"),
                            rights: None,
                        });
                    }
                    if out.len() >= n { break; }
                }
            }
            "commons" => {
                // CC-Zero カテゴリを検索し、extmetadata の LicenseShortName でも1枚ずつ確認(CC0 / Public domain 以外は捨てる)
                let qs = ["landscape", "city street", "animal", "food", "portrait photograph", "architecture", "flowers", "beach"];
                let per = (n / qs.len()).max(10).min(50);
                page_span = per * qs.len();
                for q in qs {
                    let url = format!("https://commons.wikimedia.org/w/api.php?action=query&generator=search&gsrsearch={}%20filetype%3Abitmap%20incategory%3A%22CC-Zero%22&gsrnamespace=6&gsrlimit={per}&gsroffset={}&prop=imageinfo&iiprop=url%7Cextmetadata&iiurlwidth=1280&format=json",
                                      urlenc(q), skip / qs.len());
                    let v = getj!(url);
                    for pg in v["query"]["pages"].as_object().map(|o| o.values().cloned().collect::<Vec<_>>()).unwrap_or_default() {
                        let Some(ii) = pg["imageinfo"].as_array().and_then(|a| a.first()) else { continue };
                        let lic = s(&ii["extmetadata"]["LicenseShortName"]["value"]);
                        if !(lic == "CC0" || lic == "Public domain") { continue; }
                        let thumb = { let t = s(&ii["thumburl"]); if t.is_empty() { s(&ii["url"]) } else { t } };
                        if thumb.is_empty() { continue; }
                        let artist = strip_tags(&s(&ii["extmetadata"]["Artist"]["value"]));
                        push(&mut out, &mut seen, Item {
                            url: thumb,
                            title: s(&pg["title"]).trim_start_matches("File:").to_string(),
                            credit: format!("{artist} / Wikimedia Commons ({lic})"),
                            landing: s(&ii["descriptionurl"]),
                            rights: None,
                        });
                    }
                    if out.len() >= n { break; }
                }
            }
            "wellcome" => {
                // API の license パラメータは効かないことがあるので、結果側の thumbnail.license.id で cc0 / pdm だけ通す
                let qs = ["landscape", "anatomy", "botanical", "animal", "portrait", "map"];
                let per = (n / qs.len()).max(10).min(100);
                page_span = per * qs.len();
                for q in qs {
                    let url = format!("https://api.wellcomecollection.org/catalogue/v2/images?query={}&license=cc0,pdm&pageSize={per}&page={}",
                                      urlenc(q), skip / (per * qs.len()) + 1);
                    let v = getj!(url);
                    for r in v["results"].as_array().cloned().unwrap_or_default() {
                        let lic = s(&r["thumbnail"]["license"]["id"]);
                        if !(lic == "cc0" || lic == "pdm") { continue; }
                        let info = s(&r["thumbnail"]["url"]);
                        if !info.ends_with("/info.json") { continue; }
                        push(&mut out, &mut seen, Item {
                            url: info.replace("/info.json", "/full/!1024,1024/0/default.jpg"), // "1024," だと本文0バイトが返ることがある
                            title: s(&r["source"]["title"]),
                            credit: format!("Wellcome Collection ({})", if lic == "cc0" { "CC0" } else { "Public Domain Mark" }),
                            landing: format!("https://wellcomecollection.org/works/{}/images?id={}", s(&r["source"]["id"]), s(&r["id"])),
                            rights: None,
                        });
                    }
                    if out.len() >= n { break; }
                }
            }
            "smk" => {
                let qs = ["landskab", "portræt", "blomster", "dyr", "by", "hav"]; // デンマーク語で検索(landscape/portrait/flowers/animals/city/sea)
                let per = (n / qs.len()).max(10).min(100);
                page_span = per * qs.len();
                for q in qs {
                    let url = format!("https://api.smk.dk/api/v1/art/search/?keys={}&filters=%5Bpublic_domain%3Atrue%5D%2C%5Bhas_image%3Atrue%5D&rows={per}&offset={}", urlenc(q), skip / qs.len());
                    let v = getj!(url);
                    for r in v["items"].as_array().cloned().unwrap_or_default() {
                        if r["public_domain"] != true { continue; }
                        // image_native は3〜8MBで配信が遅い(60秒超)。IIIF があれば長辺1024で、無ければサムネで取る
                        let iiif = s(&r["image_iiif_id"]);
                        let img = if !iiif.is_empty() { format!("{iiif}/full/!1024,1024/0/default.jpg") } else { s(&r["image_thumbnail"]) };
                        if img.is_empty() { continue; }
                        let title = r["titles"].as_array().and_then(|t| t.first()).map(|t| s(&t["title"])).unwrap_or_default();
                        let artist = r["artist"].as_array().and_then(|a| a.first()).map(s).unwrap_or_default();
                        push(&mut out, &mut seen, Item {
                            url: img,
                            title,
                            credit: format!("{artist} / SMK Statens Museum for Kunst (public domain)"),
                            landing: s(&r["frontend_url"]),
                            rights: None,
                        });
                    }
                    if out.len() >= n { break; }
                }
            }
            _ => return Err(format!("unknown sample set: {id}")),
        }
        if out.len() >= n || out.len() == before { break; } // 満ちた / もう出てこない
        skip += page_span.max(1);
    }
    out.truncate(n);
    Ok(out)
}

fn urlenc(q: &str) -> String {
    q.bytes().map(|b| match b {
        b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
        b' ' => "%20".into(),
        _ => format!("%{b:02X}"),
    }).collect()
}

/// extmetadata の Artist 等はHTML断片なのでタグを落とす
fn strip_tags(h: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in h.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
}

// ---------- 大きな注釈ファイルつきの源(COCO / Open Images) ----------

/// engine/data/ 配下に初回だけダウンロード(進捗は Progress の total/done を MB で流用)
async fn ensure_file(client: &reqwest::Client, root: &Path, rel: &str, url: &str, p: &Progress, label: &std::sync::Mutex<String>, what: &str) -> Result<PathBuf, String> {
    use std::io::Write;
    let path = root.join("engine/data").join(rel);
    if path.exists() {
        return Ok(path);
    }
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    *label.lock().unwrap() = format!("{what} を初回取得中");
    let tmp = path.with_extension("part");
    let mut resp = client.get(url).header("User-Agent", UA).send().await.map_err(|e| format!("{what} 取得失敗: {e}"))?
        .error_for_status().map_err(|e| format!("{what} 取得失敗: {e}"))?;
    let total = resp.content_length().unwrap_or(0);
    p.total.store((total >> 20) as usize, Relaxed);
    p.done.store(0, Relaxed);
    let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    let mut got: u64 = 0;
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("DL中断: {e}"))? {
        f.write_all(&chunk).map_err(|e| e.to_string())?;
        got += chunk.len() as u64;
        p.done.store((got >> 20) as usize, Relaxed);
    }
    drop(f);
    if total > 0 && got != total {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{what} DLサイズ不一致({got}/{total})"));
    }
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    p.total.store(0, Relaxed);
    p.done.store(0, Relaxed);
    Ok(path)
}

/// 依存なしの乱択(xorshift・時刻シード)。毎回違う顔ぶれになれば十分
fn shuffle<T>(v: &mut [T]) {
    let mut x: u64 = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0x9E3779B97F4A7C15) | 1;
    for i in (1..v.len()).rev() {
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        v.swap(i, (x % (i as u64 + 1)) as usize);
    }
}

/// 引用符つきCSVの1行を分解(Title に , や " が入る Open Images 用)
fn csv_fields(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut inq = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if inq && chars.peek() == Some(&'"') => { cur.push('"'); chars.next(); }
            '"' => inq = !inq,
            ',' if !inq => { out.push(std::mem::take(&mut cur)); }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// COCO 2017: 注釈zipから画像一覧とライセンスを読み、CC BY 系だけ通す(val→足りなければ train)
async fn coco(client: &reqwest::Client, root: &Path, p: &Progress, label: &std::sync::Mutex<String>, n: usize, already: &Seen) -> Result<Vec<Item>, String> {
    let z = ensure_file(client, root, "coco/annotations_trainval2017.zip",
                        "http://images.cocodataset.org/annotations/annotations_trainval2017.zip", p, label, "COCO 注釈(253MB)").await?;
    // zip の中の1ファイルを読む。val2017(5千枚)→足りなければ train2017(11.8万枚)の順に開く
    let read_part = |name: &'static str| {
        let z = z.clone();
        async move {
            tokio::task::spawn_blocking(move || -> Result<Value, String> {
                use std::io::Read;
                let f = std::fs::File::open(&z).map_err(|e| e.to_string())?;
                let mut ar = zip::ZipArchive::new(f).map_err(|e| e.to_string())?;
                let mut e = ar.by_name(name).map_err(|e| e.to_string())?;
                let mut txt = String::new();
                e.read_to_string(&mut txt).map_err(|e| e.to_string())?;
                serde_json::from_str(&txt).map_err(|e| e.to_string())
            }).await.map_err(|e| e.to_string())?
        }
    };
    // licenses: 1-3=CC NC系(除外) 4=CC BY 2.0 5=CC BY-SA 2.0 6=CC BY-ND 2.0(改変禁止・除外) 7=No known copyright restrictions 8=US Government Work
    let rights_of = |id: i64| -> Option<&'static str> {
        match id {
            4 => Some("cc-by-2.0"), 5 => Some("cc-by-sa-2.0"), 7 => Some("no-known-restrictions"), 8 => Some("us-government"),
            1 if !cfg!(feature = "store") => Some("cc-by-nc-sa-2.0"),
            2 if !cfg!(feature = "store") => Some("cc-by-nc-2.0"),
            3 if !cfg!(feature = "store") => Some("cc-by-nc-nd-2.0"),
            6 if !cfg!(feature = "store") => Some("cc-by-nd-2.0"),
            _ => None,
        }
    };
    let harvest = |json: &Value, part: &'static str| -> Vec<Item> {
        let lic: std::collections::HashMap<i64, String> = json["licenses"].as_array().map(|a| a.iter()
            .filter_map(|l| Some((l["id"].as_i64()?, s(&l["name"])))).collect()).unwrap_or_default();
        json["images"].as_array().cloned().unwrap_or_default().into_iter().filter_map(|im| {
            let lid = im["license"].as_i64()?;
            let rights = rights_of(lid)?;
            let url = s(&im["coco_url"]);
            if url.is_empty() || already.contains(&url) { return None; } // 既に取った物は候補から外す
            let flickr = s(&im["flickr_url"]);
            Some(Item {
                url,
                title: s(&im["file_name"]),
                credit: format!("Flickr {} (COCO {}, {})", flickr, part, lic.get(&lid).cloned().unwrap_or_default()),
                landing: if flickr.is_empty() { "https://cocodataset.org/".into() } else { flickr },
                rights: Some(rights.to_string()),
            })
        }).collect()
    };
    let n = if n == 0 { usize::MAX } else { n }; // 0=全部
    let mut rows = harvest(&read_part("annotations/captions_val2017.json").await?, "val2017");
    // val2017 で権利が通るのは5000枚中1279枚だけ。まとめ取りではすぐ底を突くので train2017 も開く
    // (同じzipに入っている。104MBのJSONなので、必要になった時だけ読む)
    if rows.len() < n {
        *label.lock().unwrap() = "COCO train2017(11.8万枚)の一覧を読み込み中".into();
        if let Ok(t) = read_part("annotations/captions_train2017.json").await {
            rows.extend(harvest(&t, "train2017"));
        }
    }
    shuffle(&mut rows);
    rows.truncate(n);
    Ok(rows)
}

/// Open Images validation(全て Flickr CC BY 2.0)。faces=true なら Human face(/m/0dzct) ラベルが付いた画像だけ
async fn openimages(client: &reqwest::Client, root: &Path, p: &Progress, label: &std::sync::Mutex<String>, split: &str, faces: bool, n: usize, already: &Seen) -> Result<Vec<Item>, String> {
    let rot = ensure_file(client, root, &format!("openimages/{split}-images-with-rotation.csv"),
                          &format!("https://storage.googleapis.com/openimages/2018_04/{split}/{split}-images-with-rotation.csv"), p, label,
                          &format!("Open Images {split} 画像一覧")).await?;
    let allowed: Option<std::collections::HashSet<String>> = if faces {
        let lab = ensure_file(client, root, "openimages/validation-annotations-human-imagelabels-boxable.csv",
                              "https://storage.googleapis.com/openimages/v5/validation-annotations-human-imagelabels-boxable.csv", p, label, "Open Images ラベル(11MB)").await?;
        let txt = std::fs::read_to_string(&lab).map_err(|e| e.to_string())?;
        Some(txt.lines().filter_map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            (f.len() >= 4 && f[2] == "/m/0dzct" && f[3] == "1").then(|| f[0].to_string())
        }).collect())
    } else { None };
    let txt = std::fs::read_to_string(&rot).map_err(|e| e.to_string())?;
    let mut rows: Vec<Item> = txt.lines().skip(1).filter_map(|line| {
        let f = csv_fields(line);
        if f.len() < 8 || !f[4].contains("licenses/by/2.0") { return None; }
        if let Some(a) = &allowed { if !a.contains(&f[0]) { return None; } }
        Some(Item {
            url: format!("https://open-images-dataset.s3.amazonaws.com/{split}/{}.jpg", f[0]),
            title: f[7].clone(),
            credit: format!("{} / Flickr (CC BY 2.0) via Open Images", f[6]),
            landing: f[3].clone(),
            rights: Some("cc-by-2.0".into()),
        })
    }).collect();
    rows.retain(|it| !already.contains(&it.url)); // 既に取った分は候補から外す(次は続きが来る)
    shuffle(&mut rows);
    if n > 0 { rows.truncate(n); } // 0=全部
    Ok(rows)
}

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
        SampleSet { id: "coco", label: "📷 COCO val2017(実写・物体検出の定番)", license: "cc-by-2.0", license_label: "CC BY系のみ(帰属必要)",
            note: "COCO 2017 val 5,000枚のうち、画像ごとの Flickr ライセンスが CC BY 2.0 / CC BY-SA 2.0 / 制限なし / 米政府作品のものだけ(非商用・改変禁止は除外)。初回に注釈zip 253MB を取得。帰属(作者URL)はサイドカーに保存", origin: "real" },
        SampleSet { id: "oi_faces", label: "🧑 Open Images 顔写真(Human face)", license: "cc-by-2.0", license_label: "CC BY 2.0(帰属必要)",
            note: "Open Images validation の Human face ラベル付き写真(全て Flickr CC BY 2.0)。初回に CSV 26MB を取得。実在の人物なので肖像権には別途注意", origin: "real" },
        SampleSet { id: "oi_photos", label: "📷 Open Images 実写いろいろ", license: "cc-by-2.0", license_label: "CC BY 2.0(帰属必要)",
            note: "Open Images validation(41,620枚・全て Flickr CC BY 2.0)からランダムに取得。初回に CSV 15MB を取得", origin: "real" },
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

async fn get_json(client: &reqwest::Client, url: &str) -> Result<Value, String> {
    client.get(url).header("User-Agent", UA).timeout(std::time::Duration::from_secs(30))
        .send().await.map_err(|e| e.to_string())?
        .error_for_status().map_err(|e| e.to_string())?
        .json::<Value>().await.map_err(|e| e.to_string())
}

fn s(v: &Value) -> String { v.as_str().unwrap_or("").to_string() }

/// n 枚ぶんの候補(重複除去済み・多めに返すことがある)。root/p/label は大きな注釈ファイルの初回取得(進捗表示)に使う
pub async fn fetch_list(client: &reqwest::Client, root: &Path, p: &Progress, label: &std::sync::Mutex<String>, id: &str, n: usize) -> Result<Vec<Item>, String> {
    match id {
        "coco" => return coco(client, root, p, label, n).await,
        "oi_faces" => return openimages(client, root, p, label, true, n).await,
        "oi_photos" => return openimages(client, root, p, label, false, n).await,
        _ => {}
    }
    let mut out: Vec<Item> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    fn push(out: &mut Vec<Item>, seen: &mut std::collections::HashSet<String>, it: Item) {
        if seen.insert(it.url.clone()) { out.push(it); }
    }
    match id {
        "artic" => {
            let qs = ["landscape", "portrait", "still life", "city", "animal", "flowers"];
            let per = (n / qs.len()).max(10).min(100);
            for q in qs {
                let url = format!("https://api.artic.edu/api/v1/artworks/search?q={}&query%5Bterm%5D%5Bis_public_domain%5D=true&fields=id,title,image_id,artist_display,is_public_domain&limit={per}",
                                  urlenc(q));
                let v = get_json(client, &url).await?;
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
            for t in types {
                let url = format!("https://openaccess-api.clevelandart.org/api/artworks/?cc0=1&has_image=1&type={t}&limit={per}&skip=0");
                let v = get_json(client, &url).await?;
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
            for q in qs {
                let url = format!("https://collectionapi.metmuseum.org/public/collection/v1/search?isPublicDomain=true&hasImages=true&q={}", urlenc(q));
                let v = get_json(client, &url).await?;
                let ids: Vec<i64> = v["objectIDs"].as_array().map(|a| a.iter().filter_map(|x| x.as_i64()).take(per).collect()).unwrap_or_default();
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
            for q in qs {
                let url = format!("https://images-api.nasa.gov/search?q={}&media_type=image&page=1", urlenc(q));
                let v = get_json(client, &url).await?;
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
            for q in qs {
                let url = format!("https://commons.wikimedia.org/w/api.php?action=query&generator=search&gsrsearch={}%20filetype%3Abitmap%20incategory%3A%22CC-Zero%22&gsrnamespace=6&gsrlimit={per}&prop=imageinfo&iiprop=url%7Cextmetadata&iiurlwidth=1280&format=json",
                                  urlenc(q));
                let v = get_json(client, &url).await?;
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
            for q in qs {
                let url = format!("https://api.wellcomecollection.org/catalogue/v2/images?query={}&license=cc0,pdm&pageSize={per}", urlenc(q));
                let v = get_json(client, &url).await?;
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
            for q in qs {
                let url = format!("https://api.smk.dk/api/v1/art/search/?keys={}&filters=%5Bpublic_domain%3Atrue%5D%2C%5Bhas_image%3Atrue%5D&rows={per}&offset=0", urlenc(q));
                let v = get_json(client, &url).await?;
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

/// COCO 2017 val: 注釈zipの captions_val2017.json(小さい)から画像一覧とライセンスを読み、CC BY 系だけ通す
async fn coco(client: &reqwest::Client, root: &Path, p: &Progress, label: &std::sync::Mutex<String>, n: usize) -> Result<Vec<Item>, String> {
    let z = ensure_file(client, root, "coco/annotations_trainval2017.zip",
                        "http://images.cocodataset.org/annotations/annotations_trainval2017.zip", p, label, "COCO 注釈(253MB)").await?;
    let json: Value = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        use std::io::Read;
        let f = std::fs::File::open(&z).map_err(|e| e.to_string())?;
        let mut ar = zip::ZipArchive::new(f).map_err(|e| e.to_string())?;
        let mut e = ar.by_name("annotations/captions_val2017.json").map_err(|e| e.to_string())?;
        let mut txt = String::new();
        e.read_to_string(&mut txt).map_err(|e| e.to_string())?;
        serde_json::from_str(&txt).map_err(|e| e.to_string())
    }).await.map_err(|e| e.to_string())??;
    // licenses: 1-3=CC NC系(除外) 4=CC BY 2.0 5=CC BY-SA 2.0 6=CC BY-ND 2.0(改変禁止・除外) 7=No known copyright restrictions 8=US Government Work
    let lic: std::collections::HashMap<i64, String> = json["licenses"].as_array().map(|a| a.iter()
        .filter_map(|l| Some((l["id"].as_i64()?, s(&l["name"])))).collect()).unwrap_or_default();
    let rights_of = |id: i64| -> Option<&'static str> {
        match id { 4 => Some("cc-by-2.0"), 5 => Some("cc-by-sa-2.0"), 7 => Some("no-known-restrictions"), 8 => Some("us-government"), _ => None }
    };
    let mut rows: Vec<Item> = json["images"].as_array().cloned().unwrap_or_default().into_iter().filter_map(|im| {
        let lid = im["license"].as_i64()?;
        let rights = rights_of(lid)?;
        let url = s(&im["coco_url"]);
        if url.is_empty() { return None; }
        let flickr = s(&im["flickr_url"]);
        Some(Item {
            url,
            title: s(&im["file_name"]),
            credit: format!("Flickr {} (COCO val2017, {})", flickr, lic.get(&lid).cloned().unwrap_or_default()),
            landing: if flickr.is_empty() { "https://cocodataset.org/".into() } else { flickr },
            rights: Some(rights.to_string()),
        })
    }).collect();
    shuffle(&mut rows);
    rows.truncate(n);
    Ok(rows)
}

/// Open Images validation(全て Flickr CC BY 2.0)。faces=true なら Human face(/m/0dzct) ラベルが付いた画像だけ
async fn openimages(client: &reqwest::Client, root: &Path, p: &Progress, label: &std::sync::Mutex<String>, faces: bool, n: usize) -> Result<Vec<Item>, String> {
    let rot = ensure_file(client, root, "openimages/validation-images-with-rotation.csv",
                          "https://storage.googleapis.com/openimages/2018_04/validation/validation-images-with-rotation.csv", p, label, "Open Images 画像一覧(15MB)").await?;
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
            url: format!("https://open-images-dataset.s3.amazonaws.com/validation/{}.jpg", f[0]),
            title: f[7].clone(),
            credit: format!("{} / Flickr (CC BY 2.0) via Open Images", f[6]),
            landing: f[3].clone(),
            rights: Some("cc-by-2.0".into()),
        })
    }).collect();
    shuffle(&mut rows);
    rows.truncate(n);
    Ok(rows)
}

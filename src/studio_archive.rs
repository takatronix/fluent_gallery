//! Private baked edits travel with their source sidecars, never as gallery images.
use image::{ImageFormat, ImageReader};
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const PRIVATE: &str = ".fluent_gallery";
const MAX_FILES: usize = 16_384;
const MAX_TOTAL: u64 = 2 << 30;
const MAX_IMAGE: u64 = 64 << 20;
const MAX_JSON: u64 = 16 << 20;
const MAX_EDGE: u32 = 8192;
const MAX_PIXELS: u64 = 32_000_000;

#[derive(Clone, Debug)]
enum Kind {
    Render(String),
    Snapshot(String),
    Asset(String, ImageFormat),
}

impl Kind {
    fn limit(&self) -> u64 {
        if matches!(self, Self::Snapshot(_)) {
            MAX_JSON
        } else {
            MAX_IMAGE
        }
    }
}

fn valid_sha(sha: &str) -> bool {
    sha.len() == 40
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Also recognizes malformed reserved paths so upload never ingests their PNGs as photos.
pub fn is_private_entry(name: &str) -> bool {
    name.split(['/', '\\']).any(|part| part == PRIVATE)
}

fn parse_entry(name: &str) -> Result<Option<(String, Kind)>, String> {
    if !is_private_entry(name) {
        return Ok(None);
    }
    if name.contains('\\') || name.starts_with('/') {
        return Err("フィルター添付のパスが不正です".into());
    }
    let parts: Vec<_> = name.trim_end_matches('/').split('/').collect();
    if parts
        .iter()
        .any(|part| part.is_empty() || *part == "." || *part == "..")
    {
        return Err("フィルター添付のパスが不正です".into());
    }
    let offset = parts.iter().position(|part| *part == PRIVATE).unwrap();
    let parts = &parts[offset..];
    if name.ends_with('/') {
        let valid_dir = matches!(
            parts,
            [PRIVATE] | [PRIVATE, "studio_renders"] | [PRIVATE, "studio_assets"]
        ) || matches!(parts, [PRIVATE, "studio_renders", shard] if shard.len() == 2 && shard.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        return if valid_dir {
            Ok(None)
        } else {
            Err("フィルター添付のフォルダーが不正です".into())
        };
    }
    let kind = match parts {
        [PRIVATE, "studio_renders", shard, file] => {
            let (sha, ext) = file
                .split_once('.')
                .ok_or("フィルター添付の名前が不正です")?;
            if !valid_sha(sha) || *shard != &sha[..2] {
                return Err("フィルター添付のIDが不正です".into());
            }
            match ext {
                "png" => Kind::Render(sha.to_owned()),
                "json" => Kind::Snapshot(sha.to_owned()),
                _ => return Err("未対応のフィルター添付です".into()),
            }
        }
        [PRIVATE, "studio_assets", file] => {
            let (sha, ext) = file.split_once('.').ok_or("追加画像の名前が不正です")?;
            if !valid_sha(sha) {
                return Err("追加画像のIDが不正です".into());
            }
            let format = match ext {
                "png" => ImageFormat::Png,
                "jpg" | "jpeg" => ImageFormat::Jpeg,
                "webp" => ImageFormat::WebP,
                _ => return Err("未対応の追加画像です".into()),
            };
            Kind::Asset(sha.to_owned(), format)
        }
        _ => return Err("未対応のフィルター添付です".into()),
    };
    Ok(Some((parts.join("/"), kind)))
}

fn disk_path(root: &Path, relative: &str) -> PathBuf {
    root.join("store")
        .join(relative.strip_prefix(".fluent_gallery/").unwrap())
}

fn render_name(sha: &str, ext: &str) -> String {
    format!("{PRIVATE}/studio_renders/{}/{sha}.{ext}", &sha[..2])
}

fn read_bounded(reader: impl Read, limit: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > limit {
        return Err("フィルター添付のサイズが上限を超えています".into());
    }
    Ok(bytes)
}

fn collect_edits(edits: &Value, refs: &mut BTreeSet<String>) -> Result<(), String> {
    if edits.is_null() {
        return Ok(());
    }
    let edits = edits
        .as_array()
        .filter(|edits| edits.len() <= MAX_FILES)
        .ok_or("編集履歴が不正または長すぎます")?;
    for edit in edits {
        if edit["op"] != "studio" {
            continue;
        }
        let sha = edit["params"]["render_sha"]
            .as_str()
            .filter(|sha| valid_sha(sha))
            .ok_or("フィルター画像の参照が不正です")?;
        refs.insert(render_name(sha, "png"));
        refs.insert(render_name(sha, "json"));
    }
    if refs.len() > MAX_FILES {
        return Err("フィルター添付が多すぎます".into());
    }
    Ok(())
}

fn snapshot_refs(snapshot: &Value, refs: &mut BTreeSet<String>) -> Result<(), String> {
    collect_edits(&snapshot["source_edits"], refs)?;
    collect_edits(&snapshot["recipe"]["photo_edits"], refs)?;
    if let Some(assets) = snapshot["recipe"]["assets"].as_array() {
        if assets.len() > 64 {
            return Err("追加画像が多すぎます".into());
        }
        for asset in assets {
            if let Some(name) = asset["data"]
                .as_str()
                .and_then(|data| data.strip_prefix("/studio-assets/"))
            {
                let relative = format!("{PRIVATE}/studio_assets/{name}");
                let Some((canonical, Kind::Asset(..))) = parse_entry(&relative)? else {
                    return Err("追加画像の参照が不正です".into());
                };
                refs.insert(canonical);
            }
        }
    }
    if refs.len() > MAX_FILES {
        return Err("フィルター添付が多すぎます".into());
    }
    Ok(())
}

fn validate(kind: &Kind, bytes: &[u8]) -> Result<Option<Value>, String> {
    if bytes.len() as u64 > kind.limit() {
        return Err("フィルター添付のサイズが上限を超えています".into());
    }
    if let Kind::Snapshot(sha) = kind {
        let snapshot: Value =
            serde_json::from_slice(bytes).map_err(|_| "編集レシピのJSONが不正です")?;
        if snapshot["render_sha"] != sha.as_str()
            || snapshot["version"] != 2
            || !snapshot["recipe"].is_object()
        {
            return Err("編集レシピとフィルター画像のIDが一致しません".into());
        }
        return Ok(Some(snapshot));
    }
    let (sha, expected_format) = match kind {
        Kind::Render(sha) => (sha, ImageFormat::Png),
        Kind::Asset(sha, format) => (sha, *format),
        Kind::Snapshot(_) => unreachable!(),
    };
    if hex::encode(Sha1::digest(bytes)) != *sha {
        return Err("フィルター添付の画像とIDが一致しません".into());
    }
    if image::guess_format(bytes).ok() != Some(expected_format) {
        return Err("フィルター添付の画像形式が一致しません".into());
    }
    let (w, h) = ImageReader::with_format(Cursor::new(bytes), expected_format)
        .into_dimensions()
        .map_err(|_| "フィルター添付の画像を読み取れません")?;
    if w == 0 || h == 0 || w > MAX_EDGE || h > MAX_EDGE || u64::from(w) * u64::from(h) > MAX_PIXELS
    {
        return Err("フィルター添付の画像サイズが上限を超えています".into());
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), expected_format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_EDGE);
    limits.max_image_height = Some(MAX_EDGE);
    limits.max_alloc = Some(512 << 20);
    reader.limits(limits);
    reader
        .decode()
        .map_err(|_| "フィルター添付の画像を読み取れません")?;
    Ok(None)
}

/// Returns paths relative to the exported folder; callers may prefix their ZIP folder name.
pub fn export_files(root: &Path, meta: &Value) -> Result<Vec<(String, PathBuf)>, String> {
    let mut refs = BTreeSet::new();
    collect_edits(&meta["edits"], &mut refs)?;
    let mut pending: VecDeque<_> = refs.iter().cloned().collect();
    let mut files = BTreeMap::new();
    let mut total = 0u64;
    while let Some(relative) = pending.pop_front() {
        if files.contains_key(&relative) {
            continue;
        }
        let (_, kind) = parse_entry(&relative)?.ok_or("フィルター添付の参照が不正です")?;
        let path = disk_path(root, &relative);
        let file = File::open(&path)
            .map_err(|_| format!("保存済みフィルター添付が見つかりません: {relative}"))?;
        let bytes = read_bounded(file, kind.limit())?;
        total += bytes.len() as u64;
        if total > MAX_TOTAL {
            return Err("フィルター添付の合計が2GiBを超えています".into());
        }
        if let Some(snapshot) = validate(&kind, &bytes)? {
            let mut discovered = BTreeSet::new();
            snapshot_refs(&snapshot, &mut discovered)?;
            for reference in discovered {
                if refs.insert(reference.clone()) {
                    pending.push_back(reference);
                }
            }
            if refs.len() > MAX_FILES {
                return Err("フィルター添付が多すぎます".into());
            }
        }
        files.insert(relative, path);
    }
    Ok(files.into_iter().collect())
}

fn existing_matches(path: &Path, bytes: &[u8]) -> Result<bool, String> {
    match File::open(path) {
        Ok(file) => {
            let prior = read_bounded(file, bytes.len() as u64)?;
            if prior != bytes {
                return Err("既存のフィルター添付と内容が異なるため上書きできません".into());
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

fn publish(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if existing_matches(path, bytes)? {
        return Ok(());
    }
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().ok_or("フィルター添付の保存先が不正です")?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp = parent.join(format!(
        ".import-{}-{nonce}-{}.tmp",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|error| error.to_string())?;
        file.write_all(bytes).map_err(|error| error.to_string())?;
        drop(file);
        // An exclusive hard link publishes complete bytes without replacing a
        // concurrently created file (rename alone would overwrite it on Unix).
        match fs::hard_link(&tmp, path) {
            Ok(()) => Ok(()),
            Err(_) if path.exists() => existing_matches(path, bytes).map(|_| ()),
            Err(error) => Err(error.to_string()),
        }
    })();
    let _ = fs::remove_file(tmp);
    result
}

/// Restores private attachments before ordinary image import. It never creates metadata or index rows.
pub fn import_files<R: Read + Seek>(
    root: &Path,
    archive: &mut zip::ZipArchive<R>,
) -> Result<(), String> {
    let mut files = BTreeMap::new();
    let mut refs = BTreeSet::new();
    let mut total = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
        let Some((relative, kind)) = parse_entry(entry.name())? else {
            continue;
        };
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
            || entry.is_dir()
        {
            return Err("フィルター添付にリンクは使用できません".into());
        }
        if entry.size() > kind.limit() || files.len() >= MAX_FILES {
            return Err("フィルター添付がサイズまたは件数の上限を超えています".into());
        }
        if files.contains_key(&relative) {
            return Err("フィルター添付の名前が重複しています".into());
        }
        let bytes = read_bounded(&mut entry, kind.limit())?;
        total += bytes.len() as u64;
        if total > MAX_TOTAL {
            return Err("フィルター添付の合計が2GiBを超えています".into());
        }
        if let Some(snapshot) = validate(&kind, &bytes)? {
            if let Kind::Snapshot(sha) = &kind {
                refs.insert(render_name(sha, "png"));
            }
            snapshot_refs(&snapshot, &mut refs)?;
        } else if let Kind::Render(sha) = &kind {
            refs.insert(render_name(sha, "json"));
        }
        existing_matches(&disk_path(root, &relative), &bytes)?;
        files.insert(relative, (index, kind));
    }
    // Every dependency must arrive in this ZIP or already exist in private storage.
    let mut pending: VecDeque<_> = refs.iter().cloned().collect();
    while let Some(relative) = pending.pop_front() {
        if files.contains_key(&relative) {
            continue;
        }
        let (_, kind) = parse_entry(&relative)?.ok_or("フィルター添付の参照が不正です")?;
        let bytes = read_bounded(
            File::open(disk_path(root, &relative))
                .map_err(|_| format!("フィルター添付が不足しています: {relative}"))?,
            kind.limit(),
        )?;
        total += bytes.len() as u64;
        if total > MAX_TOTAL {
            return Err("フィルター添付の合計が2GiBを超えています".into());
        }
        if let Some(snapshot) = validate(&kind, &bytes)? {
            let mut discovered = BTreeSet::new();
            snapshot_refs(&snapshot, &mut discovered)?;
            for reference in discovered {
                if refs.insert(reference.clone()) {
                    pending.push_back(reference);
                }
            }
            if refs.len() > MAX_FILES {
                return Err("フィルター添付が多すぎます".into());
            }
        }
    }
    // Validate the whole attachment set before publishing any files. Reading one
    // bounded entry at a time avoids retaining all decoded resources in memory.
    for (relative, (index, kind)) in files {
        let bytes = read_bounded(
            archive.by_index(index).map_err(|error| error.to_string())?,
            kind.limit(),
        )?;
        publish(&disk_path(root, &relative), &bytes)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn private_paths_are_recognized_and_strictly_validated() {
        let sha = "a".repeat(40);
        assert!(is_private_entry(&format!(
            "album/{PRIVATE}/studio_renders/aa/{sha}.png"
        )));
        assert!(is_private_entry("../.fluent_gallery/studio_assets/x.png"));
        assert!(is_private_entry(
            "album\\.fluent_gallery\\studio_assets\\x.png"
        ));
        assert!(!is_private_entry("album/photo.png"));
        assert!(!is_private_entry("album/meta/abc.json"));
        assert!(
            parse_entry(&format!("album/{PRIVATE}/studio_renders/aa/{sha}.png"))
                .unwrap()
                .is_some()
        );
        for name in [
            format!("../{PRIVATE}/studio_renders/aa/{sha}.png"),
            format!("{PRIVATE}/studio_renders/ab/{sha}.png"),
            format!("{PRIVATE}/studio_renders/aa/{sha}.png/extra"),
            format!("{PRIVATE}/studio_assets/../{sha}.png"),
            format!("{PRIVATE}/studio_assets/{sha}.gif"),
        ] {
            assert!(parse_entry(&name).is_err(), "{name}");
        }
    }

    #[test]
    fn snapshots_require_the_matching_render_and_collect_nested_dependencies() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let asset = format!("{}.png", "c".repeat(40));
        let snapshot = json!({"version":2,"render_sha":a,"source_edits":[{"op":"studio","params":{"render_sha":b}}],
            "recipe":{"photo_edits":[{"op":"studio","params":{"render_sha":b}}],
                "assets":[{"data":format!("/studio-assets/{asset}")}]}});
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        assert!(validate(&Kind::Snapshot(a), &bytes).is_ok());
        assert!(validate(&Kind::Snapshot(b), &bytes).is_err());
        let mut refs = BTreeSet::new();
        snapshot_refs(&snapshot, &mut refs).unwrap();
        assert_eq!(refs.len(), 3);
        assert!(refs.contains(&format!("{PRIVATE}/studio_assets/{asset}")));
    }

    #[test]
    fn old_archives_have_no_private_attachments() {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file("album/plain.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"legacy archive").unwrap();
        let mut archive =
            zip::ZipArchive::new(Cursor::new(zip.finish().unwrap().into_inner())).unwrap();
        assert!(import_files(Path::new("/unused-studio-archive-test"), &mut archive).is_ok());
    }

    #[test]
    fn private_resources_round_trip_without_image_rows_or_overwriting_conflicts() {
        struct Temp(PathBuf);
        impl Drop for Temp {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = Temp(std::env::temp_dir().join(format!(
            "fluent-studio-archive-{}-{nonce}",
            std::process::id()
        )));
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(3, 2)
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        let bytes = png.into_inner();
        let sha = hex::encode(Sha1::digest(&bytes));
        let snapshot = serde_json::to_vec(&json!({"version":2,"render_sha":sha,"source_edits":[],
            "recipe":{"photo_edits":[],"assets":[{"data":format!("/studio-assets/{sha}.png")}]}}))
        .unwrap();
        let entries = [
            (render_name(&sha, "png"), bytes.clone()),
            (render_name(&sha, "json"), snapshot.clone()),
            (format!("{PRIVATE}/studio_assets/{sha}.png"), bytes.clone()),
        ];
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in &entries {
            writer
                .start_file(
                    format!("album/{name}"),
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer.write_all(data).unwrap();
        }
        let zip = writer.finish().unwrap().into_inner();
        let mut archive = zip::ZipArchive::new(Cursor::new(&zip)).unwrap();
        import_files(&root.0, &mut archive).unwrap();
        import_files(&root.0, &mut archive).unwrap();
        let meta = json!({"edits":[{"op":"studio","params":{"render_sha":sha}}]});
        let exported = export_files(&root.0, &meta).unwrap();
        assert_eq!(exported.len(), 3);
        for (name, expected) in &entries {
            assert_eq!(fs::read(disk_path(&root.0, name)).unwrap(), *expected);
        }
        assert!(!root.0.join("store/images").exists());
        assert!(!root.0.join("store/meta").exists());
        assert!(!root.0.join("store/index.sqlite").exists());
        let snapshot_path = disk_path(&root.0, &render_name(&sha, "json"));
        fs::write(&snapshot_path, b"different existing data").unwrap();
        assert!(import_files(&root.0, &mut archive).is_err());
        assert_eq!(fs::read(snapshot_path).unwrap(), b"different existing data");
    }
}

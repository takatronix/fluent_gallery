"""fluent_gallery コア — 収蔵(ingest)・属性・検索・払い出し(materialize)。

正本はサイドカーJSON(store/meta/)。SQLiteは検索インデックスで、いつでも rebuild() で再構築できる。
本体は content-addressed(store/images/ab/<sha1>.<ext>)、同一FSならハードリンクで容量を食わない。
"""
from __future__ import annotations

import hashlib
import json
import os
import sqlite3
import time
from pathlib import Path

import numpy as np
from PIL import Image

ROOT = Path(__file__).resolve().parent
STORE = ROOT / "store"
IMAGES = STORE / "images"
META = STORE / "meta"
DATASETS = STORE / "datasets"
DB = STORE / "index.sqlite"
THUMBS = STORE / "thumbs"
IMG_EXTS = {".jpg", ".jpeg", ".png", ".webp", ".bmp"}

_SCHEMA = """
CREATE TABLE IF NOT EXISTS images(
  sha1 TEXT PRIMARY KEY, ext TEXT, w INT, h INT, bytes INT, phash TEXT,
  source TEXT, origin TEXT, ingested REAL,
  vlm_model TEXT, caption TEXT, quality INT, nsfw INT,
  scene TEXT, subject TEXT, lighting TEXT, style TEXT);
CREATE TABLE IF NOT EXISTS tags(sha1 TEXT, tag TEXT, PRIMARY KEY(sha1, tag));
CREATE INDEX IF NOT EXISTS idx_tags_tag ON tags(tag);
CREATE INDEX IF NOT EXISTS idx_images_source ON images(source);
CREATE VIRTUAL TABLE IF NOT EXISTS captions USING fts5(sha1 UNINDEXED, caption);
"""


def db() -> sqlite3.Connection:
    STORE.mkdir(parents=True, exist_ok=True)
    c = sqlite3.connect(DB, timeout=30)
    c.row_factory = sqlite3.Row
    c.executescript(_SCHEMA)
    return c


def _shard(sha1: str) -> str:
    return sha1[:2]


def meta_path(sha1: str) -> Path:
    return META / _shard(sha1) / f"{sha1}.json"


def image_path(sha1: str, ext: str) -> Path:
    return IMAGES / _shard(sha1) / f"{sha1}.{ext}"


def load_meta(sha1: str) -> dict | None:
    p = meta_path(sha1)
    try:
        return json.loads(p.read_text())
    except (OSError, ValueError):
        return None


def save_meta(m: dict) -> None:
    p = meta_path(m["sha1"])
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_suffix(".tmp")
    tmp.write_text(json.dumps(m, ensure_ascii=False))
    tmp.replace(p)


def phash64(img: Image.Image) -> str:
    """DCTベースのpHash(64bit)。imagehash非依存の自前実装。"""
    g = np.asarray(img.convert("L").resize((32, 32), Image.LANCZOS), np.float32)
    # 2D DCT-II (numpyのみ): D @ g @ D.T
    n = 32
    k = np.arange(n)
    d = np.sqrt(2 / n) * np.cos(np.pi * (2 * k[None, :] + 1) * k[:, None] / (2 * n))
    d[0] /= np.sqrt(2)
    dct = d @ g @ d.T
    low = dct[:8, :8].flatten()
    bits = low > np.median(low[1:])                # DC成分を閾値から除く定石
    return f"{int(''.join('1' if b else '0' for b in bits), 2):016x}"


def index_meta(c: sqlite3.Connection, m: dict) -> None:
    v = m.get("vlm") or {}
    a = v.get("attrs") or {}
    c.execute(
        "INSERT OR REPLACE INTO images VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        (m["sha1"], m["ext"], m["w"], m["h"], m["bytes"], m.get("phash"),
         m.get("source"), m.get("origin"), m.get("ingested"),
         v.get("model"), v.get("caption"),
         a.get("quality"), 1 if a.get("nsfw") else 0 if v else None,
         a.get("scene"), a.get("subject"), a.get("lighting"), a.get("style")))
    c.execute("DELETE FROM tags WHERE sha1=?", (m["sha1"],))
    for t in v.get("tags") or []:
        c.execute("INSERT OR IGNORE INTO tags VALUES(?,?)", (m["sha1"], str(t)[:48].lower()))
    c.execute("DELETE FROM captions WHERE sha1=?", (m["sha1"],))
    if v.get("caption"):
        c.execute("INSERT INTO captions VALUES(?,?)", (m["sha1"], v["caption"]))


def infer_origin(source: str) -> str:
    """生成データか実写か。gen/var系ソースはsynthetic、それ以外はreal。"""
    s = (source or "").lower()
    return "synthetic" if any(k in s for k in ("gen", "var", "synth", "t2i", "sd_", "ai_")) else "real"


def ingest(path: str | Path, source: str, move: bool = False, origin: str = "") -> dict:
    """ディレクトリ/ファイルを収蔵。sha1重複はスキップ。同FSはハードリンク(容量ゼロ)。"""
    p = Path(path).expanduser()
    files = ([p] if p.is_file() else
             [f for f in sorted(p.rglob("*")) if f.suffix.lower() in IMG_EXTS])
    added = dup = bad = 0
    c = db()
    try:
        for f in files:
            try:
                data = f.read_bytes()
                sha1 = hashlib.sha1(data).hexdigest()
                if load_meta(sha1):
                    dup += 1
                    continue
                with Image.open(f) as im:
                    im.load()
                    w, h = im.size
                    ph = phash64(im)
                ext = f.suffix.lower().lstrip(".").replace("jpeg", "jpg")
                dst = image_path(sha1, ext)
                dst.parent.mkdir(parents=True, exist_ok=True)
                if not dst.exists():
                    try:
                        os.link(f, dst)                    # 同FS: ハードリンク
                    except OSError:
                        dst.write_bytes(data)              # 別FS: コピー
                if move:
                    f.unlink(missing_ok=True)
                m = {"sha1": sha1, "ext": ext, "w": w, "h": h, "bytes": len(data),
                     "phash": ph, "source": source,
                     "origin": origin or infer_origin(source), "ingested": time.time()}
                save_meta(m)
                index_meta(c, m)
                added += 1
            except Exception:
                bad += 1
        c.commit()
    finally:
        c.close()
    return {"added": added, "dup": dup, "bad": bad, "scanned": len(files)}


def query(tag: str = "", q: str = "", source: str = "", vlm: str = "", origin: str = "",
          scene: str = "", subject: str = "", style: str = "", nsfw: str = "",
          min_quality: int = 0, limit: int = 200, offset: int = 0) -> dict:
    """属性・タグ・キャプションFTS・来歴で絞り込み。"""
    c = db()
    try:
        where, args = ["1=1"], []
        if source:
            where.append("source LIKE ?"); args.append(source + "%")
        if origin:
            where.append("origin=?"); args.append(origin)
        if vlm == "done":
            where.append("vlm_model IS NOT NULL")
        elif vlm == "none":
            where.append("vlm_model IS NULL")
        for col, val in (("scene", scene), ("subject", subject), ("style", style)):
            if val:
                where.append(f"{col}=?"); args.append(val)
        if nsfw in ("0", "1"):
            where.append("nsfw=?"); args.append(int(nsfw))
        if min_quality:
            where.append("quality>=?"); args.append(min_quality)
        if tag:
            where.append("sha1 IN (SELECT sha1 FROM tags WHERE tag=?)"); args.append(tag.lower())
        if q:
            where.append("sha1 IN (SELECT sha1 FROM captions WHERE captions MATCH ?)")
            args.append(q)
        sql = f"FROM images WHERE {' AND '.join(where)}"
        total = c.execute(f"SELECT COUNT(*) {sql}", args).fetchone()[0]
        rows = c.execute(f"SELECT * {sql} ORDER BY ingested DESC LIMIT ? OFFSET ?",
                         args + [limit, offset]).fetchall()
        return {"total": total, "items": [dict(r) for r in rows]}
    finally:
        c.close()


def facets() -> dict:
    """フィルタUI用の内訳(ソース別/シーン別/タグ上位など)。"""
    c = db()
    try:
        f = {
            "total": c.execute("SELECT COUNT(*) FROM images").fetchone()[0],
            "bytes": c.execute("SELECT COALESCE(SUM(bytes),0) FROM images").fetchone()[0],
            "enriched": c.execute("SELECT COUNT(*) FROM images WHERE vlm_model IS NOT NULL").fetchone()[0],
            "origins": {r[0] or "?": r[1] for r in c.execute(
                "SELECT origin, COUNT(*) FROM images GROUP BY origin")},
            "sources": {r[0] or "?": r[1] for r in c.execute(
                "SELECT source, COUNT(*) FROM images GROUP BY source ORDER BY 2 DESC")},
            "scenes": {r[0]: r[1] for r in c.execute(
                "SELECT scene, COUNT(*) FROM images WHERE scene IS NOT NULL GROUP BY scene ORDER BY 2 DESC LIMIT 20")},
            "subjects": {r[0]: r[1] for r in c.execute(
                "SELECT subject, COUNT(*) FROM images WHERE subject IS NOT NULL GROUP BY subject ORDER BY 2 DESC LIMIT 20")},
            "styles": {r[0]: r[1] for r in c.execute(
                "SELECT style, COUNT(*) FROM images WHERE style IS NOT NULL GROUP BY style ORDER BY 2 DESC LIMIT 12")},
            "tags": {r[0]: r[1] for r in c.execute(
                "SELECT tag, COUNT(*) FROM tags GROUP BY tag ORDER BY 2 DESC LIMIT 40")},
        }
        return f
    finally:
        c.close()


def materialize(name: str, shas: list[str]) -> dict:
    """絞り込み結果をsymlinkディレクトリとして払い出す(atelier配合/ml-hub import両対応)。"""
    import re
    slug = re.sub(r"[^a-zA-Z0-9_\-]", "_", name)[:48] or "dataset"
    out = DATASETS / slug
    out.mkdir(parents=True, exist_ok=True)
    n = 0
    for sha1 in shas:
        m = load_meta(sha1)
        if not m:
            continue
        src = image_path(sha1, m["ext"])
        link = out / f"{sha1}.{m['ext']}"
        if not link.exists() and src.exists():
            link.symlink_to(src.resolve())
            n += 1
    manifest = {"name": slug, "created": time.time(), "count": len(list(out.glob("*.*"))) - 1
                if (out / "manifest.json").exists() else n, "criteria": "api"}
    manifest["count"] = len([p for p in out.iterdir() if p.suffix.lower().lstrip('.') != 'json'])
    (out / "manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=1))
    return {"name": slug, "dir": str(out), "count": manifest["count"], "linked": n}


def rebuild() -> int:
    """サイドカー正本からSQLiteを作り直す(壊れても怖くない)。"""
    DB.unlink(missing_ok=True)
    c = db()
    n = 0
    try:
        for p in META.rglob("*.json"):
            try:
                index_meta(c, json.loads(p.read_text()))
                n += 1
            except (OSError, ValueError):
                pass
        c.commit()
    finally:
        c.close()
    return n

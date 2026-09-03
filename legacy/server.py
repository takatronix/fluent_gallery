"""fluent_gallery サーバ — 画像ライブラリ(:8790)。収蔵・属性・検索・払い出し。

起動: cd ~/fluent_gallery && nohup ~/qwen-anime/.venv/bin/uvicorn server:app \
      --host 0.0.0.0 --port 8790 > server.log 2>&1 &
"""
from __future__ import annotations

import threading
import time
from pathlib import Path

from fastapi import FastAPI, HTTPException
from fastapi.responses import FileResponse
from pydantic import BaseModel

import library as lib
import vlm

app = FastAPI(title="fluent_gallery")
AT = Path(__file__).resolve().parent


@app.get("/")
def index():
    return FileResponse(AT / "ui.html")


class IngestIn(BaseModel):
    path: str
    source: str = "import"
    move: bool = False
    origin: str = ""              # synthetic | real | ""=sourceから推定


@app.post("/api/ingest")
def api_ingest(i: IngestIn):
    p = Path(i.path).expanduser()
    if not p.exists():
        raise HTTPException(404, i.path)
    return lib.ingest(p, i.source, i.move, i.origin)


@app.get("/api/images")
def api_images(tag: str = "", q: str = "", source: str = "", vlm_: str = "", origin: str = "",
               scene: str = "", subject: str = "", style: str = "", nsfw: str = "",
               min_quality: int = 0, limit: int = 200, offset: int = 0):
    return lib.query(tag=tag, q=q, source=source, vlm=vlm_, origin=origin, scene=scene, subject=subject,
                     style=style, nsfw=nsfw, min_quality=min_quality,
                     limit=min(limit, 500), offset=offset)


@app.get("/api/facets")
def api_facets():
    return lib.facets()


@app.get("/api/meta/{sha1}")
def api_meta(sha1: str):
    m = lib.load_meta(sha1)
    if not m:
        raise HTTPException(404)
    return m


@app.get("/img/{sha1}")
def img(sha1: str):
    m = lib.load_meta(sha1)
    if not m:
        raise HTTPException(404)
    return FileResponse(lib.image_path(sha1, m["ext"]),
                        headers={"Cache-Control": "public, max-age=86400"})


@app.get("/thumb/{sha1}")
def thumb(sha1: str):
    m = lib.load_meta(sha1)
    if not m:
        raise HTTPException(404)
    c = lib.THUMBS / sha1[:2] / f"{sha1}.webp"
    if not c.exists():
        from PIL import Image
        c.parent.mkdir(parents=True, exist_ok=True)
        with Image.open(lib.image_path(sha1, m["ext"])) as im:
            im.thumbnail((360, 360))
            im.convert("RGB").save(c, "WEBP", quality=82)
    return FileResponse(c, headers={"Cache-Control": "public, max-age=86400"})


# ---- VLM enrich(逐次ジョブ。1枚ごとにサイドカー保存=いつ止めても無駄にならない) ----
_enrich = {"alive": False, "done": 0, "total": 0, "errors": 0, "backend": "", "last": "",
           "stop": False}


class EnrichIn(BaseModel):
    backend: str = "builtin"      # builtin | claude | gpt
    tag: str = ""
    q: str = ""
    source: str = ""
    only_missing: bool = True
    n: int = 0                    # 0=該当全部


def _enrich_job(sel: dict, backend: str, n: int):
    _enrich.update(alive=True, done=0, errors=0, backend=backend, stop=False, last="")
    try:
        if backend == "builtin":
            if err := vlm.ensure_builtin():
                _enrich.update(last=err, alive=False)
                return
        r = lib.query(**sel, limit=100000)
        items = r["items"][:n] if n else r["items"]
        _enrich["total"] = len(items)
        c = lib.db()
        for it in items:
            if _enrich["stop"]:
                break
            m = lib.load_meta(it["sha1"])
            if not m:
                continue
            res = vlm.describe(lib.image_path(it["sha1"], m["ext"]), backend)
            if "error" in res:
                _enrich["errors"] += 1
                _enrich["last"] = res["error"]
            else:
                m["vlm"] = {"model": f"{backend}/{vlm.BUILTIN_MODEL if backend == 'builtin' else backend}",
                            "ts": time.time(), "caption": res.get("caption", ""),
                            "tags": res.get("tags") or [], "attrs": res.get("attrs") or {}}
                lib.save_meta(m)
                lib.index_meta(c, m)
                c.commit()
                _enrich["last"] = (res.get("caption") or "")[:80]
            _enrich["done"] += 1
        c.close()
    finally:
        _enrich["alive"] = False


@app.post("/api/enrich")
def api_enrich(e: EnrichIn):
    if _enrich["alive"]:
        raise HTTPException(409, "enrichが実行中です")
    sel = {"tag": e.tag, "q": e.q, "source": e.source}
    if e.only_missing:
        sel["vlm"] = "none"
    threading.Thread(target=_enrich_job, args=(sel, e.backend, e.n), daemon=True).start()
    return {"ok": True}


@app.post("/api/enrich/stop")
def api_enrich_stop():
    _enrich["stop"] = True
    return {"ok": True}


@app.get("/api/enrich/status")
def api_enrich_status():
    return _enrich


# ---- データセット払い出し ----
class DatasetIn(BaseModel):
    name: str
    shas: list[str] = []          # 明示指定(UIの選択)
    tag: str = ""                 # またはフィルタ一式
    q: str = ""
    source: str = ""
    scene: str = ""
    subject: str = ""
    style: str = ""
    nsfw: str = ""
    min_quality: int = 0


@app.post("/api/datasets")
def api_dataset(d: DatasetIn):
    shas = d.shas
    if not shas:
        r = lib.query(tag=d.tag, q=d.q, source=d.source, scene=d.scene, subject=d.subject,
                      style=d.style, nsfw=d.nsfw, min_quality=d.min_quality, limit=100000)
        shas = [x["sha1"] for x in r["items"]]
    if not shas:
        raise HTTPException(400, "該当画像がありません")
    return lib.materialize(d.name, shas)


@app.get("/api/datasets")
def api_datasets():
    out = []
    for p in sorted(lib.DATASETS.glob("*/manifest.json")):
        try:
            import json
            m = json.loads(p.read_text())
            m["dir"] = str(p.parent)
            out.append(m)
        except (OSError, ValueError):
            pass
    return out


@app.delete("/api/datasets/{name}")
def api_dataset_del(name: str):
    import shutil
    if "/" in name:
        raise HTTPException(400)
    d = lib.DATASETS / name
    if not d.exists():
        raise HTTPException(404)
    shutil.rmtree(d)                              # 中身はsymlinkなので本体は消えない
    return {"ok": True}


@app.post("/api/rebuild")
def api_rebuild():
    return {"indexed": lib.rebuild()}


# ---- 参考画像×言語指示の量産(GPU仕事はatelierのキューに依頼) ----
class GenVarIn(BaseModel):
    shas: list[str]
    instruction: str
    per_ref: int = 4
    name: str = ""


@app.post("/api/genvar")
def api_genvar(g: GenVarIn):
    """選択画像を一時データセットに実体化→atelier(:8772)のキューへ量産依頼。
    生成が終わるとatelier側がこのライブラリへ自動収蔵する(source=atelier_var:<name>)。"""
    import json
    import urllib.request
    if not g.shas:
        raise HTTPException(400, "参考画像を選んでください")
    refs = lib.materialize(f"_refs_{int(time.time())}", g.shas)
    req = urllib.request.Request("http://127.0.0.1:8772/api/genvar",
        data=json.dumps({"refs_path": refs["dir"], "instruction": g.instruction,
                         "per_ref": g.per_ref, "name": g.name}).encode(),
        headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=90) as r:
            return json.load(r)
    except urllib.error.HTTPError as ex:
        raise HTTPException(ex.code, ex.read().decode()[:300])
    except Exception as ex:
        raise HTTPException(502, f"atelier(:8772)に繋がりません: {ex!r}")

# ---- 一般的なプリセット(手元にある定番データの一括収蔵) ----
PRESETS = {
    "coco_val2017":  {"path": "~/qwen-anime/data/val2017", "origin": "real",
                      "label": "🌍 COCO val2017 (実写5,000枚)"},
    "coco_train2017": {"path": "~/qwen-anime/data/train2017", "origin": "real",
                       "label": "🌍 COCO train2017 (実写118,287枚・重い)"},
    "places_indoor": {"path": "~/qwen-anime/data/places_indoor", "origin": "real",
                      "label": "🛋️ Places365 室内 (実写8,300枚)"},
    "collected":     {"path": "~/qwen-anime/data/collected", "origin": "real",
                      "label": "🌐 Web収集 (実写6,006枚)"},
    "faces_synth":   {"path": "~/qwen-anime/data/faces_synth", "origin": "synthetic",
                      "label": "🧑 合成顔 (生成3,000枚)"},
    "scenes_synth":  {"path": "~/qwen-anime/data/scenes_synth", "origin": "synthetic",
                      "label": "🏠 合成室内 (生成3,000枚)"},
    "webcam_captured": {"path": "~/qwen-anime/data/webcam_captured", "origin": "real",
                        "label": "📷 部屋キャプチャ (実写)"},
}


@app.get("/api/presets")
def api_presets():
    out = []
    for pid, p in PRESETS.items():
        d = Path(p["path"]).expanduser()
        out.append({"id": pid, "label": p["label"], "origin": p["origin"],
                    "available": d.exists(),
                    "n": len(list(d.glob("*.jpg")) + list(d.glob("*.png"))) if d.exists() else 0})
    return out


@app.post("/api/presets/{pid}")
def api_preset_ingest(pid: str):
    p = PRESETS.get(pid)
    if not p:
        raise HTTPException(404, pid)
    d = Path(p["path"]).expanduser()
    if not d.exists():
        raise HTTPException(404, f"{p['path']} がまだありません")
    return lib.ingest(d, f"preset:{pid}", origin=p["origin"])


@app.get("/api/datasets/{name}/shas")
def api_dataset_shas(name: str):
    if "/" in name:
        raise HTTPException(400)
    d = lib.DATASETS / name
    if not d.exists():
        raise HTTPException(404)
    return {"shas": [p.stem for p in d.iterdir() if p.suffix.lower() != ".json"]}

"""VLMアダプタ — 画像に属性(caption/tags/attrs)を付ける。

builtin(既定) = ollama qwen2.5vl:7b。ローカル・無料・アイドルでVRAM自動解放。
claude / gpt = ml-hub の settings.json のキーを流用(高精度が要る仕分け用)。
出力はJSON強制。壊れたら1回だけ再試行、それでも駄目なら {"error": ...} を返す。
"""
from __future__ import annotations

import base64
import json
import urllib.request
from pathlib import Path

OLLAMA = "http://127.0.0.1:11434"
BUILTIN_MODEL = "qwen2.5vl:7b"
_MLHUB_SETTINGS = Path.home() / "ml-hub/config/settings.json"

PROMPT = """Describe this image for a dataset library. Reply with ONLY a JSON object:
{"caption": "one sentence, concrete, in English",
 "tags": ["5-12 short lowercase tags"],
 "attrs": {"scene": "indoor|outdoor|studio|street|nature|abstract|other",
           "subject": "person|face|animal|food|vehicle|building|object|landscape|text|other",
           "lighting": "daylight|night|indoor|studio|dramatic|flat|other",
           "style": "photo|illustration|anime|3dcg|painting|sketch|other",
           "quality": 1-10, "nsfw": true|false}}"""


def _post_json(url: str, payload: dict, headers: dict | None = None, timeout: int = 180) -> dict:
    req = urllib.request.Request(url, data=json.dumps(payload).encode(),
                                 headers={"Content-Type": "application/json", **(headers or {})})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def _parse(text: str) -> dict:
    t = text.strip()
    if t.startswith("```"):
        t = t.split("```")[1].lstrip("json").strip()
    a, b = t.find("{"), t.rfind("}")
    return json.loads(t[a:b + 1])


def ensure_builtin() -> str | None:
    """内蔵モデルが無ければpull(初回のみ、~6GB)。進捗はollama側ログ。"""
    try:
        with urllib.request.urlopen(f"{OLLAMA}/api/tags", timeout=5) as r:
            have = [m["name"] for m in json.load(r)["models"]]
        if not any(BUILTIN_MODEL.split(":")[0] in m for m in have):
            _post_json(f"{OLLAMA}/api/pull", {"model": BUILTIN_MODEL, "stream": False}, timeout=1800)
        return None
    except Exception as ex:
        return f"ollamaに接続できません: {ex!r}"


def describe(image_path: Path, backend: str = "builtin") -> dict:
    b64 = base64.b64encode(image_path.read_bytes()).decode()
    for attempt in (0, 1):
        try:
            if backend == "builtin":
                r = _post_json(f"{OLLAMA}/api/generate", {
                    "model": BUILTIN_MODEL, "prompt": PROMPT, "images": [b64],
                    "stream": False, "format": "json",
                    "options": {"temperature": 0.1 + attempt * 0.4}})
                return _parse(r["response"])
            cfg = json.loads(_MLHUB_SETTINGS.read_text())
            if backend == "claude":
                r = _post_json("https://api.anthropic.com/v1/messages", {
                    "model": "claude-sonnet-5", "max_tokens": 700,
                    "messages": [{"role": "user", "content": [
                        {"type": "image", "source": {"type": "base64",
                         "media_type": "image/png" if image_path.suffix == ".png" else "image/jpeg",
                         "data": b64}},
                        {"type": "text", "text": PROMPT}]}],
                }, headers={"x-api-key": cfg["anthropic_api_key"],
                            "anthropic-version": "2023-06-01"})
                return _parse(r["content"][0]["text"])
            if backend == "gpt":
                r = _post_json("https://api.openai.com/v1/chat/completions", {
                    "model": "gpt-5.2", "max_completion_tokens": 700,
                    "messages": [{"role": "user", "content": [
                        {"type": "image_url", "image_url": {"url": f"data:image/png;base64,{b64}"}},
                        {"type": "text", "text": PROMPT}]}],
                }, headers={"Authorization": "Bearer " + cfg["openai_api_key"]})
                return _parse(r["choices"][0]["message"]["content"])
            return {"error": f"unknown backend: {backend}"}
        except Exception as ex:
            if attempt:
                return {"error": repr(ex)[:200]}
    return {"error": "unreachable"}

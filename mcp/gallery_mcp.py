#!/usr/bin/env python3
"""fluent_gallery MCP サーバー(stdio・依存ゼロ)。

ギャラリー(:8790)のHTTP APIを薄くラップしてAI(Claude Code等)から
収集の状態確認・開始/停止・台帳確認・任意APIデバッグをできるようにする。
登録(こぴぺ):
  claude mcp add gallery -- python3 /home/takatronix/fluent_gallery/mcp/gallery_mcp.py
"""
import json
import sys
import urllib.request
import urllib.error

BASE = "http://localhost:8790"

TOOLS = [
    {"name": "crawl_status",
     "description": "収集クローラーの現在状態(候補/検査/収蔵/却下理由ストリップ/コスト/順番待ち)を返す",
     "inputSchema": {"type": "object", "properties": {}}},
    {"name": "crawl_start",
     "description": "指定フォルダ(アルバム)の収集を開始する",
     "inputSchema": {"type": "object", "properties": {
         "album": {"type": "string", "description": "フォルダ名(例: IVE)"}},
         "required": ["album"]}},
    {"name": "crawl_stop",
     "description": "収集を停止する",
     "inputSchema": {"type": "object", "properties": {}}},
    {"name": "albums",
     "description": "フォルダ(アルバム)一覧: goal/engines/予算/直近runの成績を返す",
     "inputSchema": {"type": "object", "properties": {}}},
    {"name": "ledger",
     "description": "フォルダの収集台帳(使用済みクエリ/既読URL数/目標解釈brief)を読む",
     "inputSchema": {"type": "object", "properties": {
         "album": {"type": "string"}}, "required": ["album"]}},
    {"name": "api",
     "description": "ギャラリーの任意HTTP APIを叩く(デバッグ用)。例: path=/api/crawl/status",
     "inputSchema": {"type": "object", "properties": {
         "path": {"type": "string", "description": "/api/... で始まるパス"},
         "method": {"type": "string", "enum": ["GET", "POST", "DELETE"], "default": "GET"},
         "body": {"type": "object", "description": "POST時のJSONボディ"}},
         "required": ["path"]}},
]


def http(path, method="GET", body=None, timeout=60):
    req = urllib.request.Request(BASE + path, method=method)
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, data=data, timeout=timeout) as r:
            t = r.read().decode()
    except urllib.error.HTTPError as e:
        return {"http_error": e.code, "detail": e.read().decode()[:500]}
    except Exception as e:
        return {"error": str(e)}
    try:
        return json.loads(t)
    except Exception:
        return {"text": t[:2000]}


def call_tool(name, args):
    if name == "crawl_status":
        return http("/api/crawl/status")
    if name == "crawl_start":
        return http("/api/crawl", "POST", {"album": args["album"]})
    if name == "crawl_stop":
        return http("/api/crawl/stop", "POST", {})
    if name == "albums":
        a = http("/api/albums")
        if isinstance(a, list):  # recentは長いので落とす
            for x in a:
                (x.get("last_run") or {}).pop("recent", None)
        return a
    if name == "ledger":
        p = f"/home/takatronix/fluent_gallery/store/crawl/{args['album']}.ledger.json"
        try:
            d = json.load(open(p))
            return {"queries": d.get("queries"), "urls": len(d.get("urls", [])),
                    "brief": d.get("brief", "")}
        except Exception as e:
            return {"error": str(e)}
    if name == "api":
        return http(args["path"], args.get("method", "GET"), args.get("body"))
    return {"error": f"unknown tool {name}"}


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except Exception:
            continue
        rid = req.get("id")
        m = req.get("method")
        resp = None
        if m == "initialize":
            resp = {"protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fluent-gallery", "version": "1.0.0"}}
        elif m == "tools/list":
            resp = {"tools": TOOLS}
        elif m == "tools/call":
            r = call_tool(req["params"]["name"], req["params"].get("arguments") or {})
            resp = {"content": [{"type": "text", "text": json.dumps(r, ensure_ascii=False)[:60000]}]}
        elif rid is None:
            continue  # notification(initialized等)は無応答でよい
        else:
            sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": rid,
                                         "error": {"code": -32601, "message": f"unknown {m}"}}) + "\n")
            sys.stdout.flush()
            continue
        if rid is not None:
            sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": rid, "result": resp},
                                        ensure_ascii=False) + "\n")
            sys.stdout.flush()


if __name__ == "__main__":
    main()

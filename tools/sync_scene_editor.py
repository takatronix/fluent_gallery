#!/usr/bin/env python3
"""Vendor the real fluent_scene Studio and apply the small gallery embedding patch.

Run: python3 tools/sync_scene_editor.py [../fluent_scene]
The upstream checkout is read only. gallery-bridge.js is inserted into Studio's
module scope because image buffers and the renderer are intentionally private.
"""
from pathlib import Path
import hashlib
import json
import re
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
UPSTREAM = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else ROOT.parent / "fluent_scene"
DEST = ROOT / "web" / "fluent-scene"
DEST.mkdir(parents=True, exist_ok=True)
DIST = UPSTREAM / "wasm" / "dist"
html = (DIST / "edit.html").read_text()
original_hash = hashlib.sha256(html.encode()).hexdigest()
stateful_names = re.findall(r"FS_FILTER_STATEFUL(?:_IMG)?\(\w+,\s*(\w+),", (UPSTREAM / "include/fluent_scene/shared/filters_def.h").read_text())


def patch(old, new, count=1):
    global html
    found = html.count(old)
    if found != count:
        raise RuntimeError(f"Upstream changed: expected {count}, found {found}: {old[:100]!r}")
    html = html.replace(old, new)


patch("const STAGE_W = 1280, STAGE_H = 720;", """// Gallery embedding keeps the actual Studio and renderer, with a still-image host.
const galleryQuery = new URLSearchParams(location.search);
const GALLERY_EMBED = galleryQuery.get('gallery') === '1' && parent !== window;
const galleryState = { session: galleryQuery.get('session') || '', loaded: false,
  exporting: false, loading: false, dirty: 3, frames: 0, bindings: new Map(),
  width: 1280, height: 720, base: null, chainTask: null, sourceSerial: 0, generation: 0 };
let STAGE_W = 1280, STAGE_H = 720;""")
patch("sourceSerial: 0, generation: 0", "sourceSerial: 0, generation: 0, statefulNames: new Set(" + json.dumps(stateful_names) + ")")
patch("const FEED_W = COARSE ? 640 : STAGE_W, FEED_H = COARSE ? 360 : STAGE_H;",
      "let FEED_W = COARSE ? 640 : STAGE_W, FEED_H = COARSE ? 360 : STAGE_H;")
patch("  nodes.push(n);\n  return n;", "  if (GALLERY_EMBED && type === 'src' && extra.galleryBinding) galleryBindSource(n, extra.galleryBinding);\n  nodes.push(n);\n  return n;")
patch("function pushChains(t) {\n  if (!inst) return;", "function pushChains(t) {\n  if (GALLERY_EMBED) galleryState.dirty = Math.max(galleryState.dirty, 3);\n  if (!inst) return;")
patch("  if (!confirm('グラフを最初の状態に戻しますか?')) return;", "  if (!GALLERY_EMBED && !confirm('グラフを最初の状態に戻しますか?')) return;\n  ++applySeq;")
patch("  const keep = nodes.find(n => n.type === 'src');\n  for (const n of nodes)", "  const keep = nodes.find(n => n.type === 'src');\n  if (GALLERY_EMBED) galleryResetSource(keep);\n  for (const n of nodes)")
patch("      if (n.type === 'src') o.k = n.kind === 'camera' ? 'cam' : 'smp';", "      if (n.type === 'src') {\n        o.k = n.kind === 'camera' ? 'cam' : 'smp';\n        if (GALLERY_EMBED && n.galleryBinding) o.gb = n.galleryBinding;\n      }")
patch("o.c = n.code.slice(0, 20000);", "o.c = GALLERY_EMBED ? n.code : n.code.slice(0, 20000);")
patch("if (n.code.length > 20000) warnLongOnce", "if (!GALLERY_EMBED && n.code.length > 20000) warnLongOnce")
patch("packed.post = { c: postCode.slice(0, 20000), on: postState.on ? 1 : 0 };", "packed.post = { c: GALLERY_EMBED ? postCode : postCode.slice(0, 20000), on: postState.on ? 1 : 0 };")
patch("if (postCode.length > 20000) warnLongOnce", "if (!GALLERY_EMBED && postCode.length > 20000) warnLongOnce")
patch("            { kind: o.k === 'cam' ? 'camera' : 'sample' });", "            { kind: !GALLERY_EMBED && o.k === 'cam' ? 'camera' : 'sample',\n              galleryBinding: GALLERY_EMBED ? o.gb : undefined });")
patch("async function setBranchChain(spec, comment, pick = null) {", "async function studioSetBranchChain(spec, comment, pick = null) {")
patch("const RECIPES = [", "const RECIPES = [\n  // Gallery's common outline instruction uses the real Studio edge filter.\n  [/境界線|輪郭線|輪郭だけ|線画|エッジだけ|canny/, [['edge_sobel', {}]], '境界線を抽出しました (Sobel)'],")
patch("if (!/人物|背景|ポートレ|自分以外|人だけ/.test(t)) {", "if (!/人物|背景|ポートレ|自分以外|人だけ|境界線|輪郭線|輪郭だけ|エッジだけ|canny/.test(t)) {")
patch("async function generate() {\n  const text", "async function generate() {\n  const galleryGeneration = galleryState.generation;\n  const text")
for provider in ["askClaude", "askOpenAI"]:
    line = "      const r = parseAiJson(await " + provider + "(text));"
    patch(line, line + "\n      if (GALLERY_EMBED && galleryGeneration !== galleryState.generation) return;")
patch("  s.video = null; s.img = null;", "  s.video = null; s.img = null;\n  if (GALLERY_EMBED) delete s.galleryBinding;")
patch("      s.kind = 'image'; s.img = img; s.dirty = true;", "      s.kind = 'image'; s.img = img; s.dirty = true;\n      if (GALLERY_EMBED) galleryRememberSource(s, f, img);")
patch("        if (grab.width !== FEED_W) { grab.width = FEED_W; grab.height = FEED_H; }", "        if (grab.width !== FEED_W || grab.height !== FEED_H) { grab.width = FEED_W; grab.height = FEED_H; }")
patch("        g.fillStyle = '#000';\n        g.fillRect(0, 0, FEED_W, FEED_H);", "        if (GALLERY_EMBED) g.clearRect(0, 0, FEED_W, FEED_H);\n        else { g.fillStyle = '#000'; g.fillRect(0, 0, FEED_W, FEED_H); }")
patch("if (!loadHash()) {\n  const src", "if (GALLERY_EMBED) {\n  const src = addNode('src', 40, 140, { kind: 'none' });\n  const out = addNode('out', 300, 140);\n  connect(src.id, 0, out.id);\n} else if (!loadHash()) {\n  const src")
patch("setTheater(localStorage.getItem('fs_theater') === '1');", "setTheater(!GALLERY_EMBED && localStorage.getItem('fs_theater') === '1');")
patch("  if (gpu) return;\n  if (fpsShown > 0", "  if (gpu || GALLERY_EMBED) return;\n  if (fpsShown > 0")
patch("function tick() {\n  const now", "function tick() {\n  if (GALLERY_EMBED && !galleryNeedsFrame()) { requestAnimationFrame(tick); return; }\n  if (GALLERY_EMBED) { galleryState.dirty = Math.max(0, galleryState.dirty - 1); ++galleryState.frames; }\n  const now")
patch("requestAnimationFrame(tick);\n</script>", (DEST / "gallery-bridge.js").read_text() + "\nrequestAnimationFrame(tick);\n</script>")
patch("</style>\n<div id=\"aurora\">", "</style>\n<link rel=\"stylesheet\" href=\"./gallery-bridge.css\">\n<div id=\"aurora\">")

for filename in ["fluent_scene.mjs", "fluent_scene.wasm", "washi.jpg", "wcpaper.jpg", "canvas.jpg", "grain.jpg", "dust.jpg", "leak.jpg"]:
    shutil.copy2(DIST / filename, DEST / filename)
shutil.copytree(DIST / "luts", DEST / "luts", dirs_exist_ok=True)
for filename in ["LICENSE", "THIRD_PARTY.md"]:
    shutil.copy2(UPSTREAM / filename, DEST / filename)
(DEST / "edit.html").write_text(html)
commit = subprocess.check_output(["git", "-C", str(UPSTREAM), "rev-parse", "HEAD"], text=True).strip()
(DEST / "UPSTREAM.json").write_text(json.dumps({
    "repository": "https://github.com/takatronix/fluent_scene",
    "commit": commit, "entry": "wasm/dist/edit.html", "entry_sha256": original_hash,
    "sync": "python3 tools/sync_scene_editor.py ../fluent_scene",
    "patch": "Gallery image input, aspect ratio, original-size PNG export, source bindings and host messaging. Studio UI and filters remain upstream."
}, ensure_ascii=False, indent=2) + "\n")
print(f"Vendored fluent_scene Studio {commit[:12]} into {DEST}")

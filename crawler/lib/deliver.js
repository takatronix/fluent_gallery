'use strict';
// gallery への引き渡し(spec §3)と、gallery 無しの保存(deliver:false)
const fs = require('fs');
const path = require('path');

const EXT = { 'image/jpeg': 'jpg', 'image/png': 'png', 'image/webp': 'webp', 'image/gif': 'gif', 'image/avif': 'avif' };
const extOf = (type) => EXT[type] || 'jpg';

/** POST {gallery}/api/deliver(multipart: meta → file)。ネットワーク失敗は throw、判定結果は {ok, ...} で返す */
async function deliver(gallery, meta, buf, type, { timeoutMs = 240000 } = {}) {
  const fd = new FormData();
  fd.append('meta', JSON.stringify(meta));
  fd.append('file', new Blob([buf], { type }), `image.${extOf(type)}`);
  const ac = new AbortController();
  const t = setTimeout(() => ac.abort(), timeoutMs);
  try {
    const r = await fetch(gallery.replace(/\/$/, '') + '/api/deliver', { method: 'POST', body: fd, signal: ac.signal });
    const j = await r.json().catch(() => ({}));
    if (!r.ok) { const e = new Error(`HTTP ${r.status} ${j.detail || ''}`.trim()); e.status = r.status; throw e; }
    return j;
  } finally { clearTimeout(t); }
}

function saveLocal(dir, sha1, type, buf, meta) {
  fs.mkdirSync(dir, { recursive: true });
  const ext = extOf(type);
  fs.writeFileSync(path.join(dir, `${sha1}.${ext}`), buf);
  fs.writeFileSync(path.join(dir, `${sha1}.json`), JSON.stringify(meta, null, 1));
  return `${sha1}.${ext}`;
}

module.exports = { deliver, saveLocal, extOf };

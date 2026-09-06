// 編集中に遅いVLM分類が完了しても、履歴/版/分類のすべてを保持する。
// FG_URL=http://127.0.0.1:<isolated-test-port> node tests/edit_metadata_race.js
// /tmp 配下の専用データルート限定。外部AIは使わずローカルの応答ゲートで競合を再現。
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const http = require('node:http');
const zlib = require('node:zlib');

const BASE = process.env.FG_URL;
assert(BASE, 'FG_URL must point to an isolated test server');
const target = new URL(BASE);
assert(['127.0.0.1', 'localhost', '[::1]'].includes(target.hostname) && target.port !== '8790',
  'Only an isolated local server is allowed');
const nonce = crypto.randomBytes(8).toString('hex');
const source = `crawl:_editrace_${nonce}`;
const created = new Set(), pending = new Set();
let mock, held, settings, configChanged = false, checks = 0;
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const passed = label => { checks++; console.log(`PASS ${label}`); };

async function api(path, body, method = body === undefined ? 'GET' : 'POST') {
  const response = await fetch(BASE + path, {method, signal: AbortSignal.timeout(30000),
    headers: body === undefined ? {} : {'Content-Type': 'application/json'},
    body: body === undefined ? undefined : JSON.stringify(body)});
  const text = await response.text();
  assert(response.ok, `${method} ${path}: ${response.status} ${text}`);
  return text ? JSON.parse(text) : null;
}
async function until(probe, label, timeout = 20000) {
  const start = Date.now();
  while (Date.now() - start < timeout) {
    const value = await probe();
    if (value) return value;
    await sleep(20);
  }
  throw new Error(`Timeout waiting for ${label}`);
}
function crc32(bytes) {
  let crc = 0xffffffff;
  for (const value of bytes) {
    crc ^= value;
    for (let bit = 0; bit < 8; bit++) crc = (crc >>> 1) ^ (crc & 1 ? 0xedb88320 : 0);
  }
  return (crc ^ 0xffffffff) >>> 0;
}
function chunk(type, bytes) {
  const name = Buffer.from(type), size = Buffer.alloc(4), crc = Buffer.alloc(4);
  size.writeUInt32BE(bytes.length); crc.writeUInt32BE(crc32(Buffer.concat([name, bytes])));
  return Buffer.concat([size, name, bytes, crc]);
}
function fixture(index) {
  const width = 96, height = 48, stride = 1 + width * 3;
  const data = Buffer.alloc(height * stride), id = Buffer.from(nonce + index);
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      const value = y === 0 && x < id.length ? id[x] : 12 + Math.round(x * 80 / (width - 1));
      data.fill(value, y * stride + 1 + x * 3, y * stride + 4 + x * 3);
    }
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0); ihdr.writeUInt32BE(height, 4); ihdr[8] = 8; ihdr[9] = 2;
  return Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
    chunk('IHDR', ihdr), chunk('IDAT', zlib.deflateSync(data)), chunk('IEND', Buffer.alloc(0))]);
}
async function upload(index) {
  const bytes = fixture(index), sha = crypto.createHash('sha1').update(bytes).digest('hex');
  const form = new FormData(); form.append('source', source);
  form.append('file', new Blob([bytes], {type: 'image/png'}), `race-${index}.png`);
  const response = await fetch(BASE + '/api/upload', {method: 'POST', body: form});
  const result = await response.json();
  assert(response.ok && result.added === 1, JSON.stringify(result));
  created.add(sha);
  assert.equal((await api('/api/meta/' + sha)).sha1, sha);
  return {sha, bytes};
}
function classifyResult() {
  return {caption: `metadata race fixture ${nonce}`, tags: ['gray', 'gradient', 'test', 'neutral', 'ramp'],
    attrs: {scene: 'abstract', subject: 'other', gender: 'none', animal: 'none', people_count: '0',
      age_group: 'none', framing: 'wide', watermark: false, lighting: 'flat', style: 'other',
      quality: 5, nsfw: false}};
}
function releaseClassification() {
  if (!held) return;
  const response = held; held = null;
  response.writeHead(200, {'Content-Type': 'application/json'});
  response.end(JSON.stringify({choices: [{message: {role: 'assistant', content: JSON.stringify(classifyResult())}}]}));
}
async function withDelayedClassification(sha, run, batch = false) {
  assert(!held, 'previous mock response still pending');
  assert(!(await api('/api/meta/' + sha)).vlm, 'fixture must begin without classification');
  let request;
  if (batch) {
    await api('/api/enrich', {backend: 'builtin', n: 1, only_missing: true, source});
  } else {
    request = api('/api/enrich/one', {sha1: sha, backend: 'builtin'});
    pending.add(request); request.catch(() => {});
  }
  try {
    await until(() => held, 'VLM request at the controlled response gate');
    const expected = await run();
    releaseClassification();
    let classified;
    if (batch) {
      await until(async () => !(await api('/api/enrich/status')).alive, 'batch classification commit');
      const status = await api('/api/enrich/status');
      assert.equal(status.errors, 0); assert.equal(status.done, 1);
      classified = await api('/api/meta/' + sha);
    } else {
      classified = await request;
    }
    const saved = await api('/api/edits/' + sha);
    assert.deepEqual(saved.edits, expected.edits, 'delayed classification discarded committed edit history');
    assert.equal(saved.rev, expected.rev, 'delayed classification changed the edit revision');
    assert.deepEqual(classified.edits, expected.edits, 'classification result returned stale edit history');
    assert.equal(classified.vlm.caption, classifyResult().caption, 'classification itself was not saved');
    const listing = await api('/api/images?' + new URLSearchParams({source, view: 'grid', limit: '20'}));
    assert.equal(listing.items.find(image => image.sha1 === sha).erev, expected.rev,
      'indexed revision must match the saved edit history');
  } finally {
    releaseClassification();
    if (request) { await request.catch(() => {}); pending.delete(request); }
  }
}

(async () => {
  settings = await api('/api/settings');
  assert(settings.root.startsWith('/tmp/'), `refusing non-test root ${settings.root}`);
  assert(!settings.env.FG_VLM_BASE, 'FG_VLM_BASE override prevents use of the local mock');
  assert(!(await api('/api/enrich/status')).alive, 'test requires idle enrichment');
  mock = http.createServer((request, response) => {
    if (request.url === '/health') {
      response.writeHead(200, {'Content-Type': 'application/json'}); response.end('{"status":"ok"}');
    } else if (request.method === 'POST' && request.url === '/v1/chat/completions') {
      request.resume();
      request.on('end', () => {
        if (held) { response.writeHead(409); response.end('unexpected parallel classification'); return; }
        held = response;
      });
    } else { response.writeHead(404); response.end(); }
  });
  await new Promise((resolve, reject) => { mock.once('error', reject); mock.listen(0, '127.0.0.1', resolve); });
  await api('/api/settings', {path: 'vlm.base', value: `http://127.0.0.1:${mock.address().port}/v1`}, 'PATCH');
  configChanged = true;

  const first = await upload('one');
  await withDelayedClassification(first.sha, async () => {
    const pushed = await api('/api/edits/' + first.sha,
      {action: 'push', edit: {op: 'auto', params: {}}}, 'PUT');
    assert.equal(pushed.edits.length, 1); assert.equal(pushed.edits[0].op, 'auto');
    assert.equal(pushed.edits[0].params.version, 2);
    return pushed;
  });
  passed('delayed single-image VLM preserves auto history, revision, grid index and classification');

  for (let round = 0; round < 3; round++) {
    await api('/api/edits/' + first.sha, {action: 'clear'}, 'PUT');
    const labels = Array.from({length: 16}, (_, index) => `edit-${round}-${index}`);
    await Promise.all(labels.map(label => api('/api/edits/' + first.sha,
      {action: 'push', edit: {op: 'adjust', label, params: {exposure: 0.001}}}, 'PUT')));
    const saved = await api('/api/edits/' + first.sha);
    assert.equal(saved.edits.length, labels.length, 'concurrent pushes lost history entries');
    assert.deepEqual(saved.edits.map(edit => edit.label).sort(), labels.sort());
    assert.equal((await api('/api/meta/' + first.sha)).vlm.caption, classifyResult().caption);
  }
  passed('3 rounds of 16 concurrent edit pushes retain every distinct operation and classification');

  const second = await upload('batch');
  await withDelayedClassification(second.sha, () => api('/api/edits/' + second.sha,
    {action: 'push', edit: {op: 'auto', params: {}}}, 'PUT'), true);
  passed('delayed batch VLM also preserves committed auto history and index');
  for (const image of [first, second]) {
    const response = await fetch(BASE + '/img/' + image.sha);
    assert(response.ok);
    assert.deepEqual(Buffer.from(await response.arrayBuffer()), image.bytes, 'original PNG changed');
  }
  passed('original PNG bytes remain unchanged during enrichment and editing');
  console.log(`${checks} checks passed`);
})().catch(error => { console.error(error.stack || error); process.exitCode = 1; }).finally(async () => {
  releaseClassification();
  await Promise.allSettled([...pending]);
  if (configChanged) {
    await api('/api/settings', {path: 'vlm.base', value: settings.config.vlm.base}, 'PATCH')
      .catch(error => { console.error('restore config:', error.message); process.exitCode = 1; });
  }
  if (created.size) await api('/api/trash', {shas: [...created]}).catch(error => {
    console.error('cleanup fixtures:', error.message); process.exitCode = 1;
  });
  if (mock) await new Promise(resolve => { mock.close(resolve); mock.closeAllConnections(); });
});

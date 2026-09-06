// Baked fluent_scene saves: original preservation, alpha/full size, provenance and revision races.
// Run against an isolated data root: FG_URL=http://127.0.0.1:<port> node tests/studio_save.js
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const zlib = require('node:zlib');
const BASE = process.env.FG_URL;
assert(BASE, 'Set FG_URL to an isolated test server');
const nonce = crypto.randomBytes(8).toString('hex');
const source = `upload:studio_test_${nonce}`;
const created = new Set();
let checks = 0;
const passed = label => { checks++; console.log(`✓ ${label}`); };
const hash = bytes => crypto.createHash('sha1').update(bytes).digest('hex');

function chunk(type, data) {
  const name = Buffer.from(type), result = Buffer.alloc(data.length + 12);
  result.writeUInt32BE(data.length); name.copy(result, 4); data.copy(result, 8);
  let crc = 0xffffffff;
  for (const byte of Buffer.concat([name, data])) {
    crc ^= byte;
    for (let i = 0; i < 8; i++) crc = (crc >>> 1) ^ (0xedb88320 & -(crc & 1));
  }
  result.writeUInt32BE((~crc) >>> 0, result.length - 4);
  return result;
}
function png(width, height, adjusted = false) {
  const header = Buffer.alloc(13); header.writeUInt32BE(width); header.writeUInt32BE(height, 4);
  header[8] = 8; header[9] = 6;
  const rows = Buffer.alloc((width * 4 + 1) * height);
  for (let y = 0; y < height; y++) for (let x = 0; x < width; x++) {
    const offset = y * (width * 4 + 1) + 1 + x * 4;
    rows[offset] = adjusted ? 230 : 40;
    rows[offset + 1] = 35 + ((x + parseInt(nonce.slice(0, 2), 16)) % 100);
    rows[offset + 2] = 45 + y % 150;
    rows[offset + 3] = x < 100 ? 71 : 255;
  }
  return Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), chunk('IHDR', header),
    chunk('tEXt', Buffer.from('test\0' + nonce)), chunk('IDAT', zlib.deflateSync(rows)), chunk('IEND', Buffer.alloc(0))]);
}
function rgbaPixel(bytes, x, y) {
  const parts = []; let width, height, color, depth;
  for (let offset = 8; offset + 12 <= bytes.length;) {
    const size = bytes.readUInt32BE(offset), type = bytes.toString('ascii', offset + 4, offset + 8);
    const data = bytes.subarray(offset + 8, offset + 8 + size);
    if (type === 'IHDR') { width = data.readUInt32BE(); height = data.readUInt32BE(4); depth = data[8]; color = data[9]; }
    if (type === 'IDAT') parts.push(data);
    offset += size + 12;
  }
  assert.equal(depth, 8); assert([2, 6].includes(color), 'RGB or RGBA PNG required');
  const channels = color === 6 ? 4 : 3;
  const packed = zlib.inflateSync(Buffer.concat(parts)), stride = width * channels;
  let previous = Buffer.alloc(stride);
  for (let row = 0; row <= y; row++) {
    const start = row * (stride + 1), filter = packed[start], current = Buffer.from(packed.subarray(start + 1, start + stride + 1));
    for (let i = 0; i < stride; i++) {
      const a = i >= channels ? current[i - channels] : 0, b = previous[i], c = i >= channels ? previous[i - channels] : 0;
      const p = a + b - c, pa = Math.abs(p - a), pb = Math.abs(p - b), pc = Math.abs(p - c);
      const prediction = filter === 0 ? 0 : filter === 1 ? a : filter === 2 ? b : filter === 3 ? Math.floor((a + b) / 2)
        : filter === 4 ? (pa <= pb && pa <= pc ? a : pb <= pc ? b : c) : NaN;
      assert(Number.isFinite(prediction)); current[i] = (current[i] + prediction) & 255;
    }
    previous = current;
  }
  const pixel = [...previous.subarray(x * channels, x * channels + channels)];
  if (channels === 3) pixel.push(255);
  return {width, height, pixel};
}
async function api(path, body, method = body === undefined ? 'GET' : 'POST') {
  const response = await fetch(BASE + path, {method, headers: body === undefined ? {} : {'Content-Type': 'application/json'},
    body: body === undefined ? undefined : JSON.stringify(body)});
  const text = await response.text(); assert(response.ok, `${path}: ${response.status} ${text}`);
  return text ? JSON.parse(text) : null;
}
async function bytes(path) {
  const response = await fetch(BASE + path); assert(response.ok, `${path}: ${response.status}`);
  return Buffer.from(await response.arrayBuffer());
}
async function save(sha, image, recipe, expected = 200) {
  const form = new FormData(); form.append('image', new Blob([image], {type: 'image/png'}), 'studio.png');
  form.append('recipe', JSON.stringify(recipe));
  const response = await fetch(BASE + '/api/studio/' + sha + '/save', {method: 'POST', body: form});
  const text = await response.text(); assert.equal(response.status, expected, text);
  const value = JSON.parse(text); if (value.sha1) created.add(value.sha1);
  return value;
}
async function preview(sha, body, expected = 200) {
  const response = await fetch(BASE + '/api/studio/' + sha + '/preview', {
    method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify(body)});
  assert.equal(response.status, expected, `preview ${await (response.status === expected ? Promise.resolve('') : response.text())}`);
  if (expected !== 200) return null;
  assert.equal(response.headers.get('content-type'), 'image/png');
  assert.equal(response.headers.get('cache-control'), 'no-store');
  return Buffer.from(await response.arrayBuffer());
}

(async () => {
  await api('/api/enrich/stop', {});
  const original = png(1280, 960), rendered = png(1280, 960, true);
  const form = new FormData(); form.append('source', source); form.append('file', new Blob([original]), 'source.png');
  const upload = await fetch(BASE + '/api/upload', {method: 'POST', body: form});
  assert(upload.ok); assert.equal((await upload.json()).added, 1);
  const sha = hash(original); created.add(sha);
  const edit = await api('/api/edits/' + sha, {action: 'push', edit: {op: 'adjust', params: {exposure: .25}}}, 'PUT');
  const before = await api('/api/meta/' + sha);
  const basePreview = await preview(sha, {edits: [], source_edits_rev: edit.rev});
  assert.deepEqual(rgbaPixel(basePreview, 10, 20), rgbaPixel(original, 10, 20));
  const photoEdits = [{op: 'adjust', params: {exposure: .5}}];
  const adjustedPreview = await preview(sha, {edits: photoEdits, source_edits_rev: edit.rev});
  assert.equal(rgbaPixel(adjustedPreview, 1200, 900).pixel[0], 80, 'draft runs on original pixels without reapplying current edits');
  const geometry = await preview(sha, {edits: [{op: 'crop', params: {fx: 0, fy: 0, fw: .5, fh: .5}},
    {op: 'rotate', params: {deg: 90}}], source_edits_rev: edit.rev});
  const cropped = rgbaPixel(geometry, 0, 0);
  assert.equal(cropped.width, 480); assert.equal(cropped.height, 640);
  assert.deepEqual(cropped.pixel, rgbaPixel(original, 0, 479).pixel);
  assert.deepEqual(await api('/api/meta/' + sha), before);
  assert.equal(hash(await bytes('/img/' + sha)), hash(original));
  assert.equal((await api('/api/images?' + new URLSearchParams({source}))).total, 1);
  passed('写真調整 draft preview applies exact supplied history at full resolution without saving or altering source');
  await preview(sha, {edits: [], source_edits_rev: '0'.repeat(12)}, 409);
  await preview(sha, {edits: [{op: 'adjust', params: {exposure: 99}}]}, 400);
  await preview(sha, {edits: [{op: 'crop', params: {fx: 0, fy: 0, fw: 0, fh: 1}}]}, 400);
  await preview(sha, {edits: Array.from({length: 65}, () => ({op: 'auto', params: {version: 3}}))}, 400);
  await preview(sha, {edits: [], ignored: 'x'.repeat(1 << 20)}, 413);
  passed('draft preview rejects stale revisions, invalid parameters, excessive operations and oversized requests');
  const recipe = {version: 1, source_edits_rev: edit.rev, width: 1280, height: 960, time: 0,
    photo_edits: photoEdits,
    graph: {n: [{i: 'n1', t: 'src', x: 40, y: 100, k: 'smp'}, {i: 'n2', t: 'filter', x: 240, y: 100, f: 'grayscale', v: {}},
      {i: 'n3', t: 'out', x: 440, y: 100}], e: [['n1', 0, 'n2', 0], ['n2', 0, 'n3', 0]]},
    sceneYaml: 'scene:\n  width: 1280\n  height: 960\n# ' + nonce};
  const result = await save(sha, rendered, recipe);
  assert.notEqual(result.sha1, sha); assert.equal(result.reused, false);
  assert.equal(result.meta.w, 1280); assert.equal(result.meta.h, 960);
  assert.equal(result.meta.source, source);
  assert(!result.meta.edits?.length, 'no render-time edit stack on baked output');
  assert.equal(result.meta.studio.source_sha, sha);
  assert.equal(result.meta.studio.source_edits_rev, edit.rev);
  assert.deepEqual(result.meta.studio.source_edits, edit.edits);
  assert.deepEqual(result.meta.studio.recipe, recipe);
  passed('full-size ordinary image has separate ID and exact source/recipe provenance');

  const resolved = await api('/api/original/' + result.sha1);
  assert.equal(resolved.sha1, sha); assert(resolved.derived);
  assert.deepEqual(resolved.chain, [result.sha1, sha]);
  assert.deepEqual(resolved.meta.edits, edit.edits);
  const plain = await api('/api/original/' + sha);
  assert.equal(plain.sha1, sha); assert.equal(plain.derived, false);
  const {source_edits_rev: omittedRevision, ...nestedRecipe} = recipe;
  const nested = await save(result.sha1, rendered, nestedRecipe);
  const nestedResolved = await api('/api/original/' + nested.sha1);
  assert.equal(nestedResolved.sha1, sha);
  assert.deepEqual(nestedResolved.chain, [nested.sha1, result.sha1, sha]);
  assert.deepEqual(await api('/api/meta/' + sha), before);
  passed('原本に戻す resolves baked provenance chains without modifying any source history');

  const output = await bytes('/img/' + result.sha1);
  assert.equal(hash(output), result.sha1, 'normal SHA-addressed image');
  assert.deepEqual(rgbaPixel(output, 10, 20), rgbaPixel(rendered, 10, 20));
  assert.deepEqual(rgbaPixel(output, 1200, 900), rgbaPixel(rendered, 1200, 900));
  assert.equal(hash(await bytes('/img/' + sha)), hash(original));
  const after = await api('/api/meta/' + sha);
  assert.deepEqual(after, before, 'source sidecar unchanged');
  passed('full-resolution pixels and transparency preserved; source bytes and metadata unchanged');

  for (const tier of ['micro', 'thumb', 'preview']) {
    const data = await bytes('/' + tier + '/' + result.sha1);
    assert.equal(data[0], 255); assert.equal(data[1], 216, tier + ' ready as JPEG');
  }
  const list = await api('/api/images?' + new URLSearchParams({source, exclude_filtered: 'true'}));
  assert(list.items.some(item => item.sha1 === result.sha1));
  assert(list.items.some(item => item.sha1 === sha));
  passed('baked preview tiers and output available in the source folder immediately');

  const repeats = await Promise.all([save(sha, rendered, recipe), save(sha, rendered, recipe)]);
  assert(repeats.every(value => value.sha1 === result.sha1 && value.reused));
  const different = await save(sha, rendered, {...recipe, sceneYaml: recipe.sceneYaml + '\n# second recipe'});
  assert.notEqual(different.sha1, result.sha1);
  passed('identical exports reuse their ID; distinct recipes retain distinct provenance');

  const asset = png(64, 48, true);
  const withAsset = {...recipe, assets: [{kind: 'source', key: 'upload-1', name: 'layer.png', data: 'data:image/png;base64,' + asset.toString('base64')}]};
  const assetResult = await save(sha, rendered, withAsset);
  const savedRecipe = assetResult.meta.studio.recipe;
  assert.match(savedRecipe.assets[0].data, /^\/studio-assets\/[a-f0-9]{40}\.png$/);
  assert.equal(hash(await bytes(savedRecipe.assets[0].data)), hash(asset));
  assert.equal((await save(sha, rendered, savedRecipe)).sha1, assetResult.sha1);
  assert.equal((await save(sha, rendered, withAsset)).sha1, assetResult.sha1);
  await save(sha, rendered, {...recipe, assets: [{kind: 'resource', name: 'bad', data: 'https://example.com/private.png'}]}, 400);
  await save(sha, rendered, {...recipe, assets: [{kind: 'resource', name: 'bad', data: '/studio-assets/../index.sqlite'}]}, 400);
  passed('extra layers stored as SHA assets, reopenable and reusable without bloating sidecars');

  const outputEdit = await api('/api/edits/' + result.sha1, {action: 'push', edit: {op: 'filter', params: {name: 'invert'}}}, 'PUT');
  const editedOutputMeta = await api('/api/meta/' + result.sha1);
  const repeatEdited = await save(sha, rendered, recipe);
  assert.notEqual(repeatEdited.sha1, result.sha1);
  assert.deepEqual((await api('/api/meta/' + result.sha1)).edits, outputEdit.edits);
  assert.deepEqual(await api('/api/meta/' + result.sha1), editedOutputMeta);
  passed('editing a previous output never gets overwritten by a later export');

  const current = await api('/api/edits/' + sha, {action: 'push', edit: {op: 'filter', params: {name: 'sepia'}}}, 'PUT');
  await save(sha, rendered, recipe, 409);
  await save('0'.repeat(40), rendered, {...recipe, source_edits_rev: current.rev}, 404);
  await save(sha, Buffer.from('broken png'), {...recipe, source_edits_rev: current.rev}, 400);
  await save(sha, rendered, {...recipe, graph: {}}, 400);
  await save(sha, rendered, {...recipe, photo_edits: [{op: 'adjust', params: {exposure: 99}}]}, 400);
  await save(sha, rendered, {...recipe, sceneYaml: 'x'.repeat(16 << 20)}, 413);
  passed('stale edits, missing source, malformed PNG/graph and oversized recipe rejected');

  // Header claims an oversized canvas but the compressed pixels remain tiny: reject before allocation.
  const header = Buffer.alloc(13); header.writeUInt32BE(8193); header.writeUInt32BE(960, 4); header[8] = 8; header[9] = 6;
  const oversized = Buffer.concat([rendered.subarray(0, 8), chunk('IHDR', header), rendered.subarray(33)]);
  await save(sha, oversized, {...recipe, source_edits_rev: current.rev}, 400);
  assert.equal(hash(await bytes('/img/' + sha)), hash(original));
  passed('oversized dimensions rejected before decode; original still intact');
  console.log(`\n${checks} checks passed`);
})().catch(error => { console.error(error.stack || error); process.exitCode = 1; }).finally(async () => {
  if (created.size) await api('/api/trash', {shas: [...created]}).catch(error => console.error('cleanup:', error.message));
});

// In-place fluent_scene edits: stable item counts/IDs, private baked pixels, Undo, originals and revisions.
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

async function recipeFor(sha, recipe) {
  const input = recipe.input_sha || sha;
  const [targetMeta, inputMeta] = await Promise.all([api('/api/meta/' + sha), api('/api/meta/' + input)]);
  return {...recipe, input_sha: input, source_edits_rev: inputMeta.edits_rev, target_edits_rev: targetMeta.edits_rev};
}
async function rendered(sha, revision) {
  return bytes('/render/' + sha + '?' + new URLSearchParams({v: revision, w: '0'}));
}
async function uploadPNG(data, name) {
  const form = new FormData(); form.append('source', source); form.append('file', new Blob([data]), name);
  const response = await fetch(BASE + '/api/upload', {method: 'POST', body: form});
  assert(response.ok); assert.equal((await response.json()).added, 1);
  const sha = hash(data); created.add(sha); return sha;
}
async function sourceItems() { return api('/api/images?' + new URLSearchParams({source})); }

(async () => {
  await api('/api/enrich/stop', {});
  const original = png(1280, 960), finalPixels = png(1280, 960, true);
  const sha = await uploadPNG(original, 'source.png');
  for (let i = 0; i < 16; i++) {
    const pushed = await api('/api/edits/' + sha, {action: 'push', edit: {op: 'adjust', params: {exposure: (i % 10) / 10}}}, 'PUT');
    const persisted = await api('/api/meta/' + sha);
    assert.equal(persisted.edits_rev, pushed.rev, 'PUT revision must match its reloaded sidecar');
    const response = await fetch(BASE + '/render/' + sha + '?' + new URLSearchParams({v: pushed.rev, w: '64'}));
    assert(response.ok, 'PUT revision renders immediately: ' + response.status); await response.arrayBuffer();
    await api('/api/edits/' + sha, {action: 'pop'}, 'PUT');
  }
  passed('edit revisions remain stable across PUT, persisted metadata and immediate rendering');
  const edit = await api('/api/edits/' + sha, {action: 'push', edit: {op: 'adjust', params: {exposure: .25}}}, 'PUT');
  const before = await api('/api/meta/' + sha);
  assert.equal(before.studio_save_mode, 'in_place', 'server advertises the safe save protocol');
  const originalTiers = new Map();
  for (const tier of ['micro', 'thumb', 'preview']) originalTiers.set(tier, hash(await bytes('/' + tier + '/' + sha)));
  const basePreview = await preview(sha, {edits: [], source_edits_rev: edit.rev});
  assert.deepEqual(rgbaPixel(basePreview, 10, 20), rgbaPixel(original, 10, 20));
  const photoEdits = [{op: 'adjust', params: {exposure: .5}}];
  const adjusted = await preview(sha, {edits: photoEdits, source_edits_rev: edit.rev});
  assert.equal(rgbaPixel(adjusted, 1200, 900).pixel[0], 80, 'draft does not reapply saved adjustments');
  const geometry = await preview(sha, {edits: [{op: 'crop', params: {fx: 0, fy: 0, fw: .5, fh: .5}},
    {op: 'rotate', params: {deg: 90}}], source_edits_rev: edit.rev});
  assert.deepEqual(rgbaPixel(geometry, 0, 0), {width: 480, height: 640, pixel: rgbaPixel(original, 0, 479).pixel});
  assert.deepEqual(await api('/api/meta/' + sha), before);
  assert.equal(hash(await bytes('/img/' + sha)), sha); assert.equal((await sourceItems()).total, 1);
  passed('full-resolution draft controls preserve source pixels, transparency and metadata');
  await preview(sha, {edits: [], source_edits_rev: '0'.repeat(12)}, 409);
  await preview(sha, {edits: [{op: 'adjust', params: {exposure: 99}}]}, 400);
  await preview(sha, {edits: [{op: 'studio', params: {render_sha: '../private'}}]}, 400);
  await preview(sha, {edits: [{op: 'studio', params: {render_sha: '0'.repeat(40)}}]}, 400);
  await preview(sha, {edits: Array.from({length: 65}, () => ({op: 'auto', params: {version: 3}}))}, 400);
  await preview(sha, {edits: [], ignored: 'x'.repeat(1 << 20)}, 413);
  passed('draft preview rejects stale revisions, bad references, parameters and oversized requests');

  const recipe = {save_mode: 'in_place', version: 1, input_sha: sha, width: 1280, height: 960, time: 0,
    photo_edits: photoEdits,
    graph: {n: [{i: 'n1', t: 'src', x: 40, y: 100, k: 'smp'}, {i: 'n2', t: 'filter', x: 240, y: 100, f: 'grayscale', v: {}},
      {i: 'n3', t: 'out', x: 440, y: 100}], e: [['n1', 0, 'n2', 0], ['n2', 0, 'n3', 0]]},
    sceneYaml: 'scene:\n  width: 1280\n  height: 960\n# ' + nonce};
  const {save_mode: omittedMode, ...oldProtocol} = await recipeFor(sha, recipe);
  await save(sha, finalPixels, oldProtocol, 400);
  assert.deepEqual(await api('/api/meta/' + sha), before);
  assert.equal((await sourceItems()).total, 1);
  passed('old clients without in_place save mode are rejected without creating or editing images');

  const firstRecipe = await recipeFor(sha, recipe);
  const first = await save(sha, finalPixels, firstRecipe);
  assert.equal(first.sha1, sha, 'Apply keeps the selected logical ID'); assert.equal(first.reused, false);
  const firstMarker = first.meta.edits.at(-1), firstRender = firstMarker.params.render_sha;
  assert.equal(firstMarker.op, 'studio'); assert.match(firstRender, /^[a-f0-9]{40}$/);
  assert.notEqual(firstRender, sha); assert.deepEqual(first.meta.edits.slice(0, -1), edit.edits);
  assert.equal(first.meta.studio_edit.source_sha, sha); assert.equal(first.meta.studio_edit.render_sha, firstRender);
  assert.deepEqual(first.meta.studio_edit.recipe, firstRecipe);
  assert.equal(first.meta.studio_edit.w, 1280); assert.equal(first.meta.studio_edit.h, 960);
  assert(!first.meta.studio, 'active edit must not become immutable source provenance');
  for (const field of ['sha1', 'ext', 'w', 'h', 'bytes', 'source', 'ingested', 'phash', 'tint']) {
    assert.deepEqual(first.meta[field], before[field], 'immutable original metadata: ' + field);
  }
  const list = await sourceItems(); assert.equal(list.total, 1); assert.equal(list.items[0].sha1, sha);
  assert.equal(list.items[0].erev, first.meta.edits_rev);
  assert.equal((await fetch(BASE + '/api/meta/' + firstRender)).status, 404);
  assert.equal((await fetch(BASE + '/img/' + firstRender)).status, 404);
  const firstPNG = await rendered(sha, first.meta.edits_rev);
  assert.equal(hash(firstPNG), firstRender); assert.deepEqual(rgbaPixel(firstPNG, 10, 20), rgbaPixel(finalPixels, 10, 20));
  assert.deepEqual(rgbaPixel(firstPNG, 1200, 900), rgbaPixel(finalPixels, 1200, 900));
  assert.equal(hash(await bytes('/img/' + sha)), sha);
  passed('Apply updates one existing item; full-resolution RGBA bake stays private and originals remain immutable');

  for (const tier of ['micro', 'thumb', 'preview']) {
    const data = await bytes('/' + tier + '/' + sha + '?v=' + first.meta.edits_rev);
    assert.equal(data[0], 255); assert.equal(data[1], 216);
    assert.notEqual(hash(data), originalTiers.get(tier), tier + ' shows the baked pixels');
    assert.equal(hash(await bytes('/' + tier + '/' + sha)), originalTiers.get(tier), tier + ' original URL remains immutable');
  }
  const resolved = await api('/api/original/' + sha);
  assert.equal(resolved.sha1, sha); assert.equal(resolved.derived, false); assert.deepEqual(resolved.chain, [sha]);
  const repeatsRecipe = await recipeFor(sha, recipe);
  const repeats = await Promise.all([save(sha, finalPixels, repeatsRecipe), save(sha, finalPixels, repeatsRecipe)]);
  assert(repeats.every(value => value.sha1 === sha && value.reused && value.meta.edits.length === first.meta.edits.length));
  assert.equal((await sourceItems()).total, 1);
  passed('versioned tiers reflect the edit; repeated identical saves do not add images or history entries');

  const second = await save(sha, original, await recipeFor(sha, {...recipe, sceneYaml: recipe.sceneYaml + '\n# second'}));
  assert.equal(second.sha1, sha); assert.notEqual(second.meta.studio_edit.render_sha, firstRender);
  assert.equal(second.meta.edits.length, first.meta.edits.length + 1);
  assert.deepEqual(rgbaPixel(await rendered(sha, second.meta.edits_rev), 10, 20), rgbaPixel(original, 10, 20));
  await api('/api/edits/' + sha, {action: 'pop'}, 'PUT');
  let current = await api('/api/meta/' + sha);
  assert.equal(current.studio_edit.render_sha, firstRender);
  assert.equal(hash(await rendered(sha, current.edits_rev)), firstRender);
  const inverted = await api('/api/edits/' + sha, {action: 'push', edit: {op: 'filter', params: {name: 'invert'}}}, 'PUT');
  const composed = await preview(sha, {edits: [firstMarker, inverted.edits.at(-1)], source_edits_rev: inverted.rev});
  assert.equal(rgbaPixel(composed, 1200, 900).pixel[0], 25, 'native edits are applied after the last baked image');
  current = await api('/api/meta/' + sha);
  assert.deepEqual(current.studio_edit.tail_edits, [inverted.edits.at(-1)]);
  await api('/api/edits/' + sha, {action: 'clear'}, 'PUT');
  current = await api('/api/meta/' + sha);
  assert(!current.studio_edit); assert.deepEqual(current.edits, []);
  assert.equal(hash(await rendered(sha, current.edits_rev)), sha); assert.equal(hash(await bytes('/img/' + sha)), sha);
  assert.equal((await sourceItems()).total, 1);
  passed('later saves, native edits, Undo and Reset retain one ID and restore exact earlier pixels');

  const asset = png(64, 48, true);
  const withAsset = {...recipe, assets: [{kind: 'source', key: 'upload-1', name: 'layer.png', data: 'data:image/png;base64,' + asset.toString('base64')}]};
  const assetResult = await save(sha, finalPixels, await recipeFor(sha, withAsset));
  const savedRecipe = assetResult.meta.studio_edit.recipe;
  assert.match(savedRecipe.assets[0].data, /^\/studio-assets\/[a-f0-9]{40}\.png$/);
  assert.equal(hash(await bytes(savedRecipe.assets[0].data)), hash(asset));
  const reopened = await save(sha, finalPixels, await recipeFor(sha, savedRecipe));
  assert(reopened.reused); assert.equal(reopened.meta.studio_edit.render_sha, assetResult.meta.studio_edit.render_sha);
  assert.equal((await sourceItems()).total, 1);
  passed('additional layers stay in separate reusable assets, with no extra gallery entries');

  const valid = await recipeFor(sha, recipe), invalidBefore = await api('/api/meta/' + sha);
  await save(sha, finalPixels, {...valid, target_edits_rev: '0'.repeat(12)}, 409);
  await save(sha, finalPixels, {...valid, source_edits_rev: '0'.repeat(12)}, 409);
  await save('0'.repeat(40), finalPixels, {...valid, input_sha: '0'.repeat(40)}, 404);
  await save(sha, Buffer.from('broken PNG'), valid, 400);
  await save(sha, finalPixels, {...valid, graph: {}}, 400);
  await save(sha, finalPixels, {...valid, photo_edits: [{op: 'adjust', params: {exposure: 99}}]}, 400);
  await save(sha, finalPixels, {...valid, photo_edits: [{op: 'studio', params: {render_sha: '0'.repeat(40)}}]}, 400);
  await save(sha, finalPixels, {...valid, sceneYaml: 'x'.repeat(16 << 20)}, 413);
  await save(sha, finalPixels, {...valid, assets: [{kind: 'resource', name: 'bad', data: 'https://example.com/private.png'}]}, 400);
  await save(sha, finalPixels, {...valid, assets: [{kind: 'resource', name: 'bad', data: '/studio-assets/../index.sqlite'}]}, 400);
  const header = Buffer.alloc(13); header.writeUInt32BE(8193); header.writeUInt32BE(960, 4); header[8] = 8; header[9] = 6;
  await save(sha, Buffer.concat([finalPixels.subarray(0, 8), chunk('IHDR', header), finalPixels.subarray(33)]), valid, 400);
  assert.deepEqual(await api('/api/meta/' + sha), invalidBefore); assert.equal((await sourceItems()).total, 1);
  passed('invalid and stale saves preserve the current item, its history and original bytes');

  const otherBytes = png(96, 64), otherSha = await uploadPNG(otherBytes, 'other-input.png');
  const otherBefore = await api('/api/meta/' + otherSha);
  const otherRecipe = await recipeFor(sha, {...recipe, input_sha: otherSha, photo_edits: []});
  await save(sha, otherBytes, {...otherRecipe, target_edits_rev: '0'.repeat(12)}, 409);
  await save(sha, otherBytes, {...otherRecipe, source_edits_rev: '0'.repeat(12)}, 409);
  const otherInputSave = await save(sha, otherBytes, otherRecipe);
  assert.equal(otherInputSave.sha1, sha); assert.equal(otherInputSave.meta.studio_edit.source_sha, otherSha);
  assert.equal(otherInputSave.meta.studio_edit.w, 96); assert.equal(otherInputSave.meta.studio_edit.h, 64);
  assert.deepEqual(await api('/api/meta/' + otherSha), otherBefore);
  assert.equal(hash(await bytes('/img/' + sha)), sha); assert.equal(hash(await bytes('/img/' + otherSha)), otherSha);
  const finalList = await sourceItems(); assert.equal(finalList.total, 2);
  assert.deepEqual(finalList.items.map(item => item.sha1).sort(), [sha, otherSha].sort());
  passed('separate input and selected image have independent revision guards; only the selected image changes');
  console.log(`\n${checks} checks passed`);
})().catch(error => { console.error(error.stack || error); process.exitCode = 1; }).finally(async () => {
  if (created.size) await api('/api/trash', {shas: [...created]}).catch(error => console.error('cleanup:', error.message));
});

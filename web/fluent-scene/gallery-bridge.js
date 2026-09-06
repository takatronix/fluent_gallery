// Inserted into the upstream Studio module by tools/sync_scene_editor.py.
// This bridge uses the real graph/compiler/WASM renderer; it has no filter copy.
function galleryBindSource(node, key) {
  const binding = galleryState.bindings.get(key);
  if (!node || !binding) return false;
  releaseSource(node);
  node.kind = 'image'; node.img = binding.image; node.dirty = true;
  node.galleryBinding = key;
  return true;
}
function galleryRememberSource(node, blob, image) {
  const key = 'upload-' + (++galleryState.sourceSerial);
  galleryState.bindings.set(key, { blob, image, name: blob.name || 'image.png' });
  node.galleryBinding = key;
}
function galleryResetSource(node) {
  ++galleryState.generation;
  if (galleryState.base) galleryBindSource(node, 'gallery');
  postCode = ''; postState.on = false;
  postState.c?.classList.remove('on');
}
function setBranchChain(...args) {
  const task = studioSetBranchChain(...args);
  if (GALLERY_EMBED) galleryState.chainTask = task;
  return task;
}
function galleryNeedsFrame() {
  if (galleryState.exporting || galleryState.loading || !galleryState.loaded || document.hidden) return false;
  if (galleryState.dirty > 0 || peekOn || nodes.some(n => n.type === 'src' && n.dirty)) return true;
  if (nodes.some(n => n.type === 'src' && ['camera', 'video', 'sample'].includes(n.kind))) return true;
  if (postState.on || wantFeedback() || nodes.some(n => ['person', 'hands', 'face'].includes(n.type))) return true;
  if (units.some(u => u.deco?.glsl?.on)) return true;
  return anim.checked && (animatedGraph() || nodes.some(n => n.type === 'filter' && n.on && galleryState.statefulNames.has(n.name)));
}
function galleryPost(type, data = {}) {
  parent.postMessage({ type, session: galleryState.session, ...data }, location.origin);
}
function galleryPreviewSize() {
  // CPU art filters can have hundreds of samples per pixel. Keep interaction
  // inexpensive; the original pixels are fed again only for the saved PNG.
  const cap = gpu ? (COARSE ? 640 : 1280) : (COARSE ? 480 : 640);
  const scale = Math.min(1, cap / Math.max(galleryState.width, galleryState.height));
  return [Math.max(1, Math.round(galleryState.width * scale)), Math.max(1, Math.round(galleryState.height * scale))];
}
function gallerySetSize(width, height) {
  if (!Number.isSafeInteger(width) || !Number.isSafeInteger(height) || width < 1 || height < 1 ||
      width > 8192 || height > 8192 || width * height > 32000000) throw new Error('画像サイズは最大8192px・3200万画素です');
  galleryState.width = width; galleryState.height = height;
  // Logical units are stable at a 1280px long side, matching Studio's filter controls.
  const logicalScale = Math.min(1, 1280 / Math.max(width, height));
  STAGE_W = width * logicalScale; STAGE_H = height * logicalScale;
  [FEED_W, FEED_H] = galleryPreviewSize();
  canvas.width = FEED_W; canvas.height = FEED_H;
  document.documentElement.style.setProperty('--gallery-aspect', String(width / height));
}
async function galleryDecode(blob) {
  if (!(blob instanceof Blob) || !blob.type.startsWith('image/')) throw new Error('画像データがありません');
  const url = URL.createObjectURL(blob);
  try { const image = new Image(); image.src = url; await image.decode(); return image; }
  finally { URL.revokeObjectURL(url); }
}
async function galleryRestoreAssets(assets) {
  if (!assets) return;
  if (!Array.isArray(assets) || assets.length > 64) throw new Error('保存された画像リソースが不正です');
  for (const asset of assets) {
    if (!asset || typeof asset.data !== 'string') throw new Error('保存された画像リソースが不正です');
    const inline = /^data:image\/(png|jpeg|webp);base64,/.test(asset.data);
    const stored = /^\/studio-assets\/[a-f0-9]{40}\.(png|jpg|jpeg|webp)$/.test(asset.data);
    if (!inline && !stored) throw new Error('保存された画像リソースのURLが不正です');
    const response = await fetch(asset.data);
    if (!response.ok) throw new Error('保存された画像リソースを読み込めません');
    const blob = await response.blob();
    if (asset.kind === 'source') {
      const image = await galleryDecode(blob);
      galleryState.bindings.set(String(asset.key), { image, blob, name: asset.name || 'image.png' });
      galleryState.sourceSerial = Math.max(galleryState.sourceSerial, Number(String(asset.key).replace(/^upload-/, '')) || 0);
    } else if (asset.kind === 'resource') {
      const image = await galleryDecode(blob);
      await resPut({ n: String(asset.name).slice(0, 40), b: blob, w: image.naturalWidth, h: image.naturalHeight });
    } else throw new Error('保存された画像リソースが不正です');
  }
}
async function galleryInit(message) {
  if (galleryState.loading || galleryState.exporting) throw new Error('画像を処理中です');
  galleryState.loading = true; galleryState.loaded = false;
  try {
    const image = await galleryDecode(message.image);
    gallerySetSize(image.naturalWidth, image.naturalHeight);
    galleryState.bindings.clear();
    galleryState.base = { image, blob: message.image, name: message.filename || 'image.png' };
    galleryState.bindings.set('gallery', galleryState.base);
    const saved = message.recipe || message;
    await galleryRestoreAssets(saved.assets);
    for (const node of nodes) if (node.type === 'src') releaseSource(node);
    ++applySeq; ++galleryState.generation;
    if (saved.graph) {
      if (!Array.isArray(saved.graph.n) || !Array.isArray(saved.graph.e) || saved.graph.n.length > 80) throw new Error('保存されたグラフが不正です');
      unpackGraph(saved.graph);
      // Older gallery graphs may not yet have source-binding identifiers.
      const first = nodes.find(n => n.type === 'src');
      if (first && !first.galleryBinding) galleryBindSource(first, 'gallery');
    } else {
      nodes = []; edges = []; nextId = 1;
      const src = addNode('src', 40, 140, { kind: 'none', galleryBinding: 'gallery' });
      const out = addNode('out', 300, 140);
      connect(src.id, 0, out.id);
      if (postState.on) { postState.on = false; postState.c?.classList.remove('on'); }
      postCode = '';
    }
    simT = Number.isFinite(saved.time) ? Math.max(0, saved.time) : 0;
    simLast = performance.now(); undoStack = []; selId = null;
    applyGraph(true);
    if (nodes.some(n => n.type === 'person')) loadSegmenter();
    if (nodes.some(n => n.type === 'hands')) loadHands();
    galleryState.loaded = true; galleryState.dirty = 4;
    say('画像を読み込みました — 言葉・🎲・ノードでフィルタを編集できます');
  } finally { galleryState.loading = false; }
  // Render and commit the selected image before the host enables Save.
  const before = galleryState.frames;
  await new Promise(resolve => {
    const poll = () => galleryState.frames > before ? resolve() : requestAnimationFrame(poll);
    requestAnimationFrame(poll);
  });
  galleryPost('fg-studio-loaded', { width: galleryState.width, height: galleryState.height });
}
async function galleryDataUrl(blob) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader(); reader.onload = () => resolve(reader.result);
    reader.onerror = () => reject(new Error('画像リソースを書き出せません'));
    reader.readAsDataURL(blob);
  });
}
async function galleryPackAssets(graph) {
  const assets = [], sourceKeys = new Set(), resourceNames = new Set();
  for (const node of graph.n) {
    if (node.t === 'src' && node.gb && node.gb !== 'gallery') sourceKeys.add(node.gb);
    if (node.t === 'filter' && node.tx?.startsWith('r:')) resourceNames.add(node.tx.slice(2));
    if (node.t === 'glsl' && node.tx) resourceNames.add(node.tx);
    if (node.t === 'face' && node.fr) resourceNames.add(node.fr);
  }
  for (const key of sourceKeys) {
    const binding = galleryState.bindings.get(key);
    if (!binding?.blob) throw new Error('追加した入力画像が見つかりません');
    let blob = binding.blob;
    if (!['image/png', 'image/jpeg', 'image/webp'].includes(blob.type)) {
      const source = document.createElement('canvas');
      source.width = binding.image.naturalWidth; source.height = binding.image.naturalHeight;
      source.getContext('2d').drawImage(binding.image, 0, 0);
      blob = await new Promise((resolve, reject) => source.toBlob(b => b ? resolve(b) : reject(new Error('追加画像をPNGに変換できません')), 'image/png'));
    }
    assets.push({ kind: 'source', key, name: binding.name, data: await galleryDataUrl(blob) });
  }
  for (const name of resourceNames) {
    const resource = await resGet(name);
    if (!resource?.b) throw new Error('画像リソース「' + name + '」が見つかりません');
    assets.push({ kind: 'resource', name, data: await galleryDataUrl(resource.b) });
  }
  return assets;
}
async function galleryWaitAssets() {
  const started = performance.now();
  for (;;) {
    pushChains(simT);
    let ready = true;
    for (const node of nodes.filter(n => n.type === 'filter' && n.on)) {
      if (papers[node.name] && !papers[node.name][2]) ready = false;
      if (node.name === 'lut' && node.lut && !lutData[node.lut]) ready = false;
      if (node.name === 'texture') {
        const key = node.tex || 'grain';
        if (key.startsWith('r:')) { const data = texResData.get(key); if (!data || data === 'bad') ready = false; }
        else if (!texData[key]) ready = false;
      }
    }
    for (const node of nodes.filter(n => n.type === 'glsl' && n.on && n.tex)) {
      if (!ensureProc()) throw new Error('シェーダの描画にWebGL2が必要です');
      nodeTexFor(procGL, node);
      if (!procTexCache.get(node.tex)) ready = false;
    }
    if (nodes.some(n => n.type === 'person') && !segmenter) ready = false;
    if (ready) return;
    if (performance.now() - started > 12000) throw new Error('フィルタの画像やモデルの読み込みが完了していません');
    await sleep(50);
  }
}
async function galleryExport() {
  if (!galleryState.loaded || galleryState.loading) throw new Error('画像を読み込み中です');
  if (galleryState.exporting) throw new Error('画像を保存中です');
  if (aiGo.disabled) throw new Error('フィルタの生成が終わってから保存してください');
  galleryState.exporting = true;
  document.body.inert = true;
  document.body.classList.add('gallery-exporting');
  const oldCanvas = canvas, oldCtx = ctx, oldFeed = [FEED_W, FEED_H];
  const frozenTime = simT;
  try {
    if (galleryState.chainTask) await galleryState.chainTask;
    await galleryWaitAssets();
    const graph = packGraph();
    const recipe = { version: 1, graph, sceneYaml: exportYaml(), time: frozenTime,
      width: galleryState.width, height: galleryState.height, assets: await galleryPackAssets(graph) };
    if (new TextEncoder().encode(JSON.stringify(recipe)).length > 16 * 1024 * 1024) {
      throw new Error('追加画像を含む編集データが16MBを超えています。追加画像を小さくしてから保存してください');
    }
    // Feed actual source pixels again at export size, before applying every node.
    FEED_W = galleryState.width; FEED_H = galleryState.height;
    for (const node of nodes) if (node.type === 'src') node.dirty = true;
    feedSources(frozenTime);
    updateMask(performance.now());
    pushChains(frozenTime);
    canvas = document.createElement('canvas'); canvas.width = FEED_W; canvas.height = FEED_H;
    ctx = canvas.getContext('2d');
    const px = fs.render(inst, 0, FEED_W, FEED_H);
    if (!px || fs.renderW(inst) !== FEED_W || fs.renderH(inst) !== FEED_H || fs.renderStride(inst) !== FEED_W * 4) throw new Error('原寸画像を描画できませんでした');
    // Copy from WASM before toBlob yields; later allocations may grow its memory.
    const pixels = new Uint8ClampedArray(mod.HEAPU8.buffer, px, FEED_W * FEED_H * 4).slice();
    ctx.putImageData(new ImageData(pixels, FEED_W, FEED_H), 0, 0);
    drawPost(frozenTime);
    const target = postState.on && postState.c ? postState.c : canvas;
    const image = await new Promise((resolve, reject) => target.toBlob(blob => blob ? resolve(blob) : reject(new Error('PNGを書き出せませんでした')), 'image/png'));
    return { image, recipe };
  } finally {
    canvas = oldCanvas; ctx = oldCtx; [FEED_W, FEED_H] = oldFeed;
    for (const node of nodes) if (node.type === 'src') node.dirty = true;
    simT = frozenTime; simLast = performance.now();
    galleryState.exporting = false; galleryState.dirty = 4;
    document.body.inert = false;
    document.body.classList.remove('gallery-exporting');
  }
}
if (GALLERY_EMBED) {
  document.body.classList.add('gallery-embed');
  const queryWidth = Number(galleryQuery.get('w')), queryHeight = Number(galleryQuery.get('h'));
  if (queryWidth > 0 && queryHeight > 0) gallerySetSize(queryWidth, queryHeight);
  // Resume only when Studio changes, or when an animated graph actually needs frames.
  for (const event of ['input', 'click', 'pointermove', 'keydown', 'visibilitychange']) {
    document.addEventListener(event, () => { galleryState.dirty = Math.max(3, galleryState.dirty); }, { passive: true });
  }
  window.addEventListener('message', async event => {
    if (event.source !== parent || event.origin !== location.origin || event.data?.session !== galleryState.session) return;
    const message = event.data;
    try {
      if (message.type === 'fg-studio-init') await galleryInit(message);
      else if (message.type === 'fg-studio-export') {
        const result = await galleryExport();
        galleryPost('fg-studio-exported', { requestId: message.requestId, ...result });
      }
    } catch (error) {
      galleryPost('fg-studio-error', { requestId: message.requestId, message: error.message || String(error) });
      say(error.message || String(error), true);
    }
  });
  Object.assign(window.__studio, { gallery: {
    get loaded() { return galleryState.loaded; }, get exporting() { return galleryState.exporting; },
    get frames() { return galleryState.frames; }, get size() { return [galleryState.width, galleryState.height]; },
    get feedSize() { return [FEED_W, FEED_H]; }, get stageSize() { return [STAGE_W, STAGE_H]; },
    export: galleryExport, pack: packGraph
  } });
  galleryPost('fg-studio-ready');
}

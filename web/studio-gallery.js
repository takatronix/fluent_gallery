// The gallery owns the photo controls, preview and save. Studio supplies its
// renderer and real node/parameter editor inside the same editing panel.
let studioSession = null;
$('lbstudiobtn').disabled = false;
$('lbstudiobtn').title = 'フィルターを個別に編集';
for (const id of ['studio-generate', 'studio-random', 'studio-reset']) $(id).disabled = false;
function studioId() {
  return Array.from(crypto.getRandomValues(new Uint8Array(16)), v => v.toString(16).padStart(2, '0')).join('');
}
function studioIsBusy() {
  const s = studioSession;
  return !!s && (!s.ready || s.saving || s.updating || s.busy);
}
function studioStatus(state, message) {
  if (studioSession !== state) return;
  $('studio-status').textContent = message;
  edSyncStatus();
}
function studioSetBusy(state) {
  if (studioSession !== state) return;
  const busy = studioIsBusy();
  for (const id of ['studio-generate', 'studio-random', 'studio-reset', 'lbstudiobtn']) $(id).disabled = busy;
  state.frame?.style.setProperty('pointer-events', busy ? 'none' : 'auto');
  edSyncStatus();
}
function studioSnapshot(state) {
  return {graph: state.graph ? structuredClone(state.graph) : null,
    photoEdits: structuredClone(state.photoEdits), time: state.time || 0, source: state.source};
}
function studioRemember(state) {
  if (!state.graph) return;
  state.history.push(studioSnapshot(state));
  if (state.history.length > 40) state.history.shift();
}
function studioHistory(state) {
  if (studioSession !== state) return;
  edHist(state.photoEdits);
}
async function studioImage(state) {
  const response = await fetch(`/api/studio/${state.source.sha1}/preview`, {
    method: 'POST', signal: state.controller.signal,
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({edits: state.photoEdits, source_edits_rev: state.source.edits_rev})});
  if (!response.ok) {
    let detail; try { detail = (await response.json()).detail; } catch (_) {}
    throw new Error(detail || '写真の調整結果を読み込めませんでした');
  }
  return response.blob();
}
async function studioOpen() {
  if (studioSession) {
    const existing = studioSession;
    try { await existing.initialized; } catch (_) { return null; }
    return studioSession === existing ? existing : null;
  }
  const item = items[lbIdx];
  if (!item) return null;
  const state = {session: studioId(), selectedSha: item.sha1, context: lbContext,
    controller: new AbortController(), ready: false, saving: false, updating: false, busy: 0,
    frame: null, requests: new Map(), history: [], graph: null, time: 0, sourceRevision: '0', previewRevision: -1};
  state.initialized = new Promise((resolve, reject) => { state.resolveReady = resolve; state.rejectReady = reject; });
  state.initialized.catch(() => {});
  studioSession = state;
  $('lb').classList.add('editing', 'scene-editing');
  $('studio-inline').hidden = false;
  $('studio-loading').hidden = false;
  $('studio-loading').textContent = 'フィルターを準備しています…';
  lbView.refit();
  studioStatus(state, 'フィルターを準備しています…');
  studioSetBusy(state);
  try {
    await edQueues.get(item.sha1);
    if (studioSession !== state) return null;
    const options = {signal: state.controller.signal};
    let meta = await j('/api/meta/' + item.sha1, options), recipe = null;
    if (meta.studio?.recipe?.graph && !meta.edits?.length) {
      const saved = meta.studio;
      const original = await j('/api/meta/' + saved.source_sha, options);
      recipe = saved.recipe;
      // Photos saved by this editor preserve their input adjustment snapshot.
      state.photoEdits = structuredClone(recipe.photo_edits || saved.source_edits || original.edits || []);
      meta = original;
    } else if (meta.filter_source_sha) {
      const original = await j('/api/original/' + meta.sha1, options);
      meta = original.meta;
      state.fromOriginal = true;
    }
    state.source = meta;
    state.photoEdits ??= structuredClone(meta.edits || []);
    state.recipe = recipe;
    state.image = await studioImage(state);
    const bitmap = await createImageBitmap(state.image);
    state.width = bitmap.width; state.height = bitmap.height; bitmap.close();
    if (studioSession !== state) return null;
    const frame = document.createElement('iframe');
    frame.id = 'studio-frame'; frame.title = '個別フィルターとパラメーター';
    frame.src = '/fluent-scene/edit.html?' + new URLSearchParams({gallery: '1', mode: 'inline',
      w: state.width, h: state.height, session: state.session});
    state.frame = frame;
    $('studio-frame-wrap').append(frame);
    state.loadTimeout = setTimeout(() => state.rejectReady(new Error('フィルターを読み込めませんでした')), 60000);
    await state.initialized;
    return studioSession === state ? state : null;
  } catch (error) {
    if (studioSession === state) {
      state.rejectReady(error);
      studioClose();
      $('edstatus').textContent = error.message;
      toast('❌ ' + error.message);
    }
    return null;
  }
}
function studioRequest(state, type, payload = {}) {
  const requestId = studioId();
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      state.requests.delete(requestId); reject(new Error('フィルターの処理がタイムアウトしました'));
    }, type === 'fg-studio-export' ? 180000 : 60000);
    state.requests.set(requestId, {resolve, reject, timer});
    state.frame.contentWindow.postMessage({type, session: state.session, requestId, ...payload}, location.origin);
  });
}
function studioFinishRequest(state, message, error) {
  const request = state.requests.get(message.requestId);
  if (!request) return;
  state.requests.delete(message.requestId); clearTimeout(request.timer);
  error ? request.reject(error) : request.resolve(message);
}
function studioPaint(state, blob) {
  const url = URL.createObjectURL(blob);
  state.previewUrls ??= new Set(); state.previewUrls.add(url);
  lbView.edited = true; lbView.comparing = false;
  return lbView.apply(lbView.cancelPending(), url, 'scene', undefined, undefined, true).then(applied => {
    if (!applied || studioSession !== state) { URL.revokeObjectURL(url); state.previewUrls.delete(url); return false; }
    for (const old of state.previewUrls) if (old !== url) { URL.revokeObjectURL(old); state.previewUrls.delete(old); }
    edLive(); return true;
  }).catch(error => { studioStatus(state, error.message); return false; });
}
async function studioCompare(on) {
  const state = studioSession;
  if (!state?.ready) return Promise.resolve(false);
  state.comparing = on;
  if (on) {
    $('lbimg').style.filter = '';
    const original = state.source.studio || state.source.filter_source_sha ? await edOriginal(state.source.sha1) : {sha1: state.source.sha1};
    if (studioSession !== state || !state.comparing) return false;
    return lbView.apply(lbView.cancelPending(), '/img/' + original.sha1, 'original', undefined, undefined, true);
  }
  return state.lastPreview ? studioPaint(state, state.lastPreview) : Promise.resolve(false);
}
window.addEventListener('message', event => {
  const state = studioSession, message = event.data;
  if (!state || event.source !== state.frame?.contentWindow || event.origin !== location.origin ||
      !message || message.session !== state.session) return;
  if (message.type === 'fg-studio-ready') {
    state.frame.contentWindow.postMessage({type: 'fg-studio-init', session: state.session,
      image: state.image, filename: state.source.sha1 + '.png', recipe: state.recipe,
      sourceRevision: state.sourceRevision}, location.origin);
  } else if (message.type === 'fg-studio-loaded') {
    if (String(message.sourceRevision ?? '0') !== state.sourceRevision) return;
    clearTimeout(state.loadTimeout);
    state.ready = true;
    $('edcompare').style.display = '';
    $('studio-loading').hidden = true;
    state.resolveReady(state);
    if (message.requestId) studioFinishRequest(state, message);
    studioStatus(state, state.fromOriginal ? '元画像から編集しています。適用で加工画像を保存します。' : '写真調整とフィルターを組み合わせ、適用で保存できます。');
    studioSetBusy(state);
  } else if (message.type === 'fg-studio-state') {
    if (String(message.sourceRevision ?? '0') !== state.sourceRevision) return;
    if (state.graph && JSON.stringify(state.graph) !== JSON.stringify(message.graph) &&
        !state.restoring && !state.updating && message.reason !== 'refresh' && message.reason !== 'restore') {
      if (message.editId === undefined || message.editId !== state.lastEditId) studioRemember(state);
      state.lastEditId = message.editId;
    }
    state.graph = message.graph; state.time = message.time || 0;
    studioHistory(state);
  } else if (message.type === 'fg-studio-preview') {
    if (String(message.sourceRevision ?? '0') !== state.sourceRevision || state.updating ||
        state.context !== lbContext || items[lbIdx]?.sha1 !== state.selectedSha ||
        !(message.image instanceof Blob) || message.revision <= state.previewRevision) return;
    state.previewRevision = message.revision;
    state.lastPreview = message.image;
    if (!state.comparing) studioPaint(state, message.image);
  } else if (message.type === 'fg-studio-height') {
    if (Number.isFinite(message.height)) state.frame.style.height = Math.max(240, Math.min(800, message.height)) + 'px';
  } else if (message.type === 'fg-studio-exported' || message.type === 'fg-studio-command-done') {
    studioFinishRequest(state, message);
  } else if (message.type === 'fg-studio-error') {
    const error = new Error(message.error || message.message || 'フィルターを処理できませんでした');
    if (message.requestId) studioFinishRequest(state, message, error);
    else if (!state.ready) state.rejectReady(error);
    studioStatus(state, error.message);
  }
});
async function studioCommand(action, text) {
  const state = await studioOpen();
  if (!state || studioIsBusy()) return false;
  studioRemember(state);
  state.restoring = true; state.busy++; studioSetBusy(state);
  try {
    await studioRequest(state, 'fg-studio-command', {action, text});
    return true;
  } catch (error) { studioStatus(state, error.message); return false; }
  finally { state.restoring = false; state.busy--; studioSetBusy(state); }
}
function studioGenerate() {
  const text = $('studio-text').value.trim();
  if (!text) { $('studio-text').focus(); return; }
  return studioCommand('generate', text);
}
async function studioRefresh(state) {
  const previousRevision = state.sourceRevision;
  let sent = false;
  state.updating = true; state.sourceRevision = String(+state.sourceRevision + 1);
  studioSetBusy(state);
  try {
    const image = await studioImage(state);
    if (studioSession !== state) return;
    const bitmap = await createImageBitmap(image);
    state.width = bitmap.width; state.height = bitmap.height; bitmap.close();
    // Allow only frames produced with this newly committed input revision.
    state.updating = false;
    sent = true;
    await studioRequest(state, 'fg-studio-refresh', {image, sourceRevision: state.sourceRevision});
  } catch (error) {
    if (!sent) state.sourceRevision = previousRevision;
    throw error;
  } finally { state.updating = false; studioSetBusy(state); }
}
async function studioPhotoEdit(body) {
  const state = studioSession;
  if (!state) return false;
  try { await state.initialized; } catch (_) { return false; }
  if (studioSession !== state || studioIsBusy()) return false;
  if (body.action === 'push' && body.edit?.op === 'auto' &&
      state.photoEdits.at(-1)?.op === 'auto' && state.photoEdits.at(-1)?.params?.version === 3) return true;
  const before = studioSnapshot(state);
  studioRemember(state); state.busy++; studioSetBusy(state);
  try {
    const edits = state.photoEdits;
    if (body.action === 'push') {
      const edit = structuredClone(body.edit);
      if (edit.op === 'auto') {
        while (edits.at(-1)?.op === 'auto') edits.pop();
        edit.params = {version: 3};
      }
      edits.push(edit);
    } else if (body.action === 'clear') state.photoEdits = [];
    edReset();
    await studioRefresh(state);
    studioHistory(state);
    studioStatus(state, '調整を反映しました。適用で加工画像を保存します。');
    return true;
  } catch (error) {
    state.photoEdits = before.photoEdits; state.history.pop();
    studioStatus(state, error.message); return false;
  } finally { state.busy--; studioSetBusy(state); }
}
async function studioRestoreSnapshot(state, snapshot) {
  state.restoring = true;
  try {
    state.source = snapshot.source || state.source;
    state.photoEdits = structuredClone(snapshot.photoEdits);
    edReset();
    await studioRefresh(state);
    await studioRequest(state, 'fg-studio-command', {action: 'restore', graph: snapshot.graph, time: snapshot.time});
    state.graph = snapshot.graph; state.time = snapshot.time;
    studioHistory(state);
  } finally { state.restoring = false; }
}
async function studioUndo() {
  const state = studioSession;
  if (!state?.ready || studioIsBusy() || !state.history.length) return false;
  const snapshot = state.history.pop(); state.busy++; studioSetBusy(state);
  try { await studioRestoreSnapshot(state, snapshot); return true; }
  catch (error) { state.history.push(snapshot); studioStatus(state, error.message); return false; }
  finally { state.busy--; studioSetBusy(state); }
}
async function studioResetAll() {
  const state = studioSession;
  if (!state?.ready || studioIsBusy()) return false;
  studioRemember(state); state.busy++; state.restoring = true; studioSetBusy(state);
  try {
    if (state.source.studio || state.source.filter_source_sha) {
      state.source = (await edOriginal(state.source.sha1)).meta;
    }
    state.photoEdits = []; edReset();
    await studioRefresh(state);
    await studioRequest(state, 'fg-studio-command', {action: 'reset'});
    studioStatus(state, '原本の見た目に戻しました。適用で保存します。');
    return true;
  } catch (error) { studioStatus(state, error.message); return false; }
  finally { state.restoring = false; state.busy--; studioSetBusy(state); }
}
async function studioSave() {
  const state = studioSession;
  if (!state?.ready || studioIsBusy()) return false;
  const vals = edVals();
  if (Object.keys(vals).length && !await studioPhotoEdit({action: 'push', edit: {op: 'adjust', params: vals}})) return false;
  if (studioSession !== state) return false;
  state.saving = true; studioSetBusy(state);
  studioStatus(state, '写真調整とフィルターを原寸で保存しています…');
  try {
    const result = await studioRequest(state, 'fg-studio-export');
    if (studioSession !== state) return false;
    if (!(result.image instanceof Blob) || result.image.type !== 'image/png') throw new Error('画像を書き出せませんでした');
    const form = new FormData(); form.append('image', result.image, 'filtered.png');
    form.append('recipe', JSON.stringify({...result.recipe, photo_edits: state.photoEdits, source_edits_rev: state.source.edits_rev}));
    const response = await fetch(`/api/studio/${state.source.sha1}/save`, {method: 'POST', body: form});
    const saved = await response.json();
    if (!response.ok) throw new Error(saved.detail || '画像を保存できませんでした');
    const active = studioSession === state;
    const context = lbContext, view = rgen;
    if (active) studioClose(false);
    await refreshFacets();
    if (!active) { toast('加工画像を保存しました'); return true; }
    if (context !== lbContext || view !== rgen) { toast('加工画像を保存しました'); return true; }
    await reload(true);
    if (context !== lbContext || rgen !== view + 1) { toast('加工画像を保存しました'); return true; }
    let index = items.findIndex(value => value.sha1 === saved.sha1);
    if (index < 0) {
      await go({type: 'source', key: saved.meta.source, criteria: {source: saved.meta.source}});
      if (context !== lbContext) return true;
      index = items.findIndex(value => value.sha1 === saved.sha1);
    }
    if (index >= 0) await lbShow(index, 0);
    toast('写真調整とフィルターを保存しました（原本は保持）');
    return true;
  } catch (error) { studioStatus(state, error.message); return false; }
  finally { state.saving = false; studioSetBusy(state); }
}
function studioClose(restore = true) {
  const state = studioSession;
  if (!state) return;
  studioSession = null;
  clearTimeout(state.loadTimeout); state.controller.abort();
  state.rejectReady(new Error('フィルター編集を閉じました'));
  for (const request of state.requests.values()) { clearTimeout(request.timer); request.reject(new Error('フィルター編集を閉じました')); }
  state.requests.clear(); state.frame?.remove();
  $('studio-inline').hidden = true; $('lb').classList.remove('scene-editing');
  for (const id of ['studio-generate', 'studio-random', 'studio-reset', 'lbstudiobtn']) $(id).disabled = false;
  edReset(); edSyncStatus();
  const urls = state.previewUrls;
  if (restore && state.context === lbContext && items[lbIdx]?.sha1 === state.selectedSha) {
    lbShow(lbIdx, 0).finally(() => { for (const url of urls || []) URL.revokeObjectURL(url); });
  } else { for (const url of urls || []) URL.revokeObjectURL(url); }
}

// Gallery owns the selected image and final file. The embedded editor is the
// actual fluent_scene Studio; its WASM and render loop exist only while open.
let studioSession = null;

function studioDialog() {
  let dialog = document.getElementById('studio-dialog');
  if (dialog) return dialog;
  dialog = document.createElement('dialog');
  dialog.id = 'studio-dialog';
  dialog.setAttribute('aria-label', 'fluent_scene フィルターエディター');
  dialog.innerHTML = `<div id="studio-top"><span id="studio-title">fluent_scene · フィルターエディター</span>
    <span id="studio-status" role="status" aria-live="polite"></span>
    <button id="studio-save" disabled>保存して戻る</button><button id="studio-close">閉じる</button></div>
    <div id="studio-frame-wrap"><div id="studio-loading">画像を読み込んでいます…</div></div>`;
  document.body.append(dialog);
  $('studio-save').addEventListener('click', studioSave);
  $('studio-close').addEventListener('click', studioClose);
  dialog.addEventListener('cancel', event => { event.preventDefault(); studioClose(); });
  // Parent shortcuts must not move/delete the image underneath the editor.
  document.addEventListener('keydown', event => {
    if (!dialog.open) return;
    if (event.key === 'Escape') { event.preventDefault(); studioClose(); }
    event.stopImmediatePropagation();
  }, true);
  return dialog;
}

function studioStatus(state, text) {
  if (studioSession !== state) return;
  $('studio-status').textContent = text;
}

async function studioImage(meta, signal) {
  const edited = !!meta.edits?.length;
  const url = edited ? `/render/${meta.sha1}?v=${encodeURIComponent(meta.edits_rev)}` : '/img/' + meta.sha1;
  const response = await fetch(url, {signal});
  if (!response.ok) throw new Error(response.status === 409 ? '画像の編集内容が変わりました。開き直してください' : '画像を読み込めませんでした');
  return response.blob();
}

async function studioOpen() {
  if (studioSession) return;
  const item = items[lbIdx];
  if (!item) return;
  const dialog = studioDialog();
  const state = {session: crypto.randomUUID(), selectedSha: item.sha1, context: lbContext,
    controller: new AbortController(), ready: false, saving: false, frame: null, exportWait: null,
    focus: document.activeElement};
  studioSession = state;
  $('studio-loading').hidden = false;
  $('studio-loading').textContent = '画像を読み込んでいます…';
  $('studio-save').disabled = true;
  dialog.showModal();
  studioStatus(state, '画像を準備しています…');
  try {
    await edQueues.get(item.sha1);
    if (studioSession !== state) return;
    const options = {signal: state.controller.signal};
    let meta = await j('/api/meta/' + item.sha1, options);
    let recipe = null;
    // A saved Studio image reopens its graph on the same underlying original,
    // so pressing dice/reset does not add the old baked effect a second time.
    if (meta.studio?.recipe?.graph && !meta.edits?.length) {
      const saved = meta.studio;
      try {
        const source = await j('/api/meta/' + saved.source_sha, options);
        if (source.edits_rev === saved.source_edits_rev) { recipe = saved.recipe; meta = source; }
      } catch (error) { if (error.name === 'AbortError') throw error; }
    }
    // A folder export has already baked its effects into the file. Start a new
    // editable graph from the retained original so Studio Reset can remove them.
    if (!recipe && (meta.filter_source_sha || meta.studio?.source_sha)) {
      const original = await j('/api/original/' + meta.sha1, options);
      meta = original.meta;
      state.fromOriginal = true;
    }
    state.source = meta;
    state.image = await studioImage(meta, state.controller.signal);
    const bitmap = await createImageBitmap(state.image);
    state.width = bitmap.width; state.height = bitmap.height; bitmap.close();
    if (studioSession !== state) return;
    state.recipe = recipe;
    const frame = document.createElement('iframe');
    frame.id = 'studio-frame';
    frame.title = 'fluent_scene Filter Studio';
    frame.allow = 'clipboard-read; clipboard-write; fullscreen';
    frame.src = '/fluent-scene/edit.html?' + new URLSearchParams({gallery: '1',
      w: state.width, h: state.height, session: state.session});
    state.frame = frame;
    $('studio-frame-wrap').append(frame);
    studioStatus(state, 'フィルターエディターを開いています…');
    state.loadTimeout = setTimeout(() => {
      if (studioSession !== state || state.ready) return;
      $('studio-loading').textContent = 'エディターの読み込みに時間がかかっています。閉じて開き直してください。';
      studioStatus(state, 'エディターを読み込めませんでした');
    }, 60000);
  } catch (error) {
    if (studioSession !== state || error.name === 'AbortError') return;
    studioStatus(state, error.message);
    $('studio-loading').textContent = error.message;
  }
}

window.addEventListener('message', event => {
  const state = studioSession, message = event.data;
  if (!state || event.source !== state.frame?.contentWindow || event.origin !== location.origin ||
      !message || message.session !== state.session) return;
  if (message.type === 'fg-studio-ready') {
    state.frame.contentWindow.postMessage({type: 'fg-studio-init', session: state.session,
      image: state.image, filename: state.source.sha1 + '.png', recipe: state.recipe,
      graph: state.recipe?.graph, time: state.recipe?.time}, location.origin);
  } else if (message.type === 'fg-studio-loaded') {
    clearTimeout(state.loadTimeout);
    state.ready = true;
    $('studio-loading').hidden = true;
    $('studio-save').disabled = false;
    studioStatus(state, `${state.width}×${state.height} · ${state.fromOriginal ? '元画像から編集しています。' : ''}保存すると加工画像が追加されます。原本は保持します。`);
  } else if (message.type === 'fg-studio-exported' && state.exportWait?.id === message.requestId) {
    state.exportWait.resolve(message); state.exportWait = null;
  } else if (message.type === 'fg-studio-error') {
    if (state.exportWait && (!message.requestId || state.exportWait.id === message.requestId)) {
      state.exportWait.reject(new Error(message.error || message.message || '画像を書き出せませんでした'));
      state.exportWait = null;
    } else {
      studioStatus(state, message.error || message.message || 'エディターでエラーが発生しました');
      if (!state.ready) $('studio-loading').textContent = $('studio-status').textContent;
    }
  }
});

async function studioSave() {
  const state = studioSession;
  if (!state?.ready || state.saving) return;
  state.saving = true;
  $('studio-save').disabled = true;
  studioStatus(state, '元の解像度で画像を仕上げています…');
  let timer;
  try {
    const requestId = crypto.randomUUID();
    const exported = new Promise((resolve, reject) => {
      state.exportWait = {id: requestId, resolve, reject};
      timer = setTimeout(() => reject(new Error('書き出しがタイムアウトしました。もう一度お試しください')), 180000);
    });
    state.frame.contentWindow.postMessage({type: 'fg-studio-export', session: state.session, requestId}, location.origin);
    const result = await exported;
    clearTimeout(timer);
    if (studioSession !== state) return;
    if (!(result.image instanceof Blob) || result.image.type !== 'image/png') throw new Error('画像を書き出せませんでした');
    const form = new FormData();
    form.append('image', result.image, 'fluent_scene.png');
    form.append('recipe', JSON.stringify({...result.recipe, source_edits_rev: state.source.edits_rev}));
    studioStatus(state, '加工画像をギャラリーに保存しています…');
    const response = await fetch(`/api/studio/${state.source.sha1}/save`, {method: 'POST', body: form});
    const saved = await response.json();
    if (!response.ok) throw new Error(saved.detail || '画像を保存できませんでした');
    const active = studioSession === state;
    if (active) studioClose();
    await refreshFacets();
    if (!active) { toast('加工画像を保存しました'); return; }
    await reload(true);
    let index = items.findIndex(value => value.sha1 === saved.sha1);
    if (index < 0) {
      await go({type: 'source', key: saved.meta.source, criteria: {source: saved.meta.source}});
      index = items.findIndex(value => value.sha1 === saved.sha1);
    }
    if (index >= 0) {
      if ($('lb').classList.contains('show')) await lbShow(index, 0); else openLb(index);
    }
    toast(saved.reused ? '保存済みの加工画像を開きました' : '加工画像を保存しました（原本は保持）');
  } catch (error) {
    if (studioSession === state) studioStatus(state, error.message);
  } finally {
    clearTimeout(timer);
    state.exportWait = null; state.saving = false;
    if (studioSession === state) $('studio-save').disabled = !state.ready;
  }
}

function studioClose() {
  const state = studioSession;
  if (!state) return;
  studioSession = null;
  clearTimeout(state.loadTimeout);
  state.controller.abort();
  state.exportWait?.reject(new Error('エディターを閉じました'));
  state.frame?.remove(); // Stop the Studio's WASM, animation, workers and media.
  const dialog = $('studio-dialog');
  if (dialog.open) dialog.close();
  if (state.focus?.isConnected) state.focus.focus();
}

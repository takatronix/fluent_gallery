// fluent_gallery thumbnail resize performance probe (read-only).
// Usage: node tests/thumbnail_perf.js
// Env: FG_URL, CHROME, PERF_WIDTH, PERF_HEIGHT, PERF_DPR, PERF_SCRUB_MS.
const puppeteer = require('puppeteer-core');

const BASE = process.env.FG_URL || 'http://127.0.0.1:8790';
const WIDTH = +(process.env.PERF_WIDTH || 1920);
const HEIGHT = +(process.env.PERF_HEIGHT || 1080);
const DPR = +(process.env.PERF_DPR || 1);
const SCRUB_MS = +(process.env.PERF_SCRUB_MS || 2000);
const ITEM_COUNT = +(process.env.PERF_ITEMS || 2400);
const SCROLL_ITEMS = +(process.env.PERF_SCROLL_ITEMS || 10000);

const percentile = (values, p) => {
  if (!values.length) return 0;
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.min(sorted.length - 1, Math.ceil(sorted.length * p) - 1)];
};

const metricMap = metrics => Object.fromEntries(metrics.map(({name, value}) => [name, value]));

async function measure(page, cdp, from, to) {
  await page.evaluate(async size => {
    const slider = document.getElementById('cellsize');
    slider.value = size;
    setCell(size);
    commitCell();
    await new Promise(resolve => setTimeout(resolve, 800));
  }, from);

  await page.evaluate(() => {
    window.__thumbPerf = {frames: [], longTasks: [], maxCells: 0, started: performance.now()};
    const state = window.__thumbPerf;
    state.longObserver = new PerformanceObserver(list => {
      for (const entry of list.getEntries()) state.longTasks.push(entry.duration);
    });
    state.longObserver.observe({type: 'longtask'});
    let prior = performance.now();
    const frame = now => {
      state.frames.push(now - prior);
      prior = now;
      state.maxCells = Math.max(state.maxCells, document.querySelectorAll('.cell').length);
      if (!state.done) requestAnimationFrame(frame);
    };
    requestAnimationFrame(frame);
  });

  const before = metricMap((await cdp.send('Performance.getMetrics')).metrics);
  await page.evaluate(async ({from, to, duration}) => {
    const slider = document.getElementById('cellsize');
    const start = performance.now();
    await new Promise(resolve => {
      const tick = now => {
        const ratio = Math.min(1, (now - start) / duration);
        const value = Math.round((from + (to - from) * ratio) / 4) * 4;
        slider.value = value;
        slider.dispatchEvent(new Event('input', {bubbles: true}));
        if (ratio < 1) requestAnimationFrame(tick);
        else {
          slider.value = to;
          slider.dispatchEvent(new Event('input', {bubbles: true}));
          slider.dispatchEvent(new Event('change', {bubbles: true}));
          resolve();
        }
      };
      requestAnimationFrame(tick);
    });
    await new Promise(resolve => setTimeout(resolve, 2200));
    window.__thumbPerf.done = true;
    window.__thumbPerf.longObserver.disconnect();
  }, {from, to, duration: SCRUB_MS});
  const after = metricMap((await cdp.send('Performance.getMetrics')).metrics);

  const ui = await page.evaluate(() => {
    const perf = window.__thumbPerf;
    const wrap = document.getElementById('gridwrap').getBoundingClientRect();
    const visible = [...document.querySelectorAll('.cell img')].filter(img => {
      const r = img.getBoundingClientRect();
      return r.bottom > wrap.top && r.top < wrap.bottom && r.right > wrap.left && r.left < wrap.right;
    });
    const tiers = {};
    let naturalMax = 0;
    for (const img of visible) {
      const tier = img.dataset.tier || img.src.match(/\/(micro|thumb|preview|atlas)\//)?.[1] || 'other';
      tiers[tier] = (tiers[tier] || 0) + 1;
      naturalMax = Math.max(naturalMax, img.naturalWidth || 0, img.naturalHeight || 0);
    }
    return {
      frames: perf.frames,
      longTasks: perf.longTasks,
      maxCells: perf.maxCells,
      cells: document.querySelectorAll('.cell').length,
      liteCells: document.querySelectorAll('.cell.lite').length,
      attrClassCells: document.querySelectorAll('.cell:is(.m1,.m2,.m3,.m4,.m5,.m6,.m7)').length,
      attributeElements: document.querySelectorAll('.cell .costb,.cell .rclean,.cell .safeb,.cell .attrs').length,
      overlayElements: document.querySelectorAll('.cell .cap,.cell .costb,.cell .rclean,.cell .safeb,.cell .attrs,.cell .newb,.cell .ck,.cell .badge').length,
      textCells: [...document.querySelectorAll('.cell')].filter(cell => cell.textContent.trim()).length,
      attributeMarks: [...document.querySelectorAll('.cell')]
        .filter(cell => /[✓$○]/.test(getComputedStyle(cell, '::after').content)).length,
      elements: document.getElementsByTagName('*').length,
      items: typeof items === 'undefined' ? 0 : items.length,
      visible: visible.length,
      uniqueSources: new Set(visible.map(img => img.src)).size,
      tiers,
      naturalMax,
      heap: performance.memory?.usedJSHeapSize || 0,
    };
  });

  const {frames: rawFrames, longTasks, heap, ...uiSummary} = ui;
  const frames = rawFrames.filter(value => value >= 0 && value < 1000);
  const delta = name => (after[name] || 0) - (before[name] || 0);
  return {
    transition: `${from}->${to}`,
    scrubMs: SCRUB_MS,
    frames: frames.length,
    frameP50Ms: +percentile(frames, 0.50).toFixed(1),
    frameP95Ms: +percentile(frames, 0.95).toFixed(1),
    frameMaxMs: +Math.max(0, ...frames).toFixed(1),
    longTaskCount: longTasks.filter(value => value > 50).length,
    longTaskTotalMs: +longTasks.reduce((sum, value) => sum + value, 0).toFixed(1),
    longTaskMaxMs: +Math.max(0, ...longTasks).toFixed(1),
    taskMs: +(delta('TaskDuration') * 1000).toFixed(1),
    layoutMs: +(delta('LayoutDuration') * 1000).toFixed(1),
    styleMs: +(delta('RecalcStyleDuration') * 1000).toFixed(1),
    scriptMs: +(delta('ScriptDuration') * 1000).toFixed(1),
    ...uiSummary,
    heapMiB: +(heap / 1048576).toFixed(1),
  };
}

async function measureScroll(page, cdp) {
  await page.evaluate(async target => {
    const slider = document.getElementById('cellsize');
    slider.value = 36;
    setCell(36);
    commitCell();
    while (items.length < Math.min(target, total)) {
      const before = items.length;
      await loadMore();
      if (items.length === before) break;
    }
    await new Promise(resolve => setTimeout(resolve, 800));
  }, SCROLL_ITEMS);

  await page.evaluate(() => {
    window.__thumbPerf = {frames: [], activeFrames: [], settleFrames: [], activeMissing: [],
      activeVisible: [], missingDetails: [], phase: 'setup', longTasks: [], maxCells: 0};
    const state = window.__thumbPerf;
    state.longObserver = new PerformanceObserver(list => {
      for (const entry of list.getEntries()) state.longTasks.push({duration: entry.duration,
        startTime: entry.startTime});
    });
    state.longObserver.observe({type: 'longtask'});
    let prior = performance.now();
    const frame = now => {
      const delta = now - prior;
      state.frames.push(delta);
      if (state.phase === 'active') state.activeFrames.push(delta);
      else if (state.phase === 'settle') state.settleFrames.push(delta);
      prior = now;
      state.maxCells = Math.max(state.maxCells, document.querySelectorAll('.cell').length);
      // geometry readをせず毎frame論理可視範囲だけ検査。1frameだけの空セルも最終静止後に隠さない。
      if (state.phase === 'active') {
        const wrap = document.getElementById('gridwrap'), cells = document.getElementById('grid').children;
        const n = setCell._n || gridCols(), ch = (gridWidth() + gridGap()) / n;
        const row = Math.floor(Math.max(0, wrap.scrollTop - contentTop()) / ch);
        const a = Math.max(0, row * n - vStart);
        const b = Math.max(a, Math.min(cells.length,
          (row + Math.max(1, Math.ceil(wrap.clientHeight / ch))) * n - vStart));
        let missing = 0, missingUrls = new Set();
        for (let i = a; i < b; i++) {
          const im = cells[i]?.firstElementChild;
          if (!im || !im.complete || !im.naturalWidth || im.style.visibility === 'hidden') {
            missing++;
            if (im?.src && missingUrls.size < 8) missingUrls.add(im.src.split('/').pop());
          }
        }
        state.activeMissing.push(missing);
        state.activeVisible.push(b - a);
        if (missing && state.missingDetails.length < 20) state.missingDetails.push({row, missing,
          urls: [...missingUrls], warming: atlasWarming?.size || 0});
      }
      if (!state.done) requestAnimationFrame(frame);
    };
    requestAnimationFrame(frame);
  });
  const before = metricMap((await cdp.send('Performance.getMetrics')).metrics);

  await page.evaluate(async duration => {
    const wrap = document.getElementById('gridwrap');
    const savedTotal = total;
    total = items.length; // bottom到達時に性能計測外の追加APIを発火させない
    updateWindow(true, true); // spacerも読込済みitemsの高さに揃えて、実距離を均等に往復する
    await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    const animate = (from, to) => new Promise(resolve => {
      const start = performance.now();
      const tick = now => {
        const ratio = Math.min(1, (now - start) / duration);
        wrap.scrollTop = from + (to - from) * ratio;
        if (ratio < 1) requestAnimationFrame(tick); else resolve();
      };
      requestAnimationFrame(tick);
    });
    const bottom = wrap.scrollHeight - wrap.clientHeight;
    window.__thumbPerf.activeStart = performance.now();
    window.__thumbPerf.phase = 'active';
    await animate(0, bottom);
    await animate(bottom, 0);
    window.__thumbPerf.settleStart = performance.now();
    window.__thumbPerf.phase = 'settle';
    await new Promise(resolve => setTimeout(resolve, 1000));
    total = savedTotal;
    updateWindow(true, true);
    window.__thumbPerf.done = true;
    window.__thumbPerf.longObserver.disconnect();
  }, SCRUB_MS);
  const after = metricMap((await cdp.send('Performance.getMetrics')).metrics);

  const ui = await page.evaluate(() => {
    const perf = window.__thumbPerf;
    const cells = [...document.querySelectorAll('.cell')];
    const ids = cells.map(cell => cell.id);
    const wrap = document.getElementById('gridwrap');
    const n = gridCols(), ch = cellPitch();
    const row = Math.floor(Math.max(0, wrap.scrollTop - contentTop()) / ch);
    const a = Math.max(0, row * n - vStart);
    const b = Math.max(a, Math.min(cells.length,
      (row + Math.max(1, Math.ceil(wrap.clientHeight / ch))) * n - vStart));
    const visibleImages = cells.slice(a, b).map(cell => cell.firstElementChild)
      .filter(img => img?.tagName === 'IMG');
    return {
      frames: perf.frames,
      activeFrames: perf.activeFrames,
      settleFrames: perf.settleFrames,
      activeMissing: perf.activeMissing,
      activeVisible: perf.activeVisible,
      missingDetails: perf.missingDetails,
      longTasks: perf.longTasks,
      activeStart: perf.activeStart,
      settleStart: perf.settleStart,
      maxCells: perf.maxCells,
      cells: cells.length,
      elements: document.getElementsByTagName('*').length,
      items: items.length,
      uniqueIds: new Set(ids).size,
      windowCount: vEnd - vStart,
      atTop: wrap.scrollTop === 0,
      visibleImages: visibleImages.length,
      visibleLoaded: visibleImages.filter(img => img.complete && img.naturalWidth > 0).length,
      visibleDeferred: visibleImages.filter(img => img.dataset.src).length,
      heap: performance.memory?.usedJSHeapSize || 0,
    };
  });
  const {frames: rawFrames, activeFrames: rawActiveFrames, settleFrames: rawSettleFrames,
    activeMissing, activeVisible,
    longTasks: longTaskEntries, activeStart, settleStart, heap, ...uiSummary} = ui;
  const longTasks = longTaskEntries.map(entry => typeof entry === 'number' ? entry : entry.duration);
  const frames = rawFrames.filter(value => value >= 0 && value < 1000);
  const activeFrames = rawActiveFrames.filter(value => value >= 0 && value < 1000);
  const settleFrames = rawSettleFrames.filter(value => value >= 0 && value < 1000);
  const delta = name => (after[name] || 0) - (before[name] || 0);
  return {
    transition: `scroll-${ui.items}`,
    scrubMs: SCRUB_MS * 2,
    frames: frames.length,
    frameP50Ms: +percentile(frames, 0.50).toFixed(1),
    frameP95Ms: +percentile(frames, 0.95).toFixed(1),
    frameMaxMs: +Math.max(0, ...frames).toFixed(1),
    activeFrameP95Ms: +percentile(activeFrames, 0.95).toFixed(1),
    settleFrameP95Ms: +percentile(settleFrames, 0.95).toFixed(1),
    activeMissingP95: percentile(activeMissing, 0.95),
    activeMissingMax: Math.max(0, ...activeMissing),
    activeVisibleP50: percentile(activeVisible, 0.50),
    longTaskCount: longTasks.filter(value => value > 50).length,
    longTaskTotalMs: +longTasks.reduce((sum, value) => sum + value, 0).toFixed(1),
    longTaskMaxMs: +Math.max(0, ...longTasks).toFixed(1),
    longTaskPhases: longTaskEntries.map(entry => typeof entry === 'number' ? 'unknown' :
      entry.startTime < activeStart ? 'setup' : entry.startTime < settleStart ? 'active' : 'settle'),
    longTaskStartsMs: longTaskEntries.map(entry => typeof entry === 'number' ? 0 : +entry.startTime.toFixed(1)),
    taskMs: +(delta('TaskDuration') * 1000).toFixed(1),
    layoutMs: +(delta('LayoutDuration') * 1000).toFixed(1),
    styleMs: +(delta('RecalcStyleDuration') * 1000).toFixed(1),
    scriptMs: +(delta('ScriptDuration') * 1000).toFixed(1),
    ...uiSummary,
    heapMiB: +(heap / 1048576).toFixed(1),
  };
}

(async () => {
  const browser = await puppeteer.launch({
    executablePath: process.env.CHROME || '/usr/bin/google-chrome',
    headless: 'new',
    args: ['--no-sandbox', '--disable-background-timer-throttling'],
    defaultViewport: {width: WIDTH, height: HEIGHT, deviceScaleFactor: DPR},
  });
  try {
    const page = await browser.newPage();
    await page.evaluateOnNewDocument(() => {
      localStorage.setItem('fg_cell', '172');
      localStorage.setItem('fg_vp', '1');
    });
    await page.goto(BASE + '/', {waitUntil: 'networkidle2', timeout: 60000});
    await page.waitForFunction(() => typeof items !== 'undefined' && items.length >= 200 && document.querySelector('.cell'), {timeout: 30000});
    await page.evaluate(async target => {
      while (items.length < Math.min(target, total)) {
        const offset = loadMore._offset;
        await loadMore();
        if (loadMore._exhausted || loadMore._offset === offset) break;
      }
      await new Promise(resolve => setTimeout(resolve, 500));
    }, ITEM_COUNT);
    const cdp = await page.createCDPSession();
    await cdp.send('Performance.enable');
    const results = [];
    results.push(await measure(page, cdp, 172, 92));
    results.push(await measure(page, cdp, 172, 36));
    results.push(await measureScroll(page, cdp));
    console.log(JSON.stringify({viewport: `${WIDTH}x${HEIGHT}@${DPR}`, results}, null, 2));
    const failures = [];
    for (const result of results.slice(0, 2)) {
      if (result.longTaskCount) failures.push(`${result.transition}: Long Task ${result.longTaskCount}`);
      if (result.frameP95Ms > 20) failures.push(`${result.transition}: frame p95 ${result.frameP95Ms}ms`);
      if (result.transition.endsWith('92') && (result.tiers?.micro !== result.visible || result.naturalMax > 120))
        failures.push(`${result.transition}: micro tier incomplete`);
      if (result.transition.endsWith('36') && (result.tiers?.atlas !== result.visible ||
          result.uniqueSources >= result.visible / 10 || result.naturalMax <= 120))
        failures.push(`${result.transition}: atlas tier incomplete`);
      if (result.liteCells !== result.cells || result.attrClassCells || result.attributeElements ||
          result.overlayElements || result.textCells || result.attributeMarks) {
        failures.push(`${result.transition}: compact cells are not image-only`);
      }
    }
    const scroll = results[2];
    if (scroll.longTaskCount) failures.push(`scroll: Long Task ${scroll.longTaskCount}`);
    if (scroll.activeFrameP95Ms > 20) failures.push(`scroll: active frame p95 ${scroll.activeFrameP95Ms}ms`);
    if (scroll.settleFrameP95Ms > 50) failures.push(`scroll: settle frame p95 ${scroll.settleFrameP95Ms}ms`);
    if (scroll.activeMissingP95) failures.push(`scroll: blank images p95 ${scroll.activeMissingP95}/${scroll.activeVisibleP50}`);
    if (scroll.maxCells > 2000) failures.push(`scroll: DOM cells ${scroll.maxCells}`);
    if (scroll.uniqueIds !== scroll.cells || scroll.windowCount !== scroll.cells) failures.push('scroll: duplicate/missing cells');
    if (scroll.visibleLoaded < scroll.visibleImages * 0.95 || scroll.visibleDeferred) {
      failures.push(`scroll: visible image recovery ${scroll.visibleLoaded}/${scroll.visibleImages}`);
    }
    if (failures.length) {
      console.error(`FAIL: ${failures.join('; ')}`);
      process.exitCode = 1;
    } else console.log('PASS: thumbnail performance budget');
  } finally {
    await browser.close();
  }
})().catch(error => {
  console.error(error.stack || error.message);
  process.exit(1);
});

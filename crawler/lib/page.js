'use strict';
// ページを「人と同じ目線」で扱う部品: 同意バナーを閉じる → 上から少しずつスクロール → 目録(画像・リンク・本文)を読む
const { sleep, jitter } = require('./util');

/** ブラウザ内で実行される目録スクリプト。DOM を人が見るように要約する(画像は表示寸法/自然寸法/周辺文脈/部品かどうか) */
function inventoryScript({ maxImages, maxLinks }) {
  const CHROME = 'nav, header, footer, aside, form, dialog, [role="navigation"], [role="banner"], [role="contentinfo"], [role="complementary"], [role="dialog"], [aria-hidden="true"], .sidebar, #sidebar, .navbar, .menu, .breadcrumb, .cookie, .consent, .share, .social';
  const MAIN = 'main, article, [role="main"], #content, #main, #mw-content-text, .content, .entry-content, .post-content, .article, .gallery, .photos, figure, .grid, .masonry, section';
  const txt = (el) => ((el && (el.innerText != null ? el.innerText : el.textContent)) || '').replace(/\s+/g, ' ').trim();
  const absu = (u) => { try { return new URL(u, location.href).href; } catch { return ''; } };
  const isImgUrl = (u) => /\.(jpe?g|png|webp|gif|avif|bmp|tiff?)(\?|#|$)/i.test(u) || /\/File:/.test(u) || /upload\.wikimedia\.org\/.*\/commons\/[0-9a-f]\/[0-9a-f]{2}\//.test(u);
  const docY = (el) => { const r = el.getBoundingClientRect(); return { x: r.left + scrollX, y: r.top + scrollY, w: r.width, h: r.height }; };
  const heads = [...document.querySelectorAll('h1,h2,h3')].map((h) => ({ y: docY(h).y, t: txt(h).slice(0, 120) })).filter((h) => h.t);
  const headAbove = (y) => { let best = ''; for (const h of heads) { if (h.y <= y + 4) best = h.t; else break; } return best; };
  const parseSrcset = (s) => {
    const out = [];
    for (const part of (s || '').split(',')) {
      const m = part.trim().match(/^(\S+)\s*(?:(\d+(?:\.\d+)?)([wx]))?$/);
      if (!m) continue;
      out.push({ url: absu(m[1]), w: m[3] === 'w' ? +m[2] : 0, x: m[3] === 'x' ? +m[2] : 0 });
    }
    return out.filter((c) => c.url);
  };
  const lazyAttrs = (el) => {
    const out = [];
    for (const a of el.attributes) {
      if (!/^data-/.test(a.name) && a.name !== 'data-src') continue;
      const v = a.value.trim();
      if (/^(https?:)?\/\//.test(v) || v.startsWith('/')) { if (isImgUrl(v) || /(src|image|img|full|large|orig|zoom|hi-?res|2x)/i.test(a.name)) out.push(absu(v)); }
      else if (/,/.test(v) && /\d+[wx]/.test(v)) for (const c of parseSrcset(v)) out.push(c.url);
    }
    return out;
  };
  const captionOf = (el) => {
    const fig = el.closest('figure');
    if (fig) { const fc = fig.querySelector('figcaption'); if (fc) return txt(fc).slice(0, 240); }
    const p = el.parentElement;
    if (p) {
      const c = p.querySelector('figcaption, .caption, .wp-caption-text, .gallerytext, .thumbcaption, [class*="caption"], [class*="Caption"]');
      if (c) return txt(c).slice(0, 240);
    }
    return (el.getAttribute('title') || '').trim().slice(0, 240);
  };
  const contextOf = (el) => {
    let n = el.parentElement, hops = 0;
    while (n && hops < 6) { const t = txt(n); if (t.length >= 40) return t.slice(0, 300); n = n.parentElement; hops++; }
    return '';
  };
  const linkOf = (el) => {
    const a = el.closest('a[href]');
    if (!a) return '';
    const h = absu(a.getAttribute('href'));
    return h && h !== location.href ? h : '';
  };

  const images = [];
  const seen = new Set();
  const pushImg = (el, src, extra) => {
    const r = docY(el);
    const item = Object.assign({
      src: absu(src), currentSrc: absu(el.currentSrc || src), srcset: [], lazy: lazyAttrs(el), link: linkOf(el),
      w: Math.round(r.w), h: Math.round(r.h), y: Math.round(r.y), nw: el.naturalWidth || 0, nh: el.naturalHeight || 0,
      alt: (el.getAttribute('alt') || '').trim().slice(0, 200), caption: captionOf(el), context: contextOf(el), head: headAbove(r.y),
      chrome: !!el.closest(CHROME), inMain: !!el.closest(MAIN), inFigure: !!el.closest('figure, .thumb, .gallerybox, [class*="photo"], [class*="image"]'),
      cls: ((el.className && el.className.baseVal !== undefined ? el.className.baseVal : el.className) || '') + ' ' + (el.id || ''),
      hidden: r.w === 0 && r.h === 0 && getComputedStyle(el).display === 'none',
    }, extra || {});
    const key = item.currentSrc || item.src || (item.lazy[0] || '');
    if (!key || seen.has(key)) return;
    seen.add(key);
    images.push(item);
  };
  for (const img of document.querySelectorAll('img')) {
    if (images.length >= maxImages) break;
    const pic = img.closest('picture');
    let srcset = parseSrcset(img.getAttribute('srcset') || img.getAttribute('data-srcset'));
    if (pic) for (const s of pic.querySelectorAll('source[srcset]')) srcset = srcset.concat(parseSrcset(s.getAttribute('srcset')));
    pushImg(img, img.getAttribute('src') || img.getAttribute('data-src') || '', { srcset, kind: 'img' });
  }
  // CSS 背景画像(hero/cover 等)。inline style だけ見る(全要素の computed style は重い)
  let n = 0;
  for (const el of document.querySelectorAll('[style*="background"]')) {
    if (images.length >= maxImages || n++ > 400) break;
    const m = (el.getAttribute('style') || '').match(/url\((['"]?)([^'")]+)\1\)/);
    if (!m) continue;
    const r = docY(el);
    if (r.w < 120 || r.h < 120) continue;
    pushImg(el, m[2], { kind: 'bg', nw: 0, nh: 0, alt: '', srcset: [] });
  }
  // meta og:image(記事の主画像。本文に無いことがある)
  const og = document.querySelector('meta[property="og:image"], meta[name="og:image"], meta[name="twitter:image"]');
  const ogImage = og ? absu(og.getAttribute('content') || '') : '';

  const links = [];
  const lseen = new Set();
  const NEXT_RE = /^(next|next page|older|older posts|more|load more|show more|see more|view more|次へ?|次のページ|次の\d+件|続きを?見る|もっと見る|さらに表示|›|»|>|→)$/i;
  const GENERIC = /^(home|top|about|about us|contact|privacy|privacy policy|terms|terms of (use|service)|login|log in|sign in|sign up|register|help|faq|sitemap|cookies?|settings|search|menu|skip to (main )?content|ホーム|トップ|会社概要|お問い合わせ|プライバシー(ポリシー)?|利用規約|ログイン|新規登録|ヘルプ|サイトマップ|検索|メニュー)$/i;
  const headLink = document.querySelector('link[rel="next"]');
  if (headLink) links.push({ href: absu(headLink.getAttribute('href')), text: 'next', title: '', context: '', chrome: false, inMain: true, wrapsImage: false, imgAlt: '', relNext: true, generic: false, y: 0 });
  for (const a of document.querySelectorAll('a[href]')) {
    if (links.length >= maxLinks) break;
    const href = absu(a.getAttribute('href'));
    if (!href || !/^https?:/.test(href) || lseen.has(href)) continue;
    lseen.add(href);
    const r = docY(a);
    const img = a.querySelector('img');
    const text = txt(a).slice(0, 120);
    links.push({
      href, text, title: (a.getAttribute('title') || '').slice(0, 120),
      context: txt(a.parentElement).slice(0, 200), chrome: !!a.closest(CHROME), inMain: !!a.closest(MAIN),
      wrapsImage: !!img, imgAlt: img ? (img.getAttribute('alt') || '').slice(0, 120) : '',
      relNext: /(^|\s)next(\s|$)/i.test(a.rel || '') || NEXT_RE.test(text) || NEXT_RE.test((a.getAttribute('aria-label') || '').trim()),
      generic: GENERIC.test(text), y: Math.round(r.y),
    });
  }
  const mainEl = document.querySelector('main, article, [role="main"], #content, #mw-content-text') || document.body;
  const desc = document.querySelector('meta[name="description"], meta[property="og:description"]');
  return {
    url: location.href, title: (document.title || '').trim().slice(0, 200), lang: document.documentElement.lang || '',
    desc: desc ? (desc.getAttribute('content') || '').slice(0, 300) : '',
    headings: heads.slice(0, 20).map((h) => h.t), text: txt(mainEl).slice(0, 3000), ogImage,
    images, links, docHeight: document.documentElement.scrollHeight,
  };
}

/** 同意バナー/ポップアップを一般的なボタン文言で閉じる(1 回だけ、見えている物だけ) */
async function dismissConsent(page) {
  const RE = '^(accept( all)?( cookies)?|agree|i agree|allow( all)?( cookies)?|got it|ok|okay|close|dismiss|continue|understood|同意(する|します)?|すべて(同意|許可)(する)?|閉じる|了解|確認|はい)$';
  try {
    const clicked = await page.evaluate((reSrc) => {
      const re = new RegExp(reSrc, 'i');
      for (const el of document.querySelectorAll('button, a[role="button"], [role="button"], input[type="button"], input[type="submit"]')) {
        const t = ((el.innerText != null ? el.innerText : el.value) || '').trim();
        if (!t || t.length > 40 || !re.test(t)) continue;
        const r = el.getBoundingClientRect();
        if (r.width > 0 && r.height > 0) { el.click(); return t; }
      }
      return null;
    }, RE);
    if (clicked) await sleep(600);
    return clicked;
  } catch { return null; }
}

/** 上から下へ人のペースでスクロール(ホイール事象=遅延読み込みが反応する)。底で少し待って無限スクロールも踏む */
async function humanScroll(page, { maxScreens = 12, pause = [250, 700] } = {}) {
  let screens = 0, stalls = 0, lastH = 0;
  while (screens < maxScreens) {
    let m;
    try { m = await page.evaluate(() => ({ y: scrollY, vh: innerHeight, h: document.documentElement.scrollHeight })); } catch { break; }
    if (m.y + m.vh >= m.h - 4) {
      if (m.h === lastH) { if (++stalls >= 2) break; } else stalls = 0;
      lastH = m.h;
      await sleep(900);
      let h2 = m.h;
      try { h2 = await page.evaluate(() => document.documentElement.scrollHeight); } catch { break; }
      if (h2 <= m.h) break;
      continue;
    }
    try { await page.mouse.wheel(0, Math.round(m.vh * (0.6 + Math.random() * 0.3))); } catch { break; }
    screens++;
    await sleep(jitter(pause));
  }
  try { await page.evaluate(() => scrollTo(0, 0)); } catch {}
  await sleep(250);
  return screens;
}

async function readPage(page, { maxImages = 400, maxLinks = 600 } = {}) {
  return page.evaluate(inventoryScript, { maxImages, maxLinks });
}

module.exports = { inventoryScript, dismissConsent, humanScroll, readPage };

'use strict';
// 「人ならどう見るか」の点数付け。画像=このページの内容の画像か / リンク=次に見に行く価値があるか。
// 判断(採用/不採用)は gallery 側の目利きがやるので、ここは「明らかな部品を落とし、見込みの順に並べる」役
const { clamp, textScore, normalizeUrl, sameSite, isMediaHost, isPrivateHost, NON_HTML_EXT, IMAGE_EXT } = require('./util');

const JUNK_WORD = /(^|[-_/. ])(icons?|logos?|avatars?|sprites?|badges?|buttons?|btn|emoji|smil(ey|ies)|spinner|loaders?|loading|pixel|tracking|tracker|beacon|share|social|arrow|bullet|stars?|rating|flags?|advert|adserver|ad-?banner|placeholder|blank|spacer|1x1|favicon|gravatar|profile-?pic|userpic|widget|captcha|qr-?code|barcode|powered|counter|separator|divider)([-_/. ]|$)/i;
const AD_HOST = /(doubleclick|googlesyndication|adservice|adsystem|adnxs|taboola|outbrain|criteo|scorecardresearch|amazon-adsystem|moatads|adsafeprotected|yieldmanager|popads|rubiconproject|openx|pubmatic|smartadserver|zedo|advertising\.com)/i;
const SKIP_LINK = /(\/(login|logout|signin|sign-in|signup|sign-up|register|account|my-?account|cart|checkout|password|auth|oauth|session|subscribe|unsubscribe|wp-admin|wp-login|admin)(\/|\?|$)|[?&](share|sharer|print|replytocom|redirect_to|returnurl|logout)=|\/feed\/?$|\.rss$|comment-page-\d+|action=(edit|history|raw|purge|info|submit)|(Special|Talk|User|Template|Help|MediaWiki|Wikipedia|Commons|Category_talk|File_talk|User_talk|Portal)(:|%3A)|oldid=|diff=|printable=yes|veaction=|redlink=1|mailto:|^tel:|^javascript:)/i;
const GALLERY_WORD = /(gallery|galleries|photos?|photograph|album|images?|pictures?|pics|portfolio|category|categories|collection|artworks?|works|wallpapers?|media|\/File:|figure|slideshow|lookbook|showcase|exhibit|画像|写真|ギャラリー|作品|アルバム|コレクション)/i;
const PAGINATION = /(\/page\/\d+|[?&](page|p|pg|pagenum|offset|start|from|cursor)=\w+|\/p\d+\/?$|\/\d+\/?$|pagefrom=|filefrom=)/i;

/** 画像 URL の「原寸っぽい」書き換え候補(有名 CDN の縮小規約)。無ければ空 */
function originalVariants(u) {
  const out = [];
  if (!u) return out;
  // Wikimedia: /commons/thumb/a/ab/X.jpg/220px-X.jpg → /commons/a/ab/X.jpg
  const m = u.match(/^(https?:\/\/upload\.wikimedia\.org\/.*?)\/thumb\/(.+)\/[^/]+$/);
  if (m) out.push(`${m[1]}/${m[2]}`);
  // WordPress: foo-1024x683.jpg → foo.jpg
  const wp = u.replace(/-\d{2,4}x\d{2,4}(\.(jpe?g|png|webp|gif))(\?.*)?$/i, '$1');
  if (wp !== u) out.push(wp);
  // 幅指定クエリ(w=, width=, resize=)を落とす
  try {
    const x = new URL(u);
    let changed = false;
    for (const k of [...x.searchParams.keys()]) if (/^(w|h|width|height|resize|fit|quality|q|s|size|crop|dpr|auto|format|fm)$/i.test(k)) { x.searchParams.delete(k); changed = true; }
    if (changed) out.push(x.toString());
  } catch {}
  return out.filter((v) => v !== u);
}

/** 目録の 1 画像を「部品」と見なす理由(null = 内容の画像かもしれない) */
function junkImage(it, opts) {
  const { minSide, seenPages } = opts;
  const src = it.currentSrc || it.src || '';
  if (it.hidden) return 'hidden';
  if (/^(data|blob):/.test(src) && !it.lazy.length && !it.srcset.length) return 'data';
  if (/\.(svg|ico)(\?|#|$)/i.test(src)) return 'icon';
  if (it.chrome) return 'nav';
  const words = `${it.cls} ${src.split('/').slice(3).join('/')} ${it.alt}`;
  if (JUNK_WORD.test(words)) return 'icon';
  if (/\/(wp-content\/(plugins|themes)|_next\/static|static\/(img|images|media)\/(ui|icons?|placeholder)|assets\/(ui|icons?|placeholders?))\//i.test(src)) return 'icon'; // テーマ/プラグインの素材=装飾
  try { if (AD_HOST.test(new URL(src).hostname)) return 'ad'; } catch {}
  if (seenPages && (seenPages.get(src) || 0) >= 3) return 'repeat'; // 3 ページ以上に同じ画像 = サイトの部品
  const rw = it.w, rh = it.h;
  const nw = it.nw || 0, nh = it.nh || 0;
  const bestW = Math.max(nw, ...it.srcset.map((c) => c.w || 0));
  const hasBigger = bestW >= minSide || it.srcset.some((c) => c.x >= 2) || it.lazy.length > 0 || (it.link && IMAGE_EXT.test(it.link)) || it.kind === 'bg';
  if (rw > 0 && rh > 0 && (rw < 60 || rh < 60)) return 'small';
  if (nw > 0 && nh > 0 && Math.min(nw, nh) < minSide && !hasBigger) return 'small';
  if (nw === 0 && rw > 0 && rh > 0 && Math.min(rw, rh) < 100 && !hasBigger) return 'small';
  const ar = nw && nh ? nw / nh : rw && rh ? rw / rh : 1;
  if (ar > 4.5 || ar < 0.22) return 'banner';
  return null;
}

/** 画像の見込み点 0..1 と理由 */
function scoreImage(it, goalTerms, opts) {
  const { minSide } = opts;
  const nw = it.nw || 0, nh = it.nh || 0;
  const bestW = Math.max(nw, ...it.srcset.map((c) => c.w || 0), it.w * 2 * (it.srcset.some((c) => c.x >= 2) ? 1 : 0));
  const area = bestW && nh && nw ? bestW * (nh * bestW / nw) : bestW ? bestW * bestW * 0.7 : it.w * it.h;
  const lo = Math.log10(minSide * minSide), hi = Math.log10(2000 * 2000);
  let size = area > 0 ? clamp((Math.log10(area) - lo) / (hi - lo), 0, 1) : 0.3;
  if (it.link && !/\/File(:|%3A)/i.test(it.link) && IMAGE_EXT.test(it.link)) size = Math.max(size, 0.6); // サムネでもリンク先が原寸画像なら大きい版がある
  const shown = it.w > 0 ? clamp(it.w / 600, 0, 1) : 0.4; // 画面上で大きく見せている画像は「見せたい画像」
  let pos = 0.2;
  if (it.inMain) pos += 0.35;
  if (it.inFigure) pos += 0.25;
  if (it.caption) pos += 0.2;
  pos = clamp(pos, 0, 1);
  const fname = decodeURIComponent((it.currentSrc || it.src || '').split('/').pop() || '').replace(/[-_.]+/g, ' ');
  const rel = goalTerms.length ? clamp(textScore(`${it.alt} ${it.caption} ${it.head}`, goalTerms) * 1.0 + textScore(`${it.context} ${fname}`, goalTerms) * 0.6, 0, 1) : 0;
  const score = goalTerms.length ? 0.35 * size + 0.15 * shown + 0.2 * pos + 0.3 * rel : 0.45 * size + 0.2 * shown + 0.35 * pos;
  const why = [];
  why.push(it.inMain ? '本文' : '本文外');
  if (it.inFigure) why.push('図版');
  if (it.caption) why.push('caption あり');
  if (rel > 0.3) why.push(`goal 語一致 ${rel.toFixed(2)}`);
  why.push(bestW ? `幅${bestW}` : `表示${it.w}x${it.h}`);
  return { score: clamp(score, 0, 1), why: why.join('・'), rel, size };
}

/** 取りに行く URL の順(最高解像度から)。重複なし */
function imageCandidates(it) {
  const out = [];
  const push = (u) => { if (u && /^https?:/.test(u) && !out.includes(u)) out.push(u); };
  // リンク先が画像そのもの(wiki の File: 説明ページは HTML なので除く=そちらはページ側で「サムネのリンク」として辿る)
  if (it.link && !/\/File:/.test(it.link) && (IMAGE_EXT.test(it.link) || /upload\.wikimedia\.org\/.*\/commons\/[0-9a-f]\/[0-9a-f]{2}\//.test(it.link))) push(it.link);
  const byW = [...it.srcset].sort((a, b) => (b.w || b.x * 1000) - (a.w || a.x * 1000));
  for (const c of byW.slice(0, 2)) push(c.url);
  for (const u of it.lazy) push(u);
  for (const u of originalVariants(it.currentSrc || it.src)) push(u);
  push(it.currentSrc);
  push(it.src);
  return out.slice(0, 5);
}

/** 行かない理由(null = 候補) */
function junkLink(href, pageUrl, opts) {
  let u;
  try { u = new URL(href); } catch { return 'bad'; }
  if (u.protocol !== 'http:' && u.protocol !== 'https:') return 'scheme';
  if (isPrivateHost(u.hostname)) return 'private';
  if (isMediaHost(u.hostname)) return 'media';
  if (NON_HTML_EXT.test(u.pathname)) return 'nonhtml';
  if (IMAGE_EXT.test(u.pathname) && !/\/File(:|%3A)/i.test(u.pathname)) return 'image'; // 画像直リンクはページではない(画像側の候補で扱う)。wiki の File: 説明ページは HTML
  if (SKIP_LINK.test(href)) return 'skip';
  if ([...u.searchParams.keys()].length > 5) return 'query';
  if (opts.sameSite && !sameSite(href, opts.startUrl)) return 'offsite';
  if (!opts.sameSite && opts.pageOffsite && !sameSite(href, opts.startUrl)) return 'offsite2'; // 別サイトは 1 段だけ
  return null;
}

/** 次に見に行く価値 0..1 と理由 */
function scoreLink(lk, goalTerms, depth, opts) {
  const urlWords = decodeURIComponent(lk.href).replace(/^https?:\/\/[^/]+/, '').replace(/[-_/.+%?=&]+/g, ' ');
  const rel = goalTerms.length
    ? clamp(textScore(`${lk.text} ${lk.imgAlt} ${lk.title}`, goalTerms) * 1.0 + textScore(lk.context, goalTerms) * 0.5 + textScore(urlWords, goalTerms) * 0.5, 0, 1)
    : 0;
  let s = 0.5 * rel;
  const why = [];
  if (rel > 0.3) why.push(`goal 語一致 ${rel.toFixed(2)}`);
  const topic = opts.topicTerms && opts.topicTerms.length ? clamp(textScore(`${lk.text} ${lk.imgAlt} ${lk.title} ${urlWords}`, opts.topicTerms), 0, 1) : 0;
  if (topic > 0) { s += 0.3 * topic; why.push(`節の主題 ${topic.toFixed(2)}`); }
  if (lk.relNext) { s += 0.45; why.push('ページ送り'); }
  else if (PAGINATION.test(lk.href)) { s += 0.25; why.push('ページ番号'); }
  if (GALLERY_WORD.test(lk.href) || GALLERY_WORD.test(lk.text)) { s += 0.22; why.push('gallery/photos 系'); }
  if (lk.wrapsImage) { s += 0.3; why.push('サムネのリンク'); }
  if (/\/File(:|%3A).*\.(svg|gif|ico|pdf|ogg|ogv|webm|mp4|mid|tiff?|djvu|stl)$/i.test(lk.href)) { s -= 0.5; why.push('写真でないファイル'); }
  else if (/\/File(:|%3A).*\.(jpe?g|png|webp)$/i.test(lk.href)) { s += 0.1; why.push('写真ファイルのページ'); }
  if (lk.inMain) { s += 0.1; why.push('本文内'); }
  try {
    const sp = new URL(opts.startUrl).pathname.replace(/\/$/, ''), lp = new URL(lk.href).pathname;
    if (sp.length > 1 && lp.startsWith(sp + '/')) { s += 0.2; why.push('同じ節の下'); }
  } catch {}
  let sameSection = false;
  try {
    const sp = new URL(opts.startUrl).pathname.replace(/\/$/, ''), lp = new URL(lk.href).pathname;
    sameSection = sp.length > 1 && lp.startsWith(sp + '/');
  } catch {}
  // 節の中のサブナビ(Saturn › Facts/Moons/Rings)は人が押す物。サイト全体の nav/footer だけ減点する
  if (lk.chrome) { if (sameSection) { s -= 0.05; why.push('節内ナビ'); } else { s -= 0.35; why.push('nav/footer'); } }
  if (lk.generic) { s -= 0.5; why.push('定型リンク'); }
  if (!sameSite(lk.href, opts.startUrl)) { s -= 0.4; why.push('別サイト'); }
  else { try { if (new URL(lk.href).hostname !== new URL(opts.startUrl).hostname) { s -= 0.15; why.push('別ホスト'); } } catch {} }
  s -= 0.06 * depth;
  if (!lk.text && !lk.imgAlt && !lk.wrapsImage) s -= 0.1;
  return { score: clamp(s, 0, 1), why: why.join('・') || 'リンク', rel };
}

module.exports = { junkImage, scoreImage, imageCandidates, junkLink, scoreLink, originalVariants, normalizeUrl };

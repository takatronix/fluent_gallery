'use strict';
// 任意の LLM 補助(spec §5)。FG_LLM_BASE(OpenAI 互換 chat、例: gallery の llama-server http://127.0.0.1:8081/v1)があるときだけ
// goal を多言語のキーワードに広げる(「柴犬」→ shiba inu / 柴犬 / shiba)。無ければ空を返し、ヒューリスティックだけで動く
const { terms } = require('./util');
const BASE = (process.env.FG_LLM_BASE || '').replace(/\/$/, '');
const MODEL = process.env.FG_LLM_MODEL || 'vlm';

async function chat(prompt, { maxTokens = 200, timeoutMs = 20000 } = {}) {
  if (!BASE) return null;
  const ac = new AbortController();
  const t = setTimeout(() => ac.abort(), timeoutMs);
  try {
    const r = await fetch(BASE + '/chat/completions', {
      method: 'POST', headers: { 'Content-Type': 'application/json' }, signal: ac.signal,
      body: JSON.stringify({ model: MODEL, temperature: 0, max_tokens: maxTokens, messages: [{ role: 'user', content: prompt }] }),
    });
    const j = await r.json();
    return j.choices && j.choices[0] && j.choices[0].message ? String(j.choices[0].message.content || '') : null;
  } catch { return null; } finally { clearTimeout(t); }
}

/** goal → 追加キーワード(主語の英訳と日本語表記)。小型モデルは連想で嘘を混ぜる(柴犬→chihuahua)ので「翻訳だけ」を頼む。失敗は [] */
async function expandGoal(goal) {
  if (!BASE || !goal) return [];
  const txt = await chat(`Goal: "${goal}"\nWhat is the main subject of this goal? Reply ONLY JSON: {"en": "<English name of the subject, 1-3 words>", "ja": "<Japanese name>"}`, { maxTokens: 80 });
  if (!txt) return [];
  const m = txt.match(/\{[\s\S]*\}/);
  if (!m) return [];
  try {
    const o = JSON.parse(m[0]);
    const out = [];
    for (const v of [o.en, o.ja]) {
      if (typeof v !== 'string') continue;
      const s = v.trim().toLowerCase();
      if (s.length < 2 || s.length > 40) continue;
      out.push(s);
      if (/^[a-z0-9 .'-]+$/.test(s)) for (const w of terms(s)) if (w.length >= 3 && !out.includes(w)) out.push(w); // "shiba inu" → shiba, inu(photo/view 等の一般語は terms() が落とす)
    }
    return [...new Set(out)].slice(0, 6);
  } catch { return []; }
}

module.exports = { chat, expandGoal, enabled: !!BASE };

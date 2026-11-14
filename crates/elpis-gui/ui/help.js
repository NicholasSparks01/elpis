// Elpis Help window. The pages are the project's own docs, rendered to HTML by the Rust side (src/help.rs), which
// escapes any raw HTML a document contains, so the article can be inserted as it is. Everything else on this page
// (contents, search results, titles) is built with textContent.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

function h(tag, props, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props || {})) {
    if (v == null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k === 'text') el.textContent = v;
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (k === 'dataset') Object.assign(el.dataset, v);
    else el.setAttribute(k, v === true ? '' : v);
  }
  for (const kid of kids.flat()) {
    if (kid == null || kid === false) continue;
    el.append(kid.nodeType ? kid : document.createTextNode(String(kid)));
  }
  return el;
}
const clear = (el) => { el.replaceChildren(); return el; };

// ---------------------------------------------------------------- theme: the same choice as the main window

const THEMES = ['dark', 'light', 'auto'];
function readTheme() { try { const s = localStorage.getItem('elpis.theme'); return THEMES.includes(s) ? s : 'dark'; } catch { return 'dark'; } }
function applyTheme(t) {
  if (t === 'auto') delete document.documentElement.dataset.theme; else document.documentElement.dataset.theme = t;
  $('theme').textContent = { dark: '☾', light: '☀', auto: '◐' }[t];
  $('theme').title = 'Theme: ' + (t === 'auto' ? 'follows your system' : t) + ' (click to change)';
}
let theme = readTheme();
applyTheme(theme);
$('theme').onclick = () => {
  theme = THEMES[(THEMES.indexOf(theme) + 1) % THEMES.length];
  try { localStorage.setItem('elpis.theme', theme); } catch {}
  applyTheme(theme);
};
// the main window changing it changes this one too
window.addEventListener('storage', (e) => { if (e.key === 'elpis.theme') { theme = readTheme(); applyTheme(theme); } });

// ---------------------------------------------------------------- pages and navigation

let pages = [];                // [{id, title, group, headings: [{level, id, text}]}]
const byId = new Map();
const htmlCache = new Map();
let current = null;            // {id, anchor}
let hist = [];
let pos = -1;
let searching = false;
let lastHits = [];
let queued = null;             // a topic that arrived before the contents were loaded

/** "page#anchor" -> {id, anchor} */
function parseTopic(topic) {
  const [id, anchor] = String(topic || '').split('#');
  return { id: id || 'index', anchor: anchor || '' };
}

const failure = (e) => String(e && e.message ? e.message : e);
function showError(e) { clear($('content')).append(h('p', { class: 'none', text: failure(e) })); }

async function loadPage(id) {
  if (!htmlCache.has(id)) htmlCache.set(id, await invoke('help_page', { id }));
  return htmlCache.get(id);
}

/** Open a topic. `push: false` is for Back and Forward. An unknown topic shows the contents page. */
async function go(topic, opts = {}) {
  let { id, anchor } = parseTopic(topic);
  if (!byId.has(id)) { if (id === 'index') return showError('The help pages could not be loaded.'); id = 'index'; anchor = ''; }
  clearSearch();
  const samePage = current && current.id === id && $('content').dataset.page === id;
  if (!samePage) {
    let page;
    try { page = await loadPage(id); } catch (e) { return showError(e); }
    renderPage(page);
  }
  current = { id, anchor };
  if (opts.push !== false) { hist = hist.slice(0, pos + 1); hist.push({ id, anchor }); pos = hist.length - 1; }
  scrollToAnchor(anchor);
  renderContents();
  renderToc();
  updateNav();
}

function updateNav() {
  $('back').disabled = pos <= 0;
  $('fwd').disabled = pos >= hist.length - 1;
}
function back() { if (pos > 0) { pos--; go(hist[pos].id + (hist[pos].anchor ? '#' + hist[pos].anchor : ''), { push: false }); } }
function forward() { if (pos < hist.length - 1) { pos++; go(hist[pos].id + (hist[pos].anchor ? '#' + hist[pos].anchor : ''), { push: false }); } }
$('back').onclick = back;
$('fwd').onclick = forward;

// ---------------------------------------------------------------- the page

function renderPage(page) {
  const el = $('content');
  el.dataset.page = page.id;
  el.innerHTML = page.html;            // trusted: rendered from the bundled docs; raw HTML in them was escaped
  // A heading's id could clash with this window's own (a "Contents" heading, say), so they live under a prefix.
  for (const node of el.querySelectorAll('[id]')) node.id = 'h-' + node.id;
  for (const pre of el.querySelectorAll('pre')) {
    pre.append(h('button', { class: 'copy small', text: 'Copy', onclick: async (e) => {
      const b = e.currentTarget;
      try { await navigator.clipboard.writeText(pre.querySelector('code').textContent); b.textContent = 'Copied'; }
      catch { b.textContent = 'Select and copy'; }
      setTimeout(() => (b.textContent = 'Copy'), 1500);
    } }));
  }
  // previous and next in reading order
  const i = pages.findIndex((p) => p.id === page.id);
  const link = (p, label) => h('button', { onclick: () => go(p.id) }, label);
  el.append(h('div', { class: 'footer-nav' },
    i > 0 ? link(pages[i - 1], '← ' + pages[i - 1].title) : h('span'),
    i < pages.length - 1 ? link(pages[i + 1], pages[i + 1].title + ' →') : h('span')));
  document.title = page.title + ' · Elpis Help';
  $('crumb').textContent = byId.get(page.id).group + ' › ' + page.title;
}

function scrollToAnchor(anchor) {
  const scroller = $('scroller');
  const target = anchor && $('content').querySelector('[id="' + CSS.escape('h-' + anchor) + '"]');
  if (!target) { scroller.scrollTo({ top: 0, behavior: 'auto' }); return; }
  target.scrollIntoView({ block: 'start', behavior: 'auto' });
  target.classList.remove('flash');
  void target.offsetWidth;             // restart the highlight if it was just shown
  target.classList.add('flash');
}

// links inside the article: other pages, and headings on this page
$('content').addEventListener('click', (e) => {
  const a = e.target.closest('a');
  if (!a) return;
  e.preventDefault();
  if (a.classList.contains('doc-link')) go(a.dataset.topic);
  else if (a.classList.contains('in-page')) scrollToAnchor(a.getAttribute('href').slice(1));
});

// ---------------------------------------------------------------- contents and "on this page"

function renderContents() {
  const nav = clear($('contents'));
  let group = null;
  for (const p of pages) {
    if (p.group !== group) { group = p.group; nav.append(h('div', { class: 'group', text: group })); }
    const here = !searching && current && current.id === p.id;
    nav.append(h('button', { class: 'page-link' + (here ? ' current' : ''), text: p.title, 'aria-current': here ? 'page' : null, onclick: () => go(p.id) }));
    if (here) {
      const subs = p.headings.filter((x) => x.level === 2);
      if (subs.length) nav.append(h('div', { class: 'subs' }, subs.map((x) => h('button', { class: 'sub-link', text: x.text, onclick: () => go(p.id + '#' + x.id) }))));
    }
  }
}

let spy = [];                   // [{button, heading}]
function renderToc() {
  const toc = clear($('toc'));
  spy = [];
  const p = current && byId.get(current.id);
  if (!p || searching) return;
  const heads = p.headings.filter((x) => x.level <= 3);
  if (!heads.length) return;
  toc.append(h('div', { class: 't', text: 'On this page' }));
  for (const x of heads) {
    const button = h('button', { class: 'l' + x.level, text: x.text, onclick: () => scrollToAnchor(x.id) });
    toc.append(button);
    const heading = $('content').querySelector('[id="' + CSS.escape('h-' + x.id) + '"]');
    if (heading) spy.push({ button, heading });
  }
  updateSpy();
}
function updateSpy() {
  if (!spy.length) return;
  const top = $('scroller').getBoundingClientRect().top + 90;
  let active = spy[0];
  for (const s of spy) if (s.heading.getBoundingClientRect().top <= top) active = s;
  for (const s of spy) s.button.classList.toggle('on', s === active);
}
let spyFrame = 0;
$('scroller').addEventListener('scroll', () => { cancelAnimationFrame(spyFrame); spyFrame = requestAnimationFrame(updateSpy); });

// ---------------------------------------------------------------- search

let searchTimer = 0;
function highlight(text, words) {
  if (!words.length) return [text];
  const re = new RegExp('(' + words.map((w) => w.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')).join('|') + ')', 'gi');
  return text.split(re).map((part, i) => (i % 2 ? h('mark', { text: part }) : part));
}

function renderResults(query, hits) {
  const words = query.split(/\s+/).filter(Boolean);
  const el = clear($('content'));
  delete el.dataset.page;
  el.append(h('div', { class: 'results' },
    h('h1', { text: hits.length ? `${hits.length} result${hits.length === 1 ? '' : 's'} for “${query}”` : `Nothing found for “${query}”` }),
    hits.length ? null : h('p', { class: 'none', text: 'Try fewer or different words. Every word you type has to appear in the same section.' }),
    hits.map((hit) => h('button', { class: 'hit', onclick: () => openHit(hit) },
      h('div', { class: 'where' }, hit.section ? `${hit.pageTitle} › ${hit.section}` : hit.pageTitle, h('span', { class: 'group', text: hit.group })),
      h('div', { class: 'snip' }, highlight(hit.snippet, words))))));
  $('scroller').scrollTo({ top: 0, behavior: 'auto' });
  clear($('toc'));
  document.title = 'Search · Elpis Help';
  $('crumb').textContent = 'Search results';
}

function openHit(hit) {
  $('q').value = '';
  go(hit.page + (hit.anchor ? '#' + hit.anchor : ''));
}

async function runSearch() {
  const query = $('q').value.trim();
  if (!query) {
    if (searching && current) go(current.id + (current.anchor ? '#' + current.anchor : ''), { push: false });
    return;
  }
  let hits;
  try { hits = await invoke('help_search', { query }); } catch (e) { return showError(e); }
  if ($('q').value.trim() !== query) return;     // a newer query is already on its way
  searching = true;
  lastHits = hits;
  renderResults(query, hits);
  renderContents();
}
function clearSearch() { searching = false; lastHits = []; }

$('q').addEventListener('input', () => { clearTimeout(searchTimer); searchTimer = setTimeout(runSearch, 130); });
$('q').addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && lastHits.length) { e.preventDefault(); openHit(lastHits[0]); }
  else if (e.key === 'ArrowDown') { const first = $('content').querySelector('.hit'); if (first) { e.preventDefault(); first.focus(); } }
});

document.addEventListener('keydown', (e) => {
  const typing = e.target === $('q');
  if ((e.key === '/' && !typing && !e.ctrlKey && !e.metaKey && !e.altKey) || (e.key.toLowerCase() === 'f' && (e.ctrlKey || e.metaKey))) {
    e.preventDefault(); $('q').focus(); $('q').select();
  } else if (e.key === 'Escape' && (typing || searching)) {
    $('q').value = ''; $('q').blur(); runSearch();
  } else if (e.altKey && e.key === 'ArrowLeft') { e.preventDefault(); back(); }
  else if (e.altKey && e.key === 'ArrowRight') { e.preventDefault(); forward(); }
});

// ---------------------------------------------------------------- start

listen('help-open', (e) => {
  if (!pages.length) { queued = e.payload || 'index'; return; }
  go(e.payload || 'index');
});

(async () => {
  try { pages = await invoke('help_pages'); } catch (e) { return showError(e); }
  for (const p of pages) byId.set(p.id, p);
  const asked = queued || (await invoke('help_take_topic').catch(() => null));
  await go(asked || 'index');
})();

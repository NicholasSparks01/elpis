// elpis-gui front end. The page talks to the Rust side through Tauri commands (see src/main.rs and src/capsules.rs).
//
// Everything that came from a manifest, a registry or a box is shown with textContent (through h() below), never
// innerHTML: the backend already replaces control characters, and this keeps markup in that text inert.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

/** Build an element. props: class, text, on<Event>, dataset, or any attribute; children: nodes, strings or arrays. */
function h(tag, props, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props || {})) {
    if (v == null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k === 'text') el.textContent = v;
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (k === 'dataset') Object.assign(el.dataset, v);
    else if (k === 'value') el.value = v;
    else el.setAttribute(k, v === true ? '' : v);
  }
  for (const kid of kids.flat()) {
    if (kid == null || kid === false) continue;
    el.append(kid.nodeType ? kid : document.createTextNode(String(kid)));
  }
  return el;
}

function clear(el) { el.replaceChildren(); return el; }

// ---------------------------------------------------------------- toasts and dialogs

function toast(text, kind = 'info') {
  const t = h('div', { class: 'toast ' + kind, text });
  $('toasts').append(t);
  setTimeout(() => t.remove(), kind === 'error' ? 10000 : 5000);
}

const failure = (e) => String(e && e.message ? e.message : e);

/**
 * A dialog. `body` is a node; `actions` are {label, primary, danger, keepOpen, onClick(dialog)}; a click handler
 * may return a promise (the buttons are disabled while it runs) and a thrown error is shown in the dialog.
 * Returns {close, setProgress, setError, el}.
 */
function dialog(title, body, actions, opts = {}) {
  const root = $('modal-root');
  const errorLine = h('div', { class: 'err', hidden: true });
  const progress = h('span', { class: 'mprogress' });
  const buttons = [];
  const api = {
    el: null,
    close() { scrim.remove(); document.removeEventListener('keydown', onKey, true); opts.onClose?.(); },
    setProgress(text) { progress.textContent = text || ''; },
    setError(text) { errorLine.hidden = !text; errorLine.textContent = text || ''; },
  };
  const busy = (on) => buttons.forEach((b) => (b.disabled = on));
  for (const a of actions) {
    const b = h('button', { class: (a.primary ? 'primary ' : '') + (a.danger ? 'danger' : ''), text: a.label });
    b.addEventListener('click', async () => {
      api.setError('');
      if (!a.onClick) return api.close();
      busy(true);
      try { await a.onClick(api); if (!a.keepOpen) api.close(); }
      catch (e) { api.setError(failure(e)); }
      finally { busy(false); api.setProgress(''); }
    });
    buttons.push(b);
  }
  const scrim = h('div', { class: 'scrim', onmousedown: (e) => { if (e.target === scrim) api.close(); } },
    h('div', { class: 'modal', role: 'dialog', 'aria-modal': 'true', 'aria-label': title },
      h('header', { text: title }),
      h('div', { class: 'mbody' }, body, errorLine),
      h('footer', null, progress, buttons)));
  const onKey = (e) => { if (e.key === 'Escape' && root.lastElementChild === scrim) { e.stopImmediatePropagation(); api.close(); } };
  document.addEventListener('keydown', onKey, true);
  root.append(scrim);
  api.el = scrim;
  (scrim.querySelector('input, select, textarea') || buttons[buttons.length - 1])?.focus();
  return api;
}

function formRow(label, control, hint) {
  return h('div', { class: 'formrow' }, h('label', { text: label }), control, hint && h('div', { class: 'note', text: hint }));
}

const textInput = (value = '', placeholder = '') => h('input', { type: 'text', value, placeholder, autocomplete: 'off', spellcheck: 'false' });

function confirmDialog(title, message, label, danger) {
  return new Promise((resolve) => {
    // closing any other way (Esc, a click outside) is a no
    dialog(title, h('div', { text: message }), [
      { label: 'Cancel', onClick: async () => resolve(false) },
      { label, primary: !danger, danger, onClick: async () => resolve(true) },
    ], { onClose: () => resolve(false) });
  });
}

async function copyText(text) {
  try { await navigator.clipboard.writeText(text); toast('copied', 'ok'); }
  catch { toast('couldn\'t copy: select the text and copy it yourself'); }
}

// ---------------------------------------------------------------- boxes (the sidebar and the Box view)

let current = null;            // selected VM id
const vms = new Map();         // id -> {id, work, namespace, path, running, source}
const logs = new Map();        // id -> array of line nodes (kept per VM)
let view = 'box';

function renderList() {
  const el = clear($('vmlist'));
  for (const vm of vms.values()) {
    const fromCapsule = vm.source && vm.source.startsWith('capsule ');
    el.append(h('div', {
      class: 'vm' + (vm.running ? ' running' : '') + (vm.id === current ? ' active' : ''),
      title: vm.source || '',
      onclick: () => { select(vm.id); showView('box'); },
    },
      h('div', { class: 'name' }, h('span', { class: 'state' }), vm.id, fromCapsule && h('span', { class: 'from', text: 'capsule' })),
      h('div', { class: 'path', text: vm.path })));
  }
  if (!vms.size) {
    el.append(h('div', { class: 'empty' }, 'No boxes found yet.', h('br'), 'Run a Capsule from the Capsules tab, or create one with ', h('code', { text: 'elpis-run create' }), '.'));
  }
}

function select(id) {
  current = id;
  const vm = vms.get(id);
  $('title').textContent = vm ? vm.id : 'Select a VM';
  const log = clear($('log'));
  for (const node of (logs.get(id) || [])) log.append(node);
  if (!logs.get(id)?.length) log.append(h('div', { class: 'empty', text: vm ? 'Not started yet.' : 'Pick a VM on the left, then Start.' }));
  log.scrollTop = log.scrollHeight;
  renderList(); updateButtons();
}

function updateButtons() {
  const vm = vms.get(current);
  $('start').disabled = !vm || vm.running;
  $('stop').disabled = !vm || !vm.running;
  $('term').disabled = !vm || !vm.running;
  $('agent').disabled = !vm || !vm.running;
}

function addLine(id, line) {
  if (!logs.has(id)) logs.set(id, []);
  const arr = logs.get(id);
  const node = h('div', { class: 'ln' },
    h('span', { class: 't', text: line.time }),
    h('span', { class: 'g ' + line.color, text: `[${line.tag}]` }),
    h('span', { class: line.color, text: line.text }));
  arr.push(node);
  if (arr.length > 2000) arr.shift();
  if (id === current) {
    const log = $('log');
    log.querySelector('.empty')?.remove();
    const atBottom = log.scrollHeight - log.scrollTop - log.clientHeight < 40;
    log.append(node);
    if (atBottom) log.scrollTop = log.scrollHeight;
  }
}

async function refreshBoxes(selectId) {
  const list = await invoke('list_vms');
  vms.clear();
  for (const vm of list) vms.set(vm.id, vm);
  if (selectId && vms.has(selectId)) current = selectId;
  if (current && !vms.has(current)) current = null;
  if (!current && list.length) current = list[0].id;
  select(current);
  $('shimline').textContent = await invoke('shim_info');
}

$('refresh').onclick = () => refreshBoxes();
$('clear').onclick = () => { if (current) { logs.set(current, []); select(current); } };

$('start').onclick = async () => {
  const vm = vms.get(current); if (!vm) return;
  const hp = $('hostport').value.trim();
  const ports = hp ? hp.split(',').map((s) => parseInt(s.trim())).filter(Number.isInteger) : [];
  $('start').disabled = true;
  try { await invoke('start_vm', { id: vm.id, ports }); }
  catch (e) { addLine(vm.id, { time: '', tag: 'gui', color: 'red', text: 'start failed: ' + failure(e) }); updateButtons(); }
};
$('stop').onclick = async () => { const vm = vms.get(current); if (vm) { $('stop').disabled = true; await invoke('stop_vm', { id: vm.id }); } };
$('term').onclick = () => current && invoke('open_terminal', { id: current, command: 'bash' }).catch((e) => toast(failure(e), 'error'));
$('agent').onclick = async () => {
  if (!current) return;
  try { await invoke('open_terminal', { id: current, command: await invoke('agent_command', { id: current }) }); }
  catch (e) { toast(failure(e), 'error'); }
};

listen('log', (e) => addLine(e.payload.id, e.payload));
listen('vm-state', (e) => { const vm = vms.get(e.payload.id); if (vm) { vm.running = e.payload.running; renderList(); updateButtons(); } });

// ---------------------------------------------------------------- views

function showView(name) {
  view = name;
  for (const b of document.querySelectorAll('.tabs button')) {
    const on = b.dataset.view === name;
    b.classList.toggle('active', on);
    b.setAttribute('aria-selected', on);
  }
  for (const v of ['box', 'capsules', 'trust']) $('view-' + v).hidden = v !== name;
  if (name === 'capsules') loadCapsules();
  if (name === 'trust') loadTrust();
}
for (const b of document.querySelectorAll('.tabs button')) b.onclick = () => showView(b.dataset.view);

// ---------------------------------------------------------------- Capsules

let caps = [];                 // CapsuleRow[]
const badges = new Map();      // digest -> {text, level}
let selectedRef = null;
let filterText = '';
let searchHits = null;         // registry search results, or null
let loadSeq = 0;

const badgeEl = (b) => h('span', { class: 'tb ' + (b ? b.level : 'dim'), text: b ? b.text : 'checking…' });

function matches(c) {
  const words = filterText.toLowerCase().split(/\s+/).filter(Boolean);
  const text = (c.name + ' ' + c.description).toLowerCase();
  return words.every((w) => text.includes(w));
}

function renderCapTable() {
  const el = clear($('captable'));
  const shown = caps.filter(matches);
  if (!caps.length) {
    el.append(h('div', { class: 'empty' },
      h('div', { text: 'No Capsules yet.' }),
      h('div', { class: 'note', text: 'A Capsule is a portable agent: its image, files and start command, and what it asks for. Build one from a folder, or pull one from a registry.' }),
      h('div', { style: 'margin-top:12px;display:flex;gap:8px;justify-content:center' },
        h('button', { text: 'Build…', onclick: buildDialog }), h('button', { text: 'Pull…', onclick: pullDialog }))));
    return;
  }
  if (!shown.length) { el.append(h('div', { class: 'empty', text: `No Capsule in your store matches “${filterText}”.` })); return; }
  const rows = shown.map((c) => h('tr', {
    class: 'row' + (c.reference === selectedRef ? ' sel' : ''), tabindex: '0', dataset: { ref: c.reference },
    onclick: () => selectCapsule(c.reference),
    onkeydown: (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); selectCapsule(c.reference); } },
  },
    h('td', null, h('strong', { text: c.reference })),
    h('td', { class: 'trust' }, badgeEl(badges.get(c.digest))),
    h('td', { text: String(c.payloadFiles) }),
    h('td', { class: 'note', text: c.origin ? 'from ' + c.origin.split('/')[0] : 'built here' }),
    h('td', { class: 'desc', text: c.description, title: c.description })));
  el.append(h('table', null,
    h('thead', null, h('tr', null, ['Capsule', 'Trust', 'Files', 'Origin', 'Description'].map((t) => h('th', { text: t })))),
    h('tbody', null, rows)));
}

function updateBadges() {
  for (const tr of document.querySelectorAll('#captable tr.row')) {
    const c = caps.find((x) => x.reference === tr.dataset.ref);
    if (c) tr.querySelector('td.trust').replaceChildren(badgeEl(badges.get(c.digest)));
  }
}

async function loadCapsules(keepSelection = true) {
  const seq = ++loadSeq;
  try { caps = await invoke('capsule_list'); }
  catch (e) { toast('couldn\'t list Capsules: ' + failure(e), 'error'); return; }
  if (!keepSelection || !caps.some((c) => c.reference === selectedRef)) selectedRef = caps[0]?.reference || null;
  renderCapTable();
  renderDetail();
  // checking reads whole payloads, so it follows the list instead of holding it up
  try {
    const results = await invoke('capsule_verify_all');
    if (seq !== loadSeq) return;
    badges.clear();
    for (const [digest, b] of results) badges.set(digest, b);
    updateBadges();
  } catch (e) { toast('couldn\'t check Capsules: ' + failure(e), 'error'); }
}

async function selectCapsule(ref) {
  selectedRef = ref;
  for (const tr of document.querySelectorAll('#captable tr.row')) tr.classList.toggle('sel', tr.dataset.ref === ref);
  await renderDetail();
}

async function renderDetail() {
  const el = clear($('capdetail'));
  if (searchHits) return renderResults(el);
  const c = caps.find((x) => x.reference === selectedRef);
  if (!c) { el.append(h('div', { class: 'empty', text: 'Select a Capsule to see what it asks for and who vouches for it.' })); return; }
  el.append(h('div', { class: 'note', text: 'checking…' }));
  const seq = ++renderDetail.seq || (renderDetail.seq = 1);
  let d;
  try { d = await invoke('capsule_detail', { reference: c.reference }); }
  catch (e) { clear($('capdetail')).append(h('div', { class: 'err', text: failure(e) })); return; }
  if (seq !== renderDetail.seq || selectedRef !== c.reference) return;
  badges.set(d.digest, d.report.badge);
  updateBadges();
  const out = clear($('capdetail'));
  out.append(
    h('div', { class: 'dhead' },
      h('h3', { text: d.reference }), badgeEl(d.report.badge),
      h('div', { class: 'actions' },
        h('button', { class: 'primary', text: 'Run…', onclick: () => runDialog(d.reference) }),
        h('button', { text: 'Verify', title: 'Check its files, payload and signatures again', onclick: async () => { await renderDetail(); toast('checked ' + d.reference, 'ok'); } }),
        h('button', { text: 'Sign…', onclick: () => signDialog(d.reference) }),
        h('button', { text: 'Push…', onclick: () => pushDialog(d.reference) }),
        h('button', { text: 'Fork…', onclick: () => forkDialog(d.reference) }),
        h('button', { class: 'danger', text: 'Remove', onclick: () => removeCapsule(d.reference) }))),
    h('div', { class: 'cols' },
      h('div', { class: 'card' }, h('h4', { text: 'What it runs and asks for' }), h('div', { class: 'summary', text: d.summary.slice(1).join('\n') })),
      h('div', { class: 'card' }, h('h4', { text: 'Verification' }), verificationNodes(d.report))));
}

function verificationNodes(r) {
  const nodes = r.checks.map((c) => h('div', { class: 'check' },
    h('span', { class: 'mark ' + (c.ok ? 'ok' : 'fail'), text: c.ok ? 'ok' : 'FAIL' }), h('span', { text: `${c.name}${c.detail ? ' (' + c.detail + ')' : ''}` })));
  nodes.push(h('div', { class: 'note', text: 'source: ' + (r.provenance || 'not recorded (not built from a git repository)') }));
  if (!r.signatures.length) nodes.push(h('div', { class: 'note', text: 'signatures: none' }));
  const who = { trusted: (s) => `valid, by ${s.detail}, whom you trust for this namespace`, 'out-of-scope': (s) => `valid, by ${s.detail}, but you don't trust that key for this namespace`, unknown: () => 'valid, by a key you haven\'t trusted', invalid: (s) => 'INVALID: ' + s.detail };
  const level = { trusted: 'good', 'out-of-scope': 'warn', unknown: 'warn', invalid: 'bad' };
  for (const s of r.signatures) {
    nodes.push(h('div', { class: 'sig' },
      h('span', { class: 'tb ' + level[s.standing], text: s.standing }), ' ', h('span', { text: who[s.standing](s) }),
      h('div', { class: 'keyline' }, h('span', { class: 'mono', text: s.publicKey, title: s.publicKey }),
        s.standing === 'unknown' && h('button', { class: 'small', text: 'Trust this key…', onclick: () => trustDialog(s.publicKey, selectedRef) }))));
  }
  nodes.push(h('div', { class: 'verdict' }, 'Verdict: ', h('span', { class: 'tb ' + r.badge.level, text: r.verdict })));
  return nodes;
}

function renderResults(el) {
  clear(el);
  el.append(h('div', { class: 'dhead' }, h('h3', { text: searchHits.length ? `${searchHits.length} result(s) in the registry` : 'Nothing found' }),
    h('div', { class: 'actions' }, h('button', { text: 'Back to my Capsules', onclick: () => { searchHits = null; renderDetail(); } }))));
  if (!searchHits.length) el.append(h('div', { class: 'note', text: 'No Capsule in that registry matches.' }));
  for (const hit of searchHits) {
    el.append(h('div', { class: 'hit' },
      h('div', { class: 'body' },
        h('div', { class: 'ref', text: hit.reference }),
        h('div', { class: 'note', text: hit.description || '(no description)' }),
        hit.signaturesPublished && h('span', { class: 'tb warn', title: 'Someone signed it. Whether you trust them is for Verify and your trust list to say.', text: 'signatures published' })),
      h('div', { style: 'display:flex;gap:6px' },
        h('button', { class: 'small', text: 'Inspect', onclick: () => inspectRemote(hit.reference) }),
        h('button', { class: 'small primary', text: 'Pull', onclick: () => pullDialog(hit.reference) }))));
  }
  el.append(h('div', { class: 'note', style: 'margin-top:10px', text: 'Names and descriptions come from the registry and are unchecked. Inspect reads only the manifest; nothing runs until you pull it and run it, and running asks what to grant.' }));
}

// search ---------------------------------------------------------------

$('cscope').onchange = () => { $('creg').hidden = $('cscope').value !== 'registry'; if (!$('creg').hidden) $('creg').focus(); };
$('cq').onkeydown = (e) => { if (e.key === 'Enter') $('csearch').click(); };
$('cq').oninput = () => {
  if ($('cscope').value !== 'store') return;
  filterText = $('cq').value;
  renderCapTable();
  if (searchHits) { searchHits = null; renderDetail(); }
};
$('csearch').onclick = async () => {
  if ($('cscope').value === 'store') { filterText = $('cq').value; searchHits = null; renderCapTable(); return renderDetail(); }
  const registry = $('creg').value.trim();
  if (!registry) { toast('type the registry to search, e.g. registry.example.com/team', 'error'); return $('creg').focus(); }
  try { localStorage.setItem('elpis.registry', registry); } catch {}
  $('csearch').disabled = true;
  try { searchHits = await invoke('capsule_search', { query: $('cq').value, registry, allVersions: false }); renderDetail(); }
  catch (e) { toast(failure(e), 'error'); }
  finally { $('csearch').disabled = false; $('cprogress').textContent = ''; }
};

async function inspectRemote(ref) {
  try {
    const [lines, digest] = await invoke('capsule_inspect_remote', { reference: ref });
    dialog('Registry manifest', h('div', null,
      h('div', { class: 'note', text: ref + '  ·  manifest ' + digest }),
      h('div', { class: 'summary', style: 'margin-top:8px', text: lines.join('\n') })), [
      { label: 'Close' }, { label: 'Pull…', primary: true, onClick: async () => pullDialog(ref) }]);
  } catch (e) { toast(failure(e), 'error'); }
}

// actions --------------------------------------------------------------

listen('capsule-progress', (e) => {
  $('cprogress').textContent = e.payload;
  for (const p of document.querySelectorAll('.mprogress')) p.textContent = e.payload;
});

async function removeCapsule(ref) {
  if (!(await confirmDialog('Remove Capsule', `Remove ${ref} from your store? Boxes already made from it keep working; a registry copy is untouched.`, 'Remove', true))) return;
  try { await invoke('capsule_remove', { reference: ref }); toast('removed ' + ref, 'ok'); selectedRef = null; await loadCapsules(false); }
  catch (e) { toast(failure(e), 'error'); }
}

function buildDialog() {
  const dir = textInput('', 'C:\\path\\to\\my-capsule');
  const force = h('input', { type: 'checkbox' });
  dialog('Build a Capsule', h('div', { style: 'display:flex;flex-direction:column;gap:10px' },
    formRow('Capsule folder (the one with capsule.yaml)', dir, 'The manifest is validated, the payload is packed, and the result is stored as NAME@VERSION. Files holding obvious secrets are refused.'),
    h('label', { class: 'check-row' }, force, h('span', { text: 'Replace this version if it was built with different content' }))), [
    { label: 'Cancel' },
    { label: 'Build', primary: true, keepOpen: true, onClick: async (d) => {
      const row = await invoke('capsule_build', { dir: dir.value, force: force.checked });
      d.close(); toast('built ' + row.reference, 'ok'); selectedRef = row.reference; searchHits = null; await loadCapsules();
    } }]);
}

function pullDialog(prefill = '') {
  const ref = textInput(prefill, 'registry.example.com/team/namespace/name:1.0.0');
  const force = h('input', { type: 'checkbox' });
  dialog('Pull a Capsule', h('div', { style: 'display:flex;flex-direction:column;gap:10px' },
    formRow('Registry reference', ref, 'Add @sha256:… instead of :VERSION to pin exactly what you reviewed. Nothing runs from a pull.'),
    h('label', { class: 'check-row' }, force, h('span', { text: 'Replace a stored version that has different content' }))), [
    { label: 'Cancel' },
    { label: 'Pull', primary: true, keepOpen: true, onClick: async (d) => {
      const row = await invoke('capsule_pull', { reference: ref.value, force: force.checked });
      d.close(); searchHits = null; selectedRef = row.reference; await loadCapsules();
      toast(`pulled ${row.reference}: ${(badges.get(row.digest) || {}).text || 'check its signatures in the details'}`, 'ok');
    } }]);
}

async function runDialog(ref, allowUnsigned = false) {
  let p;
  try { p = await invoke('capsule_prepare_run', { reference: ref, allowUnsigned }); }
  catch (e) {
    const msg = failure(e);
    // a policy refusal can be overridden for one run; a failed integrity check can't
    const policy = !allowUnsigned && msg.includes('policy requires');
    dialog(policy ? 'Your trust policy refuses this Capsule' : 'This Capsule can\'t be run', h('div', { class: 'err', text: msg }),
      [{ label: 'Close' }, policy && { label: 'Run anyway, just this once', danger: true, onClick: async () => runDialog(ref, true) }].filter(Boolean));
    return;
  }
  const name = textInput(p.boxName);
  const ws = p.declaresWorkspace ? textInput('', 'C:\\code\\my-project') : null;
  const asks = h('div', { class: 'asks' },
    h('div', { class: 'line' }, h('span', { class: 'k', text: 'network' }), h('span', { text: p.network.length ? p.network.join(', ') : 'none: the box will be offline' })),
    p.hostAccess && h('div', { class: 'line' }, h('span', { class: 'k', text: 'host' }), h('span', { style: 'color:var(--yellow)', text: 'access to this Windows machine\'s services' })),
    p.credentials.length > 0 && h('div', { class: 'line' }, h('span', { class: 'k', text: 'keys' }), h('span', { class: 'note', text: `credentials requested (${p.credentials.join(', ')}) aren't injected yet; the agent signs in itself` })));
  dialog('Run ' + p.reference, h('div', { style: 'display:flex;flex-direction:column;gap:10px' },
    h('div', null, 'Standing: ', h('span', { class: 'tb ' + p.badge.level, text: p.verdict }), allowUnsigned && h('span', { class: 'note', text: '  (policy overridden for this run)' })),
    h('div', { class: 'note', text: 'It asks for the following. Granting creates the box with exactly this as its network policy; anything else stays blocked.' }),
    asks,
    formRow('Box name', name),
    ws && formRow('Workspace folder (shared into the box)', ws, 'The agent can change anything in it.')), [
    { label: 'Cancel' },
    { label: 'Grant and create', primary: true, keepOpen: true, onClick: async (d) => {
      const box = await invoke('capsule_run', { reference: p.reference, digest: p.digest, name: name.value, workspace: ws ? ws.value : '', allowUnsigned });
      d.close(); toast(`created ${box}: press Start to boot it`, 'ok');
      await refreshBoxes(box); showView('box');
    } }]);
}

async function signDialog(ref) {
  let keys = [];
  try { keys = await invoke('capsule_keys'); } catch (e) { toast(failure(e), 'error'); return; }
  if (!keys.length) {
    dialog('Sign a Capsule', h('div', { text: 'You have no signing key yet. A key lets you vouch for a Capsule; its private half stays in Windows Credential Manager.' }),
      [{ label: 'Cancel' }, { label: 'Make a key…', primary: true, onClick: async () => keygenDialog(() => signDialog(ref)) }]);
    return;
  }
  const sel = h('select', null, keys.map((k) => h('option', { value: k.name, text: k.name })));
  dialog('Sign ' + ref, h('div', { style: 'display:flex;flex-direction:column;gap:10px' },
    h('div', { text: 'Signing says this key\'s holder vouches for exactly these files and the git state they were built from. It isn\'t a promise the Capsule is safe.' }),
    formRow('Signing key', sel)), [
    { label: 'Cancel' },
    { label: 'Sign', primary: true, onClick: async () => { await invoke('capsule_sign', { reference: ref, key: sel.value }); toast('signed ' + ref + ' (push it to publish the signature)', 'ok'); await loadCapsules(); } }]);
}

function keygenDialog(then) {
  const name = textInput('', 'e.g. alice');
  dialog('New signing key', formRow('Key name', name, 'Letters, digits, - and _. The private key is kept in Windows Credential Manager and never shown.'), [
    { label: 'Cancel' },
    { label: 'Create', primary: true, keepOpen: true, onClick: async (d) => {
      const pub = await invoke('capsule_keygen', { name: name.value });
      d.close(); toast(`created key ${name.value.trim()}`, 'ok');
      if (view === 'trust') loadTrust();
      if (then) then(); else await copyText(pub);
    } }]);
}

function pushDialog(ref) {
  let last = ''; try { last = localStorage.getItem('elpis.registry') || ''; } catch {}
  const to = textInput(last, 'registry.example.com/team');
  const force = h('input', { type: 'checkbox' });
  dialog('Push ' + ref, h('div', { style: 'display:flex;flex-direction:column;gap:10px' },
    formRow('Registry (and optional path)', to, 'Stored as REGISTRY/PATH/NAMESPACE/NAME:VERSION. Your signatures for it go too. Needs a login for that registry (elpis-run secret set registry.HOST) unless it\'s open.'),
    h('label', { class: 'check-row' }, force, h('span', { text: 'Overwrite a release that exists with different content' }))), [
    { label: 'Cancel' },
    { label: 'Push', primary: true, keepOpen: true, onClick: async (d) => {
      const r = await invoke('capsule_push', { reference: ref, to: to.value, force: force.checked });
      try { localStorage.setItem('elpis.registry', to.value.trim()); } catch {}
      d.close();
      const pin = r.reference.replace(/:[^:/]*$/, '') + '@' + r.digest;
      dialog(r.uploaded ? 'Pushed' : 'Already in the registry', h('div', { style: 'display:flex;flex-direction:column;gap:8px' },
        h('div', { class: 'mono', text: r.reference }),
        h('div', { class: 'note', text: r.signatures ? `${r.signatures} signature(s) in the registry` : 'No signatures yet: sign it, then push again.' }),
        h('div', { class: 'note', text: 'Anyone who can write the tag can replace it. Share the pinned form so others get exactly this:' }),
        h('div', { class: 'keyline' }, h('span', { class: 'mono', text: pin, title: pin }), h('button', { class: 'small', text: 'Copy', onclick: () => copyText(pin) }))), [{ label: 'Done', primary: true }]);
    } }]);
}

async function forkDialog(ref) {
  const name = textInput('', 'yourns/your-name');
  const dir = textInput('', '');
  const version = textInput('', '0.1.0');
  let edited = false;
  dir.addEventListener('input', () => { edited = true; });
  name.addEventListener('input', async () => { if (!edited && name.value.includes('/')) dir.value = await invoke('capsule_default_fork_dir', { name: name.value }); });
  dialog('Fork ' + ref, h('div', { style: 'display:flex;flex-direction:column;gap:10px' },
    h('div', { class: 'note', text: 'A fork is a folder you can edit: the same files, a new name, and its lineage pointing back at the original. It starts unsigned.' }),
    formRow('Name for your fork', name, 'NAMESPACE/NAME, different from the original\'s.'),
    formRow('Folder to create', dir, 'Must be new or empty.'),
    formRow('Version (optional)', version)), [
    { label: 'Cancel' },
    { label: 'Fork', primary: true, keepOpen: true, onClick: async (d) => {
      const f = await invoke('capsule_fork', { source: ref, name: name.value, dir: dir.value, version: version.value });
      d.close();
      dialog('Fork created', h('div', { style: 'display:flex;flex-direction:column;gap:8px' },
        h('div', { text: `${f.name}@${f.version}, forked from ${f.parent}` }),
        h('div', { class: 'mono', text: f.dir }), h('div', { class: 'note', text: `${f.files} payload file(s). Edit capsule.yaml and the files, then build it. YAML comments aren't kept.` })), [
        { label: 'Close' },
        { label: 'Build it now', primary: true, onClick: async () => { const row = await invoke('capsule_build', { dir: f.dir, force: false }); toast('built ' + row.reference, 'ok'); selectedRef = row.reference; await loadCapsules(); } }]);
    } }]);
}

$('cbuild').onclick = buildDialog;
$('cpull').onclick = () => pullDialog('');
$('crefresh').onclick = () => { searchHits = null; loadCapsules(); };

// ---------------------------------------------------------------- Trust and keys

async function loadTrust() {
  const root = clear($('trustview'));
  let t, keys;
  try { [t, keys] = await Promise.all([invoke('capsule_trust'), invoke('capsule_keys')]); }
  catch (e) { root.append(h('div', { class: 'err', text: failure(e) })); return; }

  const require = h('input', { type: 'checkbox', checked: t.requireSigned || null });
  require.checked = t.requireSigned;
  require.onchange = async () => {
    try { await invoke('capsule_trust_require', { on: require.checked }); toast(require.checked ? 'Capsules now need a signature from a trusted key to run' : 'unsigned Capsules can run again (you\'re still told they\'re unsigned)', 'ok'); }
    catch (e) { require.checked = !require.checked; toast(failure(e), 'error'); }
  };
  root.append(h('div', null,
    h('h3', { text: 'Policy' }),
    h('label', { class: 'check-row' }, require, h('span', null, 'Only run Capsules signed by a key I trust',
      h('div', { class: 'note', text: 'Without this, an unsigned Capsule still runs after you grant what it asks for, and is marked unsigned. A modified Capsule never runs.' }))),

    h('h3', { text: 'Keys I trust' }),
    h('div', { class: 'note', style: 'margin-bottom:8px', text: 'A signature counts only for the namespaces you list here: a key trusted for one team can\'t vouch for another\'s Capsules. A signature arriving from a registry earns no trust by being there.' }),
    t.keys.length
      ? h('table', null, h('thead', null, h('tr', null, ['Label', 'Public key', 'Namespaces', ''].map((x) => h('th', { text: x })))),
          h('tbody', null, t.keys.map((k) => h('tr', null,
            h('td', null, h('strong', { text: k.name })),
            h('td', null, h('span', { class: 'mono', title: k.publicKey, text: k.publicKey.slice(0, 22) + '…' })),
            h('td', { text: k.namespaces.join(', ') }),
            h('td', null, h('button', { class: 'small danger', text: 'Stop trusting', onclick: async () => {
              if (!(await confirmDialog('Stop trusting', `Stop trusting ${k.name}? Capsules it signed will show as signed but untrusted.`, 'Stop trusting', true))) return;
              try { await invoke('capsule_trust_remove', { name: k.name }); loadTrust(); } catch (e) { toast(failure(e), 'error'); }
            } }))))))
      : h('div', { class: 'note', text: 'You trust no keys yet.' }),
    h('div', { style: 'margin-top:10px' }, h('button', { text: 'Trust a key…', onclick: () => trustDialog('', '') })),

    h('h3', { text: 'My signing keys' }),
    keys.length
      ? keys.map((k) => h('div', { class: 'keyline', style: 'margin:4px 0' }, h('strong', { text: k.name }), h('span', { class: 'mono', title: k.publicKey, text: k.publicKey }),
          h('button', { class: 'small', text: 'Copy public key', onclick: () => copyText(k.publicKey) })))
      : h('div', { class: 'note', text: 'You have no signing keys.' }),
    h('div', { style: 'margin-top:10px' }, h('button', { text: 'New signing key…', onclick: () => keygenDialog() })),
    h('div', { class: 'note', style: 'margin-top:14px', text: 'Signatures use Ed25519; private keys live in Windows Credential Manager. There is no revocation or expiry: if a key is lost or leaked, everyone who trusted it removes it from their list.' })));
}

function trustDialog(publicKey, ref) {
  const ns = ref ? ref.split('/')[0] : '';
  const label = textInput('', 'a name for this key, e.g. alice');
  const key = textInput(publicKey || '', 'ed25519:…');
  const spaces = textInput(ns, 'myteam (comma-separated, or * for all)');
  dialog('Trust a key', h('div', { style: 'display:flex;flex-direction:column;gap:10px' },
    formRow('Label', label), formRow('Public key', key),
    formRow('Namespaces it may vouch for', spaces, 'Required. Capsules are named NAMESPACE/NAME; this key counts only for the namespaces you list.')), [
    { label: 'Cancel' },
    { label: 'Trust', primary: true, onClick: async () => {
      await invoke('capsule_trust_add', { name: label.value, publicKey: key.value, namespaces: spaces.value });
      toast('trusting ' + label.value.trim(), 'ok');
      if (view === 'trust') loadTrust(); else loadCapsules();
    } }]);
}

// ---------------------------------------------------------------- start

(async () => {
  try { const r = await invoke('capsule_default_registry'); if (r) { $('creg').value = r; } else { try { $('creg').value = localStorage.getItem('elpis.registry') || ''; } catch {} } } catch {}
  await refreshBoxes();
})();

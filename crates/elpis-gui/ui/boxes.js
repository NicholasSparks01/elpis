// What the TUI shows for boxes, in the window: live cards (CPU, RAM, uptime, traffic), a box's network activity, rules,
// ports and shared folders, approval prompts, auto-stop, new boxes from a kit, a template or another box, and the
// Agents and Templates views. Loaded after app.js, which owns h(), dialog(), toast() and the sidebar.
//
// As in app.js, anything that came from a box, a registry or the network (host names, kit descriptions) is put on the
// page with textContent through h(), never as markup.

// ---------------------------------------------------------------- formatting

function fmtBytes(n) {
  if (n >= 2 ** 30) return (n / 2 ** 30).toFixed(1) + ' GB';
  if (n >= 2 ** 20) return (n / 2 ** 20).toFixed(1) + ' MB';
  if (n >= 2 ** 10) return (n / 2 ** 10).toFixed(1) + ' KB';
  return n + ' B';
}

function fmtUptime(s) {
  if (s >= 3600) return Math.floor(s / 3600) + 'h' + String(Math.floor((s % 3600) / 60)).padStart(2, '0') + 'm';
  if (s >= 60) return Math.floor(s / 60) + 'm' + String(s % 60).padStart(2, '0') + 's';
  return s + 's';
}

const plural = (n, one, many) => n + ' ' + (n === 1 ? one : many);

// ---------------------------------------------------------------- live cards

const cards = new Map();        // id -> Card from the backend (CPU, RAM, traffic, who is waiting)
const cardOf = (id) => cards.get(id);

/** The lines under a box's name in the sidebar. They are replaced in place on every poll (see updateMetas). */
function cardLines(vm) {
  const box = h('div', { class: 'metas', dataset: { id: vm.id } });
  fillMetas(box, vm);
  return box;
}

function fillMetas(box, vm) {
  const c = cardOf(vm.id);
  const lines = [];
  if (c && c.running) {
    const parts = ['up ' + fmtUptime(c.uptimeSecs), 'CPU ' + c.cpu.toFixed(1) + '%', 'RAM ' + fmtBytes(c.ramBytes)];
    lines.push(h('div', { class: 'meta net', text: parts.join(' · ') }));
    if (c.stopsInSecs != null) lines.push(h('div', { class: 'meta' }, h('span', { class: 'warn', text: 'stops in ' + c.stopsInSecs + 's' })));
    else if (c.sessions) lines.push(h('div', { class: 'meta', text: plural(c.sessions, 'terminal', 'terminals') + ' open' }));
  } else if (c && c.elsewhere) {
    lines.push(h('div', { class: 'meta' }, h('span', { class: 'warn', text: 'in use elsewhere' })));
  } else {
    lines.push(h('div', { class: 'path', text: vm.path }));
  }
  if (c && c.net) {
    const asking = c.waiting.length;
    lines.push(h('div', { class: 'meta net' },
      `net ${plural(c.net.hosts, 'host', 'hosts')} · ${c.net.hits} hits`,
      c.net.blocked ? h('span', { class: 'bad', text: ` · ${c.net.blocked} blocked` }) : null,
      asking ? h('span', { class: 'warn', text: ` · ⚠ ${asking} asking` }) : null));
  }
  box.replaceChildren(...lines);
}

function updateMetas() {
  for (const box of document.querySelectorAll('#vmlist .metas')) {
    const vm = vms.get(box.dataset.id);
    if (vm) fillMetas(box, vm);
    const c = cardOf(box.dataset.id);
    box.closest('.vm')?.classList.toggle('elsewhere', !!(c && c.elsewhere));
  }
}

let polling = false;
async function pollCards() {
  if (polling) return;
  polling = true;
  try {
    const list = await invoke('box_cards');
    cards.clear();
    for (const c of list) {
      cards.set(c.id, c);
      const vm = vms.get(c.id);
      if (vm && vm.running !== c.running) { vm.running = c.running; renderList(); updateButtons(); }
    }
    updateMetas();
    renderBoxInfo();
    refreshPane();
    checkApprovals();
  } catch { /* the next poll tries again */ }
  finally { polling = false; }
}

function startPolling() {
  pollCards();
  setInterval(() => { if (!document.hidden) pollCards(); }, 2000);
  document.addEventListener('visibilitychange', () => { if (!document.hidden) pollCards(); });
}

listen('auto-stopped', (e) => { toast(`${e.payload} stopped: no terminal was open for a while (Auto-stop on the Box tab keeps a box running)`); pollCards(); });

// ---------------------------------------------------------------- the Box view: info line, buttons, sub-tabs

let sub = 'log';
let paneFor = null;             // `${box}/${sub}` the pane's skeleton was built for
let paneList = null;            // the part of the pane that is re-filled

function renderBoxInfo() {
  const el = clear($('boxinfo'));
  const vm = vms.get(current);
  const c = cardOf(current);
  if (!vm || !c) { el.append(h('span', { text: 'Select a box on the left, or press + New to make one.' })); updateAutoStop(); return; }
  const item = (k, v) => h('span', null, k + ' ', h('b', { text: v }));
  el.append(item('state', c.running ? 'running' : c.elsewhere ? 'in use elsewhere' : 'stopped'));
  if (c.running) {
    el.append(item('up', fmtUptime(c.uptimeSecs)), item('CPU', c.cpu.toFixed(1) + '%'), item('RAM', fmtBytes(c.ramBytes)));
    if (c.shimPid) el.append(item('shim pid', String(c.shimPid)));
  }
  el.append(item('size', `${c.cpus ?? '?'} vCPU · ${c.memoryMb ? c.memoryMb + ' MB' : '? MB'}`), item('from', c.source || 'prepared'));
  if (c.docker) el.append(h('span', { class: 'tb good', text: 'docker' }));
  if (c.elsewhere) el.append(h('span', { class: 'warn', style: 'color:var(--yellow)', text: 'its disk is open in a VM this window did not start; end that VM first' }));
  updateAutoStop();
}

function updateAutoStop() {
  const c = cardOf(current);
  const b = $('autostop');
  b.disabled = !c;
  b.textContent = c && c.autoStopSecs == null ? 'Keeps running' : c ? `Auto-stop: ${c.autoStopSecs}s` : 'Auto-stop';
  b.title = !c ? '' : c.autoStopSecs == null
    ? 'This box keeps running until you stop it. Click to stop it automatically after its last terminal closes.'
    : `Stops ${c.autoStopSecs} seconds after its last terminal closes. Click to keep it running instead.`;
}

function updateBoxButtons() {
  const vm = vms.get(current);
  $('copybox').disabled = !vm;
  $('savetpl').disabled = !vm || vm.running;
  $('delbox').disabled = !vm || vm.running;
  updateAutoStop();
}

function boxSelected() { paneFor = null; renderBoxInfo(); refreshPane(); }

$('autostop').onclick = async () => {
  if (!current) return;
  try {
    const secs = await invoke('auto_stop_toggle', { id: current });
    toast(secs == null ? 'keeps running until you stop it' : `auto-stop on: stops ${secs} s after its last terminal closes`, 'ok');
  } catch (e) { toast(failure(e), 'error'); }
  pollCards();
};

for (const b of document.querySelectorAll('#subtabs button[data-sub]')) b.onclick = () => setSub(b.dataset.sub);

function setSub(name) {
  sub = name;
  for (const b of document.querySelectorAll('#subtabs button[data-sub]')) {
    const on = b.dataset.sub === name;
    b.classList.toggle('active', on);
    b.setAttribute('aria-selected', on);
  }
  $('log').hidden = name !== 'log';
  $('pane').hidden = name === 'log';
  paneFor = null;
  refreshPane();
}

const PANES = {
  net: { build: buildNet, fill: fillNet },
  rules: { build: buildRules, fill: fillRules },
  ports: { build: buildPorts, fill: fillPorts },
  files: { build: buildFiles, fill: fillFiles },
};

/** Build the pane's skeleton when the box or tab changed, then re-fill its list (cheap when nothing changed). */
function refreshPane() {
  const pane = PANES[sub];
  if (!pane || view !== 'box') return;
  const root = $('pane');
  if (!current) { clear(root).append(emptyState('Pick a box')); paneFor = null; return; }
  if (paneFor !== current + '/' + sub) {
    paneFor = current + '/' + sub;
    paneList = h('div', { class: 'list' });
    clear(root).append(...pane.build(), paneList);
  }
  const id = current, list = paneList;
  Promise.resolve(pane.fill(id, list)).catch((e) => { if (id === current) setList(list, [h('div', { class: 'err pad', text: failure(e) })]); });
}

/** Replace a list's content only when its data changed, so a poll doesn't undo a click or a text selection. */
function setList(list, nodes, sig) {
  if (sig !== undefined) { if (list.dataset.sig === sig) return false; list.dataset.sig = sig; } else delete list.dataset.sig;
  list.replaceChildren(...nodes);
  return true;
}

async function act(promise, then) {
  try { const text = await promise; if (text) toast(text, 'ok'); }
  catch (e) { toast(failure(e), 'error'); }
  if (then) then();
}

const th = (t, cls) => h('th', { text: t, class: cls });
const table = (head, rows) => h('table', null, h('thead', null, h('tr', null, head)), h('tbody', null, rows));

// ---- Network activity

function buildNet() { return []; }

async function fillNet(id, list) {
  const v = await invoke('net_activity', { id });
  if (id !== current) return;
  if (!v.hasStats) {
    setList(list, [emptyState(v.running ? 'Waiting for traffic…' : 'Not started yet', h('div', { text: 'Start the box to see which hosts it talks to, and to allow or block them.' }))], 'none');
    return;
  }
  if (!v.rows.length) { setList(list, [emptyState('No traffic yet', h('div', { text: v.running ? 'Nothing has left the box so far.' : 'This box made no connections last time it ran.' }))], 'empty' + v.running); return; }
  const rows = v.rows.map((r) => {
    const decision = r.decision === 'waiting' ? h('span', { class: 'ask', text: 'waiting for you' })
      : r.decision === 'none' ? h('span', { class: 'dimtext', text: '—' })
      : h('span', null, h('b', { class: r.decision === 'blocked' ? 'deny' : 'allow', text: r.decision }), h('span', { class: 'dimtext', text: ' · ' + (r.rule || 'default') }));
    return h('tr', null,
      h('td', { text: r.key }),
      h('td', { class: 'num', text: String(r.hits) }),
      h('td', { class: 'num allow', text: String(r.allowed) }),
      h('td', { class: 'num' + (r.blocked ? ' deny' : ' dimtext'), text: String(r.blocked) }),
      h('td', { class: 'num', text: fmtBytes(r.sent) }),
      h('td', { class: 'num', text: fmtBytes(r.received) }),
      h('td', { class: 'dimtext', text: r.last }),
      h('td', null, decision),
      h('td', { class: 'act' }, r.host && [
        h('button', { class: 'small danger', text: 'Block', title: 'Block ' + r.host + ' for new connections', onclick: () => act(invoke('net_mark_host', { id, key: r.key, action: 'deny' }), refreshPane) }),
        h('button', { class: 'small', text: 'Allow', title: 'Allow ' + r.host + ' for new connections', onclick: () => act(invoke('net_mark_host', { id, key: r.key, action: 'allow' }), refreshPane) }),
      ]));
  });
  setList(list, [table(
    [th('Host'), th('Hits', 'num'), th('Allowed', 'num'), th('Blocked', 'num'), th('Sent', 'num'), th('Received', 'num'), th('Last'), th('Decision'), th('')], rows)], JSON.stringify(v));
}

// ---- Network rules

const ACTIONS = ['allow', 'ask', 'deny'];

function buildRules() {
  const pattern = textInput('', 'example.com   *.example.com   **   10.0.0.0/8   host:443');
  const action = h('select', { 'aria-label': 'Action' }, h('option', { value: 'allow', text: 'allow' }), h('option', { value: 'deny', text: 'deny' }));
  const id = current;
  const add = async () => {
    const p = pattern.value.trim();
    if (!p) return;
    try { await invoke('policy_add', { id, pattern: p, action: action.value }); pattern.value = ''; toast(`${action.value} ${p.toLowerCase()} — applies to new connections now`, 'ok'); refreshPane(); }
    catch (e) { toast(failure(e), 'error'); }
  };
  pattern.addEventListener('keydown', (e) => { if (e.key === 'Enter') add(); });
  return [h('div', { class: 'formline' }, pattern, action, h('button', { class: 'primary', text: 'Add rule', onclick: add }),
    h('span', { class: 'note', text: 'Deny beats allow. Changes apply to new connections at once, even while the box runs.' }))];
}

async function fillRules(id, list) {
  const p = await invoke('policy_get', { id });
  if (id !== current) return;
  const seg = h('div', { class: 'seg', role: 'group', 'aria-label': 'Default' }, ACTIONS.map((a) => h('button', {
    class: (p.default === a ? 'on ' : '') + a, text: a,
    title: { allow: 'Hosts no rule covers are allowed', ask: 'Hosts no rule covers wait for you to allow or deny them', deny: 'Hosts no rule covers are blocked' }[a],
    onclick: () => act(invoke('policy_default', { id, action: a }).then(() => `default is now ${a}`), refreshPane),
  })));
  const head = h('div', { class: 'formline' }, h('b', { text: 'Default' }), seg,
    h('span', { class: 'note', text: p.default === 'ask' ? 'Each new host waits for your answer.' : 'What happens to a host no rule covers.' }));
  const body = p.rules.length
    ? table([th('ID'), th('Action'), th('Pattern'), th('Added'), th('')], p.rules.map((r) => h('tr', null,
      h('td', { class: 'dimtext', text: String(r.id) }),
      h('td', null, h('b', { class: r.action === 'deny' ? 'deny' : 'allow', text: r.action })),
      h('td', { text: r.pattern }),
      h('td', { class: 'dimtext', text: r.added }),
      h('td', { class: 'act' }, h('button', { class: 'small danger', text: 'Remove', onclick: () => act(invoke('policy_remove', { id, rule: r.id }).then(() => `removed rule ${r.id} (${r.action} ${r.pattern})`), refreshPane) })))))
    : emptyState('No rules', h('div', { text: `Every destination gets the default (${p.default}). Add one above, or press Block or Allow on a host under Network activity.` }),
      h('div', { class: 'note', style: 'margin-top:8px', text: 'Patterns: example.com · *.example.com · ** · 1.2.3.4 · 10.0.0.0/8 · host:443. The Windows host (172.17.0.0) is only reachable by a rule that names host.docker.internal.' }));
  setList(list, [head, body], JSON.stringify(p));
}

// ---- Ports

function buildPorts() {
  const text = textInput('', '8080:80   3000   0.0.0.0:8080:80');
  const id = current;
  const add = async () => {
    const t = text.value.trim();
    if (!t) return;
    try { toast(await invoke('port_add', { id, text: t }), 'ok'); text.value = ''; refreshPane(); }
    catch (e) { toast(failure(e), 'error'); }
  };
  text.addEventListener('keydown', (e) => { if (e.key === 'Enter') add(); });
  return [h('div', { class: 'formline' }, text, h('button', { class: 'primary', text: 'Publish', onclick: add }),
    h('span', { class: 'note', text: 'HOST:BOX, just BOX, or IP:HOST:BOX (0.0.0.0 for your whole network). Applies at once when the box runs.' }))];
}

async function fillPorts(id, list) {
  const ports = await invoke('ports_list', { id });
  if (id !== current) return;
  if (!ports.length) {
    setList(list, [emptyState('No published ports', h('div', { text: 'A server in the box, or a docker run -p container in it, is then at http://127.0.0.1:HOST on Windows once you publish its port above.' }))], '[]');
    return;
  }
  const status = (s) => ({ live: h('b', { class: 'allow', text: 'live' }), failed: h('span', { class: 'deny', text: 'not published (port in use?)' }), 'on-start': h('span', { class: 'dimtext', text: 'on start' }) }[s]);
  setList(list, [table([th('On Windows'), th('In the box'), th('Status'), th('')], ports.map((p) => h('tr', null,
    h('td', null, h('b', { text: p.url }), ' ', h('button', { class: 'small', text: 'Copy', onclick: () => copyText(p.url) })),
    h('td', { text: ':' + p.guest }),
    h('td', null, status(p.status)),
    h('td', { class: 'act' }, h('button', { class: 'small danger', text: 'Remove', onclick: () => act(invoke('port_remove', { id, host: p.host }), refreshPane) })))))], JSON.stringify(ports));
}

// ---- Files

function buildFiles() { return [h('div', { class: 'formline' }, h('span', { class: 'note', text: 'Windows folders shared into the box. Changing read or write applies the next time the box starts.' }))]; }

async function fillFiles(id, list) {
  const rules = await invoke('fs_list', { id });
  if (id !== current) return;
  if (!rules.length) { setList(list, [emptyState('No shared folders', h('div', { text: 'This box shares no Windows folders.' }))], '[]'); return; }
  const mark = (on) => h('b', { class: on ? 'allow' : 'deny', text: on ? '✓' : '✗' });
  setList(list, [table([th('In the box'), th('Windows folder'), th('Read'), th('Write'), th('')], rules.map((r) => h('tr', null,
    h('td', null, h('b', { text: r.target })),
    h('td', { class: 'dimtext', text: r.source }),
    h('td', null, mark(true)),
    h('td', null, mark(!r.readOnly)),
    h('td', { class: 'act' }, h('button', { class: 'small', text: r.readOnly ? 'Make writable' : 'Make read-only', onclick: () => act(invoke('fs_toggle', { id, target: r.target }), refreshPane) })))))], JSON.stringify(rules));
}

// ---------------------------------------------------------------- approvals: a box waiting on a host no rule covers

let approving = false;

function checkApprovals() {
  if (approving) return;
  for (const c of cards.values()) {
    if (c.running && c.waiting.length) { askApproval(c.id, c.waiting[0]); return; }
  }
}

function askApproval(id, w) {
  approving = true;
  let answered = false;
  const answer = async (a) => {
    answered = true;
    try { toast(await invoke('net_answer', { id, host: w.host, answer: a }), a === 'always' || a === 'once' ? 'ok' : 'info'); }
    catch (e) { toast(failure(e), 'error'); }
    pollCards();
  };
  dialog('Allow this connection?', h('div', null,
    h('div', null, h('b', { text: id }), ' wants to reach ', h('b', { text: w.host }), w.ports.length ? ` (port ${w.ports.join(', ')})` : '', '.'),
    h('div', { class: 'note', style: 'margin-top:6px', text: `${plural(w.attempts, 'attempt', 'attempts')} so far. No rule covers it. “Allow always” and “Deny always” save a rule; “Allow once” lasts until the box stops; “Not now” refuses it and asks again next time.` })), [
    { label: 'Not now', onClick: () => answer('later') },
    { label: 'Deny always', danger: true, onClick: () => answer('deny') },
    { label: 'Allow once', onClick: () => answer('once') },
    { label: 'Allow always', primary: true, onClick: () => answer('always') },
  ], {
    // closing it any other way (Esc, a click outside) is "not now"
    onClose: () => { approving = false; if (!answered) invoke('net_answer', { id, host: w.host, answer: 'later' }).catch(() => {}); },
  });
}

// ---------------------------------------------------------------- new boxes, templates, delete

/** Follow `job-progress` events while a dialog is open. */
function followProgress(api) {
  const un = listen('job-progress', (e) => api.setProgress(e.payload));
  return () => un.then((f) => f()).catch(() => {});
}

async function newBoxDialog(kind, name) {
  let d;
  try { d = await invoke('new_box_defaults', { kind, name, selected: current }); } catch (e) { toast(failure(e), 'error'); return; }
  const nameIn = textInput(d.name, 'mybox');
  const ws = textInput(d.workspace, 'C:\\Users\\me\\project');
  const docker = h('select', { 'aria-label': 'Docker' },
    h('option', { value: 'auto', text: kind === 'kit' ? 'Automatic (on for sbx agent images)' : 'Automatic (same as the source box)' }),
    h('option', { value: 'on', text: 'On (10 GB disk at /var/lib/docker)' }),
    h('option', { value: 'off', text: 'Off' }));
  const from = { kit: `the ${name} kit (pulls docker.io/sbx/${name}-image if needed)`, template: `template ${name}`, copy: `a copy of ${name} (same image, empty disk)` }[kind];
  let stop = () => {};
  const api = dialog('New box', h('div', { style: 'display:flex;flex-direction:column;gap:10px' },
    h('div', { class: 'note', text: 'From ' + from }),
    formRow('Name', nameIn),
    d.wantsWorkspace && formRow('Workspace', ws, 'A Windows folder shared into the box, like sbx run’s PATH.'),
    formRow('Docker engine', docker)), [
    { label: 'Cancel' },
    { label: 'Create', primary: true, onClick: async (dlg) => {
      stop = followProgress(dlg);
      const made = await invoke('box_create', { source: { kind, name }, name: nameIn.value, workspace: ws.value, docker: docker.value === 'auto' ? null : docker.value === 'on' });
      toast(`created ${made}: press Start, then Agent…`, 'ok');
      await refreshBoxes(made);
      showView('box');
    } }], { onClose: () => stop() });
  return api;
}

$('newbox').onclick = () => { showView('agents'); toast('Pick an agent and press New box…, or use a template or Copy…', 'info'); };
$('copybox').onclick = () => { if (current) newBoxDialog('copy', current); };

$('savetpl').onclick = () => {
  const vm = vms.get(current);
  if (!vm) return;
  const id = vm.id;
  const name = textInput('', 'mytemplate');
  dialog('Save as template', h('div', { style: 'display:flex;flex-direction:column;gap:10px' },
    h('div', { class: 'note', text: `Freeze ${id}'s disk so new boxes start with everything installed in it. The box must be stopped.` }),
    formRow('Template name', name)), [
    { label: 'Cancel' },
    { label: 'Save', primary: true, onClick: async () => { toast(await invoke('template_save', { id, name: name.value }), 'ok'); loadTemplates(); } }]);
};

$('delbox').onclick = async () => {
  const vm = vms.get(current);
  if (!vm) return;
  if (!(await confirmDialog('Delete box', `Delete ${vm.id} and its disk? Shared Windows folders and templates are kept. This cannot be undone.`, 'Delete', true))) return;
  try { toast(await invoke('box_delete', { id: vm.id }), 'ok'); current = null; await refreshBoxes(); }
  catch (e) { toast(failure(e), 'error'); }
};

// ---------------------------------------------------------------- Agents (kits)

let kits = [];
let kitFilter = '';
let kitsLoaded = false;

async function loadKits(refresh = false) {
  $('kinfo').textContent = refresh || !kitsLoaded ? 'listing kits on Docker Hub…' : '';
  try { kits = await invoke('kit_catalog', { refresh }); kitsLoaded = true; $('kinfo').textContent = `${plural(kits.length, 'kit', 'kits')} on Docker Hub (docker.io/sbx)`; }
  catch (e) { $('kinfo').textContent = ''; toast('couldn\'t list kits: ' + failure(e), 'error'); }
  renderKits();
}

function renderKits() {
  const el = clear($('kittable'));
  const words = kitFilter.toLowerCase().split(/\s+/).filter(Boolean);
  const shown = kits.filter((k) => words.every((w) => (k.name + ' ' + k.description).toLowerCase().includes(w)));
  if (!kits.length) { el.append(emptyState('No kit list yet', h('div', { text: 'Press Refresh to fetch it from Docker Hub.' }))); return; }
  if (!shown.length) { el.append(emptyState('Nothing matches', h('div', { text: `No kit matches “${kitFilter}”.` }))); return; }
  el.append(table([th('Kit'), th('Kind'), th('Pulled'), th('Updated'), th('Description'), th('')], shown.map((k) => h('tr', null,
    h('td', null, h('b', { text: k.name })),
    h('td', null, h('span', { class: k.agent ? 'allow' : 'dimtext', text: k.agent ? 'agent' : 'add-on' })),
    h('td', null, k.pulled ? h('b', { class: 'allow', text: '✓' }) : ''),
    h('td', { class: 'dimtext', text: k.updated }),
    h('td', { class: 'desc', text: k.description, title: k.description }),
    h('td', { class: 'act' },
      h('button', { class: 'small primary', text: 'New box…', disabled: !k.agent, title: k.agent ? 'Make a box from this agent' : 'An add-on, not an agent: add it to an agent with elpis-run run AGENT --with ' + k.name, onclick: () => newBoxDialog('kit', k.name) }),
      h('button', { class: 'small', text: 'Pull', disabled: !k.agent, title: 'Download the image now, without making a box', onclick: () => pullKit(k.name) }))))));
}

async function pullKit(name) {
  const stop = followProgress({ setProgress: (t) => ($('kprogress').textContent = t) });
  try { toast(await invoke('kit_pull', { kit: name }), 'ok'); }
  catch (e) { toast(failure(e), 'error'); }
  stop();
  $('kprogress').textContent = '';
  loadKits();
}

$('kq').addEventListener('input', () => { kitFilter = $('kq').value; renderKits(); });
$('krefresh').onclick = () => loadKits(true);
$('helpkits').onclick = () => helpTopic('gui#agents');

// ---------------------------------------------------------------- Templates

async function loadTemplates() {
  const el = clear($('tpltable'));
  let list;
  try { list = await invoke('template_list'); } catch (e) { toast(failure(e), 'error'); return; }
  if (!list.length) {
    el.append(emptyState('No templates yet', h('div', { text: 'Set a box up the way you like (sign in, install tools), stop it, and use Save as template on the Box tab. New boxes from a template start with all of that, each with its own empty disk on top.' })));
    return;
  }
  el.append(table([th('Template'), th('Saved from'), th('Made from'), th('Size', 'num'), th('Saved'), th('')], list.map((t) => h('tr', null,
    h('td', null, h('b', { text: t.name })),
    h('td', { text: t.from }),
    h('td', { class: 'dimtext', text: t.source }),
    h('td', { class: 'num', text: fmtBytes(t.size) }),
    h('td', { class: 'dimtext', text: t.saved }),
    h('td', { class: 'act' },
      h('button', { class: 'small primary', text: 'New box…', onclick: () => newBoxDialog('template', t.name) }),
      h('button', { class: 'small danger', text: 'Remove', onclick: async () => {
        if (!(await confirmDialog('Remove template', `Remove template ${t.name}? Boxes made from it keep working.`, 'Remove', true))) return;
        act(invoke('template_remove', { name: t.name }), loadTemplates);
      } }))))));
}

$('tplsave').onclick = () => {
  if (!current) return toast('select a box first (on the Box tab)', 'info');
  if (vms.get(current)?.running) return toast('stop the box before saving it as a template', 'info');
  $('savetpl').click();
};
$('helptpl').onclick = () => helpTopic('gui#templates');

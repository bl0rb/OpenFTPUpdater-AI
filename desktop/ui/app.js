// OpenFTPUpdater-AI UI: renders the view the Rust side sends and calls its commands. No framework, no build step.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const STEPS = [
  { id: 'precheck', title: 'Precheck', desc: 'Prüft Repo, Werkzeuge, neue Releases, SFTP-Zugang und Live-Seite. Ändert nichts.' },
  { id: 'update', title: 'Update', desc: 'Claude passt die Seite per Skill an neue Releases an, danach Build und Commit.' },
  { id: 'upload', title: 'Upload', desc: 'Frischer Build, Push, dann nur geänderte Dateien per SFTP auf den Server.' },
  { id: 'verify', title: 'Verify', desc: 'Vergleicht die Live-Seite Datei für Datei mit dem hochgeladenen Build.' },
];
const STATUS_TEXT = { idle: 'Bereit', running: 'Läuft', ok: 'OK', warning: 'Hinweis', error: 'Fehler' };
// Claude Code model aliases; each follows the newest model of its family.
const MODELS = [['opus', 'Opus'], ['sonnet', 'Sonnet'], ['haiku', 'Haiku'], ['fable', 'Fable']];
const modelName = (alias) => MODELS.find(([id]) => id === alias)?.[1] ?? alias ?? 'Claude-Code-Standard';

/** Model dropdown; the first entry stands for the default that applies without a choice. */
function modelSelect(value, fallback, onChange, id) {
  return h('select', { id, class: 'model-select', title: 'Modell für diesen Claude-Lauf', onchange: (e) => onChange(e.target.value) },
    h('option', { value: '', selected: !value }, `Standard (${modelName(fallback)})`),
    MODELS.map(([alias, name]) => h('option', { value: alias, selected: value === alias }, name)));
}

let view = null;
let selected = 'precheck';
let lastRunning = null;
let hostKeyOpen = false;

// MARK: Helpers

const PROPS = new Set(['value', 'checked', 'disabled', 'selected', 'hidden', 'required']);

function h(tag, props, ...children) {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(props ?? {})) {
    if (value == null || value === false) continue;
    if (key.startsWith('on')) el.addEventListener(key.slice(2), value);
    else if (key === 'class') el.className = value;
    else if (PROPS.has(key)) el[key] = value;
    else el.setAttribute(key, value === true ? '' : value);
  }
  for (const child of children.flat(Infinity)) {
    if (child == null || child === false) continue;
    el.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return el;
}

const ICONS = {
  play: '<path d="M7 5v14l11-7z"/>',
  stop: '<rect x="6" y="6" width="12" height="12" rx="2"/>',
  gear: '<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z"/>',
  external: '<path d="M14 4h6v6M20 4l-9 9M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5"/>',
  folder: '<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>',
  forward: '<path d="M5 12h14M13 6l6 6-6 6"/>',
  close: '<path d="M6 6l12 12M18 6 6 18"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  refresh: '<path d="M21 12a9 9 0 1 1-3-6.7L21 8"/><path d="M21 3v5h-5"/>',
};

function icon(name) {
  const span = document.createElement('span');
  span.innerHTML = `<svg class="icon" viewBox="0 0 24 24" aria-hidden="true">${ICONS[name]}</svg>`;
  return span.firstElementChild;
}

function button(label, onClick, { iconName, cls, disabled, title, iconOnly } = {}) {
  return h('button', {
    type: 'button', class: [cls, iconOnly && 'icon-only'].filter(Boolean).join(' ') || null, disabled,
    title: title ?? (iconOnly ? label : null), 'aria-label': iconOnly ? label : null,
    onclick: (e) => { e.stopPropagation(); onClick?.(e); },
  }, iconName && icon(iconName), !iconOnly && label);
}

const formatTime = (secs) => new Date(secs * 1000).toLocaleString('de-DE', { dateStyle: 'medium', timeStyle: 'short' });

async function call(command, args) {
  try {
    return await invoke(command, args);
  } catch (error) {
    showAlert(String(error));
    return null;
  }
}

async function refresh() {
  setView(await invoke('get_view'));
}

// MARK: Render

function setView(next) {
  view = next;
  if (view.running && view.running !== lastRunning) {
    // Follow the step that just started.
    selected = view.running;
    clearLog();
  }
  lastRunning = view.running;
  renderToolbar();
  renderBanners();
  renderMain();
  if (view.pendingHostKey && !hostKeyOpen) hostKeyDialog();
}

function renderToolbar() {
  document.getElementById('toolbar').replaceChildren(
    h('div', { class: 'brand' },
      h('img', { src: 'icon.svg', alt: '' }),
      h('div', null,
        h('h1', null, 'OpenFTPUpdater-AI'),
        h('div', { class: 'project' }, view.site ? `${view.site.name} · ${view.projectDisplay}` : view.projectDisplay || 'Kein Projekt eingerichtet'))),
    h('span', { class: 'spacer' }),
    view.site && button(view.site.url.replace(/^https?:\/\//, ''), () => call('open_url', { url: view.site.url }), { iconName: 'external', cls: 'link', title: 'Live-Seite öffnen' }),
    button('Einrichtung', settingsDialog, { iconName: 'gear', disabled: !!view.running }));
}

function banner(kind, text, actions = []) {
  return h('div', { class: `banner ${kind}` }, h('div', { class: 'text selectable' }, text),
    actions.map(([label, action]) => button(label, action)));
}

function renderBanners() {
  const items = [];
  if (view.error) items.push(banner('error', view.error, [['Schließen', () => call('dismiss')]]));
  if (view.notice) items.push(banner('info', view.notice, [['Schließen', () => call('dismiss')]]));
  if (view.config.projectDir && view.siteError) items.push(banner('warning', `Projekt: ${view.siteError}`, [['Einrichtung', settingsDialog]]));
  if (view.site && !view.sftpReady) items.push(banner('warning', 'SFTP ist noch nicht eingerichtet: Upload ist erst danach möglich.', [['Einrichtung', settingsDialog]]));
  document.getElementById('banners').replaceChildren(...items);
}

let skeleton = null;

function renderMain() {
  const main = document.getElementById('main');
  if (!view.config.projectDir) {
    skeleton = null;
    main.replaceChildren(h('div', { class: 'empty' },
      h('img', { src: 'icon.svg', alt: '' }),
      h('h2', null, 'Willkommen'),
      h('p', { class: 'muted' }, 'Wähle den Projektordner deiner Website (mit .openftpupdater/site.json) und den SFTP-Zugang. Danach prüft, aktualisiert und veröffentlicht die App die Seite in vier Schritten.'),
      button('Einrichten', settingsDialog, { cls: 'primary' })));
    return;
  }
  if (!skeleton) {
    // Built once, so the instruction field keeps focus and the log keeps its scroll position.
    skeleton = {
      steps: h('section', { class: 'steps' }),
      tasks: h('div', { class: 'tasks' }),
      instructions: h('textarea', { id: 'instructions', rows: 2, placeholder: 'z. B. „Ergänze im nudgit-Abschnitt den Hinweis auf die Chrome-Erweiterung“ – leer lassen für ein reines Release-Update' }),
      runAll: null,
      detailHead: h('div', { class: 'detail-head' }),
      detailResult: h('div', { class: 'detail-result' }),
      log: h('pre', { class: 'log', id: 'log' }),
    };
    skeleton.runAll = button('Alle Schritte', () => call('run_all', { instructions: skeleton.instructions.value }),
      { iconName: 'forward', title: 'Precheck, Update (wenn nötig), Upload und Verify nacheinander; stoppt beim ersten Fehler' });
    main.replaceChildren(
      skeleton.steps,
      h('section', { class: 'task-panel' },
        h('div', { class: 'task-panel-head' }, h('strong', null, 'Aufgaben beim Update'),
          h('span', { class: 'muted small' }, 'Jede Aufgabe läuft als eigener Claude-Lauf mit ihrem Skill, in dieser Reihenfolge.')),
        skeleton.tasks),
      h('div', { class: 'run-row' },
        h('div', { class: 'field' }, h('label', { for: 'instructions' }, 'Zusätzliche Anweisung an Claude (optional, gilt für Update)'), skeleton.instructions),
        skeleton.runAll),
      h('section', { class: 'detail' }, skeleton.detailHead, skeleton.detailResult, skeleton.log));
    loadLog(selected);
  }
  skeleton.runAll.disabled = !!view.running || !view.site;
  skeleton.instructions.disabled = !!view.running;
  renderSteps();
  renderTasks();
  renderDetail();
}

function renderTasks() {
  skeleton.tasks.replaceChildren(...view.tasks.map((t) => {
    const save = (enabled, model) => call('set_task', { id: t.id, enabled, model });
    return h('div', { class: `task ${t.enabled ? 'on' : ''}` },
      h('label', { class: 'task-head' },
        h('input', { type: 'checkbox', checked: t.enabled, disabled: !!view.running, onchange: (e) => save(e.target.checked, t.chosenModel) }),
        h('strong', null, t.label)),
      t.description && h('div', { class: 'muted small' }, t.description),
      h('div', { class: 'task-meta' },
        modelSelect(t.chosenModel, t.defaultModel, (model) => save(t.enabled, model), `task-model-${t.id}`),
        t.effort && h('span', { class: 'badge', title: 'Effort aus site.json' }, `Effort ${t.effort}`),
        t.skill && h('span', { class: 'badge mono', title: 'Skill im Projekt' }, t.skill)));
  }));
}

function ready(step) {
  if (!view.site) return step === 'precheck' && !!view.config.projectDir;
  if (step === 'upload') return view.sftpReady;
  return true;
}

/** The step that makes sense next, highlighted as the primary button. */
function suggested() {
  const s = view.steps;
  if (view.running) return null;
  if (s.precheck.status === 'idle') return 'precheck';
  if (view.outdated?.length) return 'update';
  if (s.update.status === 'ok' && s.upload.status !== 'ok') return 'upload';
  if (s.upload.status === 'ok' && s.verify.status === 'idle') return 'verify';
  return null;
}

function statusChip(status, stale = false) {
  if (stale) return h('span', { class: 'chip idle', title: 'Stand vor dem letzten Update oder Upload' }, 'Veraltet');
  return h('span', { class: `chip ${status}` }, status === 'running' && h('span', { class: 'spinner' }), STATUS_TEXT[status]);
}

function renderSteps() {
  const next = suggested();
  skeleton.steps.replaceChildren(...STEPS.map((step, i) => {
    const state = view.steps[step.id];
    const running = view.running === step.id;
    const action = h('div', { class: 'step-actions' }, running
      ? button('Abbrechen', () => invoke('cancel'), { iconName: 'stop', cls: 'danger' })
      : button('Starten', () => call('run_step', { step: step.id, instructions: skeleton.instructions.value }),
        { iconName: 'play', cls: next === step.id ? 'primary' : null, disabled: !!view.running || !ready(step.id) }),
      step.id === 'update' && !running && button('Neues Produkt …', addProductDialog,
        { iconName: 'plus', disabled: !!view.running || !view.site, title: 'Ein Repository als neues Produkt auf der Website aufnehmen' }));
    return h('div', {
      class: `step ${selected === step.id ? 'selected' : ''}`, role: 'button', tabindex: '0',
      onclick: () => selectStep(step.id), onkeydown: (e) => { if (e.key === 'Enter') selectStep(step.id); },
    },
      h('div', { class: 'step-head' }, h('span', { class: 'step-number' }, i + 1), h('h2', null, step.title), h('span', { class: 'spacer' }), statusChip(state.status, state.stale)),
      h('div', { class: 'desc' }, step.desc),
      h('div', { class: `summary ${state.stale ? 'muted' : state.status === 'error' ? 'red' : state.status === 'warning' ? 'orange' : 'muted'}` },
        state.stale ? 'Nach Update/Upload erneut prüfen' : state.summary || ' '),
      action);
  }));
}

function selectStep(step) {
  if (selected === step) return;
  selected = step;
  renderSteps();
  renderDetail();
  loadLog(step);
}

function renderDetail() {
  const step = STEPS.find((s) => s.id === selected);
  const state = view.steps[selected];
  skeleton.detailHead.replaceChildren(
    h('h3', null, step.title), statusChip(state.status, state.stale),
    h('span', { class: 'muted small' }, state.finishedAt ? `zuletzt ${formatTime(state.finishedAt)}` : ''));
  skeleton.detailResult.replaceChildren(...({
    precheck: precheckResult, update: updateResult, upload: uploadResult, verify: verifyResult,
  }[selected]()).filter(Boolean));
}

function precheckResult() {
  if (!view.checks.length) return [];
  const stale = view.steps.precheck.stale;
  const outdated = view.outdated?.length
    ? h('div', { class: 'facts' }, h('span', null, h('strong', null, 'Neue Releases: '),
      view.outdated.map((o) => `${o.name} ${o.documented} → ${o.latestTag.replace(/^v/, '')}`).join(', ')))
    : null;
  return [
    stale && h('div', { class: 'note orange' }, 'Dieses Ergebnis stammt von vor dem letzten Update oder Upload (z. B. Remote-Stand und Releases). Precheck erneut starten für den aktuellen Stand.'),
    outdated && !stale ? outdated : null,
    h('div', { class: `checks ${stale ? 'stale' : ''}` }, view.checks.map((c) => [
    h('span', { class: `status-dot ${c.status}` }), h('span', { class: 'label' }, c.label), h('span', { class: 'detail-text selectable' }, c.detail),
  ]))];
}

function updateResult() {
  const site = view.site;
  const items = [];
  if (site) {
    items.push(h('div', { class: 'facts' },
      h('span', null, h('strong', null, 'Aufgaben: '), view.tasks.filter((t) => t.enabled).map((t) => t.label).join(', ') || 'keine gewählt'),
      h('span', null, h('strong', null, 'Build: '), site.build),
      h('span', null, h('strong', null, 'Produkte: '), site.products.map((p) => `${p.name} ${p.documented}`).join(', ') || '—')));
  }
  for (const run of view.claudeRuns) {
    const c = run.outcome;
    items.push(h('div', { class: 'facts' },
      h('span', null, h('strong', null, `${run.task}: `), c.success ? 'abgeschlossen' : 'nicht abgeschlossen'),
      h('span', null, h('strong', null, 'Modell: '), modelName(run.model === 'Standard' ? null : run.model)),
      c.turns != null && h('span', null, h('strong', null, 'Runden: '), c.turns),
      c.costUsd != null && h('span', null, h('strong', null, 'Kosten: '), `${c.costUsd.toFixed(2)} USD`),
      c.denials.length > 0 && h('span', { class: 'orange' }, h('strong', null, 'Nicht freigegeben: '), c.denials.join(' · '))));
    if (c.text) items.push(h('div', { class: 'result-text' }, c.text));
  }
  return items;
}

function uploadResult() {
  const items = [];
  const cfg = view.config.sftp;
  items.push(h('div', { class: 'facts' },
    h('span', null, h('strong', null, 'Ziel: '), cfg.host ? `${cfg.user}@${cfg.host}:${cfg.port} ${cfg.remoteDir}` : 'nicht eingerichtet'),
    view.lastUpload && h('span', null, h('strong', null, 'Letzter Upload: '),
      `${view.lastUpload.uploadedAt ? formatTime(view.lastUpload.uploadedAt) : '?'} · Commit ${view.lastUpload.commit ?? '?'} · ${view.lastUpload.files} Dateien`)));
  if (view.upload) {
    items.push(h('div', { class: 'facts' },
      h('span', null, h('strong', null, 'Hochgeladen: '), view.upload.uploaded),
      h('span', null, h('strong', null, 'Entfernt: '), view.upload.deleted),
      h('span', null, h('strong', null, 'Unverändert: '), view.upload.unchanged),
      h('span', null, h('strong', null, 'Git: '), view.upload.pushed)));
  }
  return items;
}

function verifyResult() {
  const r = view.verify;
  if (!r) return [];
  return [
    h('div', { class: 'facts' },
      h('span', null, h('strong', null, 'Geprüft: '), r.checked),
      h('span', { class: 'green' }, h('strong', null, 'Identisch: '), r.identical),
      r.problems.length > 0 && h('span', { class: 'red' }, h('strong', null, 'Abweichend: '), r.problems.length)),
    r.problems.length > 0 && h('div', { class: 'result-text mono' }, r.problems.slice(0, 50).join('\n') + (r.problems.length > 50 ? `\n… ${r.problems.length - 50} weitere` : '')),
  ];
}

// MARK: Log

function clearLog() {
  if (skeleton) skeleton.log.replaceChildren();
}

function appendLog(lines) {
  if (!skeleton) return;
  const log = skeleton.log;
  const atBottom = log.scrollHeight - log.scrollTop - log.clientHeight < 40;
  log.append(...lines.map((l) => h('span', { class: `l-${l.kind}` }, `${l.text}\n`)));
  if (atBottom) log.scrollTop = log.scrollHeight;
}

async function loadLog(step) {
  const lines = await invoke('get_log', { step });
  if (step !== selected || !skeleton) return;
  clearLog();
  appendLog(lines);
}

// MARK: Dialogs

function openDialog(title, build) {
  const dialog = h('dialog', { 'aria-label': title });
  const close = () => dialog.close();
  const render = () => {
    const { body, actions } = build(close, render);
    dialog.replaceChildren(h('form', { method: 'dialog', onsubmit: (e) => e.preventDefault() },
      h('div', { class: 'dialog-body' }, body), h('div', { class: 'dialog-actions' }, actions)));
  };
  render();
  dialog.addEventListener('close', () => dialog.remove());
  document.body.append(dialog);
  dialog.showModal();
  return { dialog, close, render };
}

function showAlert(message) {
  openDialog('Fehler', (close) => ({
    body: [h('h2', null, 'Fehler'), h('div', { class: 'selectable' }, message)],
    actions: [button('OK', close, { cls: 'primary' })],
  }));
}

function row(label, control, id) {
  if (id) control.id = id;
  return h('div', { class: 'field-row' }, h('label', { for: id }, label), control);
}

// MARK: Repository picker

let repoCache = null;

async function loadRepos(force = false) {
  if (!repoCache || force) repoCache = await invoke('list_repos');
  return repoCache;
}

/** Filterable list of the user's repositories. Keeps its own state, so typing never re-renders the dialog. */
function repoPicker({ include = () => true, onPick }) {
  const state = { query: '', data: null, error: null, picked: null };
  const list = h('div', { class: 'repo-list', role: 'listbox' });
  const note = h('div', { class: 'note' });
  const search = h('input', { type: 'search', placeholder: 'Repositories filtern …', spellcheck: 'false', autocomplete: 'off',
    oninput: (e) => { state.query = e.target.value.trim().toLowerCase(); renderList(); } });
  const badge = (text, cls = '') => h('span', { class: `badge ${cls}` }, text);
  const renderList = () => {
    if (state.error) { list.replaceChildren(h('div', { class: 'note error-text' }, state.error)); return; }
    if (!state.data) { list.replaceChildren(h('div', { class: 'note' }, 'Repositories werden geladen …')); return; }
    const repos = state.data.repos.filter(include).filter((r) => !state.query
      || r.fullName.toLowerCase().includes(state.query) || (r.description ?? '').toLowerCase().includes(state.query));
    note.textContent = [state.data.note, `Lokale Klone werden in ${state.data.workspace} gesucht.`].filter(Boolean).join(' ');
    list.replaceChildren(...(repos.length ? repos.map((r) => h('button', {
      type: 'button', role: 'option', class: `repo-row ${state.picked === r.fullName ? 'picked' : ''}`,
      onclick: () => { state.picked = r.fullName; renderList(); onPick(r); },
    },
      h('div', { class: 'repo-head' }, h('strong', null, r.fullName),
        r.private && badge('privat', 'orange'),
        r.isWebsite && badge('Website', 'accent'),
        r.onSite && badge('auf der Seite', 'green'),
        r.localPath ? badge('lokal', 'green') : badge('nicht lokal')),
      r.description && h('div', { class: 'muted small' }, r.description))) : [h('div', { class: 'note' }, 'Keine passenden Repositories.')]));
  };
  const load = async (force) => {
    state.error = null;
    state.data = force ? null : state.data;
    renderList();
    try { state.data = await loadRepos(force); } catch (e) { state.error = String(e); }
    renderList();
  };
  const el = h('div', { class: 'repo-picker' },
    h('div', { class: 'line' }, search, button('Neu laden', () => load(true), { iconName: 'refresh', iconOnly: true })),
    list, note);
  load(false);
  return el;
}

// MARK: Settings

function settingsDialog() {
  const c = view.config;
  const s = {
    projectDir: c.projectDir, workspaceDir: c.workspaceDir ?? '', siteRepo: view.siteRepo, cloneRepo: null, cloning: false,
    showPicker: !c.projectDir,
    host: c.sftp.host, port: c.sftp.port || 22, user: c.sftp.user, remoteDir: c.sftp.remoteDir,
    auth: c.sftp.auth, keyPath: c.sftp.keyPath, secret: '', message: null, messageKind: 'note', busy: false,
  };
  const input = () => ({
    projectDir: s.projectDir, workspaceDir: s.workspaceDir, host: s.host, port: Number(s.port) || 22, user: s.user, remoteDir: s.remoteDir,
    auth: s.auth, keyPath: s.keyPath, secret: s.secret,
  });
  const text = (key, props = {}) => h('input', { type: 'text', value: s[key] ?? '', spellcheck: 'false', autocomplete: 'off', ...props,
    oninput: (e) => { s[key] = e.target.value; } });
  let render = () => {};
  const picker = repoPicker({
    include: (r) => !r.archived,
    onPick: (r) => {
      s.siteRepo = r.fullName;
      s.cloneRepo = r.localPath ? null : r.fullName;
      if (r.localPath) { s.projectDir = r.localPath; s.showPicker = false; }
      render();
    },
  });
  const dlg = openDialog('Einrichtung', (close, rerender) => {
    render = rerender;
    const password = s.auth === 'password';
    const sameProject = c.projectDir === s.projectDir;
    return {
      body: [
        h('h2', null, 'Einrichtung'),
        h('fieldset', null, h('legend', null, 'Website'),
          row('Website-Repo', h('div', { class: 'line' },
            h('span', { class: `spacer ${s.siteRepo ? 'mono' : 'muted'}` }, s.siteRepo ?? 'nicht gewählt'),
            button(s.showPicker ? 'Liste schließen' : 'Aus Liste wählen …', () => { s.showPicker = !s.showPicker; render(); })), 's-site-repo'),
          s.showPicker && picker,
          s.cloneRepo && h('div', { class: 'line' },
            h('span', { class: 'note spacer' }, `${s.cloneRepo} ist noch nicht lokal vorhanden.`),
            button(s.cloning ? 'Klone …' : `Nach ${s.workspaceDir || view.workspaceDisplay} klonen`, async () => {
              s.cloning = true; render();
              try {
                s.projectDir = await invoke('clone_repo', { repo: s.cloneRepo, workspaceDir: s.workspaceDir });
                s.cloneRepo = null; s.showPicker = false; repoCache = null;
              } catch (error) { s.message = String(error); s.messageKind = 'error-text'; }
              s.cloning = false; render();
            }, { cls: 'primary', disabled: s.cloning })),
          row('Projektordner', h('div', { class: 'line' }, text('projectDir', { placeholder: '~/Code/meine-website' }),
            button('Auswählen …', async () => { const p = await call('pick_folder'); if (p) { s.projectDir = p; render(); } })), 's-project'),
          h('div', { class: `note ${view.siteError && sameProject ? 'error-text' : ''}` },
            view.site && sameProject ? `.openftpupdater/site.json gefunden: ${view.site.name}, Build „${view.site.build}“, Ausgabe ${view.site.output}`
              : view.siteError && sameProject ? view.siteError
                : 'Der Projektordner braucht eine Datei .openftpupdater/site.json (siehe README); geprüft wird beim Sichern.'),
          row('Repo-Ordner', h('div', { class: 'line' }, text('workspaceDir', { placeholder: `${view.workspaceDisplay} (automatisch)` }),
            button('Auswählen …', async () => { const p = await call('pick_folder'); if (p) { s.workspaceDir = p; repoCache = null; render(); } })), 's-workspace'),
          h('div', { class: 'note' }, 'Hier sucht die App die lokalen Klone von Website und Produkten und legt neue Klone an.')),
        h('fieldset', null, h('legend', null, 'SFTP-Ziel'),
          row('Server', text('host', { placeholder: 'ssh.strato.de' }), 's-host'),
          row('Port', text('port', { inputmode: 'numeric' }), 's-port'),
          row('Benutzer', text('user', { placeholder: 'z. B. example.com' }), 's-user'),
          row('Zielordner', text('remoteDir', { placeholder: '/example.com' }), 's-remote'),
          h('div', { class: 'note' }, 'Ordner auf dem Server, der dem Ausgabeordner entspricht. Hochgeladen wird nur dorthin; gelöscht werden nur Dateien, die die App selbst hochgeladen hat.'),
          row('Anmeldung', h('select', { onchange: (e) => { s.auth = e.target.value; render(); } },
            h('option', { value: 'password', selected: password }, 'Passwort'),
            h('option', { value: 'key', selected: !password }, 'SSH-Schlüssel')), 's-auth'),
          !password && row('Schlüsseldatei', h('div', { class: 'line' }, text('keyPath', { placeholder: '~/.ssh/id_ed25519' }),
            button('Auswählen …', async () => { const p = await call('pick_file'); if (p) { s.keyPath = p; render(); } })), 's-key'),
          row(password ? 'Passwort' : 'Passphrase', h('input', { type: 'password', value: s.secret, autocomplete: 'off',
            placeholder: c.sftp.secretSaved ? 'gespeichert – leer lassen zum Behalten' : (password ? '' : 'leer, wenn der Schlüssel keine hat'),
            oninput: (e) => { s.secret = e.target.value; } }), 's-secret'),
          h('div', { class: 'note' }, 'Das Passwort liegt nur im Schlüsselbund des Systems, nie in einer Datei oder im Repository.',
            c.sftp.secretSaved && [' ', button('Gespeichertes Passwort entfernen', async () => { await call('forget_secret'); close(); }, { cls: 'link' })]),
          row('Server-Schlüssel', h('div', { class: c.sftp.hostKey ? 'mono selectable' : 'muted' },
            c.sftp.hostKey || 'wird bei der ersten Verbindung bestätigt'))),
        s.message && h('div', { class: `note ${s.messageKind}` }, s.message),
      ],
      actions: [
        button('Verbindung testen', async () => {
          s.busy = true; s.message = 'Verbinde …'; s.messageKind = 'note'; render();
          try {
            s.message = await invoke('test_connection', { input: input() });
            s.messageKind = 'ok-text';
            s.secret = '';
          } catch (error) {
            s.message = String(error);
            s.messageKind = 'error-text';
          }
          s.busy = false; render();
        }, { cls: 'left', disabled: s.busy || s.cloning }),
        button('Abbrechen', close),
        button('Sichern', async () => {
          try {
            await invoke('save_settings', { input: input() });
            close();
          } catch (error) {
            s.message = String(error); s.messageKind = 'error-text'; render();
          }
        }, { cls: 'primary', disabled: s.busy || s.cloning }),
      ],
    };
  });
  return dlg;
}

// MARK: New product

function addProductDialog() {
  const s = { repo: null, check: null, checking: false, error: null, name: '', localPath: '', instructions: '', cloning: false, model: '' };
  let render = () => {};
  const inspect = async (repo) => {
    s.repo = repo; s.check = null; s.error = null; s.checking = true;
    s.name = repo.name; s.localPath = repo.localPath ?? '';
    render();
    try {
      s.check = await invoke('inspect_repo', { repo: repo.fullName });
      if (s.check.localPath) s.localPath = s.check.localPath;
    } catch (error) { s.error = String(error); }
    s.checking = false; render();
  };
  const picker = repoPicker({ include: (r) => !r.onSite && !r.isWebsite && !r.archived && !r.fork, onPick: inspect });
  openDialog('Neues Produkt', (close, rerender) => {
    render = rerender;
    const info = s.check?.info;
    return {
      body: [
        h('h2', null, 'Neues Produkt aufnehmen'),
        h('p', { class: 'note', style: 'margin:0' }, 'Wähle das Repository des Produkts. Claude legt Produktseite (Deutsch und Englisch), Produktkarte, Navigation, Icon, Ablaufgrafik, llms.txt und den Eintrag in site.json an, nach dem Skill des Projekts. Danach baut die App und committet; live geht es erst mit Upload.'),
        picker,
        s.checking && h('div', { class: 'note' }, `${s.repo.fullName} wird geprüft …`),
        s.error && h('div', { class: 'note error-text' }, s.error),
        info && h('fieldset', null, h('legend', null, info.fullName),
          h('div', { class: 'facts' },
            h('span', null, h('strong', null, 'Release: '), s.check.latestTag ?? 'noch keins'),
            h('span', null, h('strong', null, 'Lizenz: '), info.license ?? 'keine erkannt'),
            info.private && h('span', { class: 'orange' }, h('strong', null, 'privat: '), 'Besucher können GitHub-Links darauf nicht öffnen')),
          info.description && h('div', { class: 'muted' }, info.description),
          !s.check.latestTag && h('div', { class: 'note orange' }, 'Ohne Release gibt es kein Versions-Badge, und der Precheck beobachtet das Produkt erst nach dem ersten Release.'),
          row('Name', h('input', { type: 'text', value: s.name, spellcheck: 'false', oninput: (e) => { s.name = e.target.value; } }), 'n-name'),
          row('Lokaler Klon', h('div', { class: 'line' },
            h('span', { class: `spacer ${s.localPath ? 'mono' : 'muted'}` }, s.localPath || 'nicht vorhanden – Claude liest dann nur über GitHub'),
            !s.localPath && button(s.cloning ? 'Klone …' : 'Klonen', async () => {
              s.cloning = true; render();
              try { s.localPath = await invoke('clone_repo', { repo: info.fullName, workspaceDir: '' }); repoCache = null; } catch (error) { s.error = String(error); }
              s.cloning = false; render();
            }, { disabled: s.cloning }),
            button('Auswählen …', async () => { const p = await call('pick_folder'); if (p) { s.localPath = p; render(); } })), 'n-local'),
          row('Modell', modelSelect(s.model, view.site?.claude?.model, (model) => { s.model = model; }, 'n-model'), 'n-model'),
          h('div', { class: 'field-col' },
            h('label', { for: 'n-instructions' }, 'Anweisung an Claude (optional)'),
            h('textarea', { id: 'n-instructions', rows: 3, value: s.instructions,
              placeholder: 'z. B. Kategorie, Zielgruppe, Position in der Produktliste, welche Screenshots aus dem Repo',
              oninput: (e) => { s.instructions = e.target.value; } }))),
      ],
      actions: [
        button('Abbrechen', close),
        button('Aufnehmen', async () => {
          const ok = await call('run_add_product', { input: { repo: info.fullName, name: s.name, localPath: s.localPath, instructions: s.instructions, model: s.model } });
          if (ok !== null) close();
        }, { cls: 'primary', disabled: !info || s.checking || s.cloning || !s.name.trim() || !!view.running }),
      ],
    };
  });
}

function hostKeyDialog() {
  hostKeyOpen = true;
  const cfg = view.config.sftp;
  const fingerprint = view.pendingHostKey;
  const { dialog } = openDialog('Server-Schlüssel bestätigen', (close) => ({
    body: [
      h('h2', null, 'Server-Schlüssel bestätigen'),
      h('p', { class: 'muted', style: 'margin:0' }, `Erste Verbindung zu ${cfg.host}:${cfg.port}. Der Server weist sich mit diesem Schlüssel aus:`),
      h('div', { class: 'fingerprint' }, fingerprint),
      h('p', { class: 'note', style: 'margin:0' }, 'Vergleiche ihn mit der Angabe deines Hosters oder im Terminal mit:'),
      h('div', { class: 'fingerprint' }, `ssh-keyscan -p ${cfg.port} ${cfg.host} | ssh-keygen -lf -`),
      h('p', { class: 'note', style: 'margin:0' }, 'Nach dem Bestätigen bricht jede Verbindung ab, falls sich der Schlüssel später ändert.'),
    ],
    actions: [
      button('Abbrechen', async () => { await call('reject_host_key'); close(); }),
      button('Vertrauen', async () => { await call('trust_host_key', { fingerprint }); close(); }, { cls: 'primary' }),
    ],
  }));
  dialog.addEventListener('close', () => { hostKeyOpen = false; });
}

// MARK: Start

async function start() {
  await listen('changed', refresh);
  await listen('log', (event) => {
    if (event.payload.step === selected) appendLog([event.payload.line]);
  });
  await refresh();
}

start();

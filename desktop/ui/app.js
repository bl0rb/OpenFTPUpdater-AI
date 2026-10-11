// OpenReleasePilot UI: renders the view the Rust side sends and calls its commands. No framework, no build step.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const STEPS = [
  { id: 'precheck', title: 'Precheck', desc: 'Prüft Repo, Werkzeuge, Releases, SFTP und Live-Seite. Ändert nichts.' },
  { id: 'update', title: 'Update', desc: 'Claude passt die Seite mit den gewählten Aufgaben an, dann Build und Commit.' },
  { id: 'upload', title: 'Upload', desc: 'Build, Push, dann nur geänderte Dateien per SFTP auf den Server.' },
  { id: 'verify', title: 'Verify', desc: 'Vergleicht die Live-Seite Datei für Datei mit dem Build.' },
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
// Main area: 'overview' (the website's four steps) or 'repos' (the products' repositories).
let area = 'overview';
let selected = 'precheck';
// Tab of the detail panel: 'result' or 'log'.
let detailTab = 'result';
// The same for the care panel in Repositories.
let careTab = 'result';
// Repository whose details are open; null = the list. The list's scroll position for the way back.
let repoKey = null;
let repoListScroll = 0;
// Project whose repositories were read once on first opening the area.
let reposReadFor = null;
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
  branch: '<circle cx="6" cy="5" r="2"/><circle cx="6" cy="19" r="2"/><circle cx="18" cy="7" r="2"/><path d="M6 7v10M18 9c0 5-6 4-11 8"/>',
  back: '<path d="M19 12H5M11 6l-6 6 6 6"/>',
};

function icon(name) {
  const span = document.createElement('span');
  span.innerHTML = `<svg class="icon" viewBox="0 0 24 24" aria-hidden="true">${ICONS[name]}</svg>`;
  return span.firstElementChild;
}

function button(label, onClick, { iconName, cls, disabled, title, iconOnly, id } = {}) {
  return h('button', {
    type: 'button', id, class: [cls, iconOnly && 'icon-only'].filter(Boolean).join(' ') || null, disabled,
    title: title ?? (iconOnly ? label : null), 'aria-label': iconOnly ? label : null,
    onclick: (e) => { e.stopPropagation(); onClick?.(e); },
  }, iconName && icon(iconName), !iconOnly && label);
}

const formatTime = (secs) => new Date(secs * 1000).toLocaleString('de-DE', { dateStyle: 'medium', timeStyle: 'short' });

/** `vor 3 Min.` for a Unix time. */
function ago(secs) {
  const d = Math.max(0, Math.round(Date.now() / 1000 - secs));
  if (d < 45) return 'gerade eben';
  if (d < 3600) return `vor ${Math.round(d / 60)} Min.`;
  if (d < 86400) return `vor ${Math.round(d / 3600)} Std.`;
  return `am ${formatTime(secs)}`;
}

const short = (sha) => (sha ?? '').slice(0, 7);

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
    // Follow the step that just started and watch its log.
    // Load the whole log: lines that arrived before this view must not be lost.
    if (view.running === 'repos') {
      careTab = 'log';
      loadCareLog();
    } else {
      selected = view.running;
      detailTab = 'log';
      loadLog(selected);
    }
  } else if (!view.running && lastRunning === 'repos') {
    if (view.repoPlan || view.repoReports.length) careTab = 'result';
  } else if (!view.running && lastRunning === selected && hasResult(selected)) {
    // The step is done: show what came out of it. Without a result the log has the details.
    detailTab = 'result';
  }
  lastRunning = view.running;
  renderToolbar();
  renderBanners();
  renderMain();
  renderFooter();
  if (view.pendingHostKey && !hostKeyOpen) hostKeyDialog();
}

function renderToolbar() {
  const attention = view.repositories.filter((r) => r.health.state === 'blocked' || r.health.state === 'needsAttention').length;
  const nav = (id, label, extra) => h('button', {
    type: 'button', class: `nav-item ${area === id ? 'active' : ''}`, 'aria-current': area === id ? 'page' : null,
    onclick: () => showArea(id),
  }, label, extra);
  document.getElementById('toolbar').replaceChildren(
    h('div', { class: 'brand' },
      h('img', { src: 'icon.svg', alt: '' }),
      h('div', null,
        h('h1', null, 'OpenReleasePilot'),
        h('div', { class: 'project' }, view.site ? `${view.site.name} · ${view.projectDisplay}` : view.projectDisplay || 'Kein Projekt eingerichtet'))),
    view.config.projectDir && h('nav', { class: 'nav', 'aria-label': 'Bereiche' },
      nav('overview', 'Übersicht'),
      nav('repos', 'Repositories', attention > 0 && h('span', { class: 'nav-badge', title: `${attention} brauchen Aufmerksamkeit`, 'aria-label': `${attention} brauchen Aufmerksamkeit` }, attention))),
    h('span', { class: 'spacer' }),
    view.site && button(view.site.url.replace(/^https?:\/\//, ''), () => call('open_url', { url: view.site.url }), { iconName: 'external', cls: 'link', title: 'Live-Seite öffnen' }),
    view.site && button('Neues Produkt …', addProductDialog,
      { iconName: 'plus', disabled: !!view.running, title: 'Ein Repository als neues Produkt auf der Website aufnehmen' }),
    button('Einrichtung', settingsDialog, { iconName: 'gear', disabled: !!view.running }));
}

function showArea(id) {
  const main = document.getElementById('main');
  if (area === id) {
    if (id === 'repos' && repoKey) closeRepo();
    return;
  }
  if (area === 'repos' && !repoKey) repoListScroll = main.scrollTop;
  area = id;
  renderToolbar();
  renderMain();
  if (id === 'repos') {
    main.scrollTop = repoKey ? 0 : repoListScroll;
    readReposOnce();
  }
}

function banner(kind, text, actions = []) {
  return h('div', { class: `banner ${kind}` }, h('div', { class: 'text selectable' }, text),
    actions.map(([label, action, opts]) => button(label, action, opts)));
}

function updateBanner(u) {
  const installing = view.installingUpdate;
  return h('div', { class: 'banner update' },
    h('div', { class: 'text' },
      h('strong', null, `${u.prerelease ? 'Beta: ' : ''}OpenReleasePilot ${u.version} ist verfügbar (installiert: ${view.version})`),
      h('div', { class: 'muted small' }, installing ? 'Paket wird geladen und geprüft …'
        : u.prerelease ? 'Beta-Versionen sind zum Testen gedacht. Die Installation benötigt Administratorrechte.'
          : 'Die Installation benötigt Administratorrechte. Danach die App neu starten.')),
    button('Versionshinweise', () => call('open_url', { url: u.pageUrl })),
    button('Später', () => call('dismiss_update'), { disabled: installing }),
    button(installing ? 'Wird geladen …' : 'Installieren', () => call('install_update'),
      { cls: 'primary', disabled: installing || !!view.running, title: view.running ? 'Erst nach dem laufenden Schritt' : null }));
}

function renderFooter() {
  const checking = view.checkingUpdate;
  const check = (beta) => () => call('check_for_updates', { beta });
  document.getElementById('footer').replaceChildren(
    h('span', null, view.devBuild ? 'OpenReleasePilot Entwicklungsversion' : `OpenReleasePilot ${view.version}`),
    h('span', { class: 'spacer' }),
    ...(checking ? [h('span', null, 'Suche nach Updates …')] : [
      button('Nach Updates suchen', check(false), { cls: 'link' }),
      button('Beta-Version suchen', check(true), { cls: 'link' })]));
}

function renderBanners() {
  const items = [];
  if (view.availableUpdate) items.push(updateBanner(view.availableUpdate));
  if (view.error) items.push(banner('error', view.error, [['Schließen', () => call('dismiss')]]));
  if (view.notice) items.push(banner('info', view.notice, [['Schließen', () => call('dismiss')]]));
  if (view.config.projectDir && view.siteError) items.push(banner('warning', `Projekt: ${view.siteError}`, [['Einrichtung', settingsDialog]]));
  if (view.site && !view.sftpReady) items.push(banner('warning', 'SFTP ist noch nicht eingerichtet: Upload ist erst danach möglich.', [['Einrichtung', settingsDialog]]));
  document.getElementById('banners').replaceChildren(...items);
}

let skeleton = null;
let reposUi = null;

function renderMain() {
  const main = document.getElementById('main');
  if (!view.config.projectDir) {
    skeleton = null;
    reposUi = null;
    area = 'overview';
    main.replaceChildren(h('div', { class: 'empty' },
      h('img', { src: 'icon.svg', alt: '' }),
      h('h2', null, 'Willkommen'),
      h('p', { class: 'muted' }, 'Wähle den Projektordner deiner Website (mit .openreleasepilot/site.json) und den SFTP-Zugang. Danach prüft, aktualisiert und veröffentlicht die App die Seite in vier Schritten.'),
      button('Einrichten', settingsDialog, { cls: 'primary' })));
    return;
  }
  if (!skeleton) {
    // Built once, so the instruction field keeps focus and the log keeps its scroll position.
    skeleton = {
      steps: h('section', { class: 'steps' }),
      products: h('div', { class: 'list products' }),
      productCount: h('span', { class: 'muted small' }),
      productAll: h('input', { type: 'checkbox', onchange: (e) => call('set_products', { ids: view.products.map((p) => p.id), enabled: e.target.checked }) }),
      tasks: h('div', { class: 'list tasks' }),
      instructions: h('textarea', { id: 'instructions', rows: 1, placeholder: 'z. B. „Ergänze im nudgit-Abschnitt den Hinweis auf die Chrome-Erweiterung“ – leer lassen für ein reines Release-Update' }),
      runAll: null,
      detailHead: h('div', { class: 'detail-head' }),
      tabResult: h('button', { type: 'button', class: 'tab', role: 'tab', id: 'tab-result', 'aria-controls': 'detail-result', onclick: () => showTab('result') }, 'Ergebnis'),
      tabLog: h('button', { type: 'button', class: 'tab', role: 'tab', id: 'tab-log', 'aria-controls': 'log', onclick: () => showTab('log') }),
      detailResult: h('div', { class: 'detail-result', id: 'detail-result', role: 'tabpanel', 'aria-labelledby': 'tab-result' }),
      log: h('pre', { class: 'log', id: 'log', role: 'tabpanel', 'aria-labelledby': 'tab-log', tabindex: '0' }),
    };
    skeleton.runAll = button('Alle Schritte', () => call('run_all', { instructions: skeleton.instructions.value }),
      { iconName: 'forward', title: 'Precheck, Update (wenn nötig), Upload und Verify nacheinander; stoppt beim ersten Fehler' });
    skeleton.root = h('div', { class: 'area' },
      h('section', { class: 'section' },
        h('div', { class: 'section-head' }, h('h2', null, 'Ablauf'),
          h('span', { class: 'muted small' }, 'Schritte einzeln starten oder alle nacheinander'),
          h('span', { class: 'spacer' }), skeleton.runAll),
        skeleton.steps),
      h('div', { class: 'config' },
        h('section', { class: 'section panel' },
          h('div', { class: 'section-head' }, h('h2', null, 'Produkte'), skeleton.productCount, h('span', { class: 'spacer' }),
            h('label', { class: 'check-all', title: 'Alle Produkte an- oder abwählen' }, skeleton.productAll, 'Alle')),
          skeleton.products,
          h('div', { class: 'note' }, 'Nur ausgewählte Produkte prüft der Precheck und aktualisiert Update.')),
        h('section', { class: 'section panel' },
          h('div', { class: 'section-head' }, h('h2', null, 'Update-Aufgaben'),
            h('span', { class: 'muted small' }, 'je ein Claude-Lauf, in dieser Reihenfolge')),
          skeleton.tasks,
          h('div', { class: 'field-col' }, h('label', { for: 'instructions' }, 'Zusätzliche Anweisung an Claude (optional)'), skeleton.instructions))),
      h('section', { class: 'detail' }, skeleton.detailHead,
        h('div', { class: 'tabs', role: 'tablist', 'aria-label': 'Ansicht', onkeydown: tabKey }, skeleton.tabResult, skeleton.tabLog),
        skeleton.detailResult, skeleton.log));
    buildRepos();
    main.replaceChildren(skeleton.root, reposUi.root);
    loadLog(selected);
    loadCareLog();
  }
  skeleton.root.hidden = area !== 'overview';
  reposUi.root.hidden = area !== 'repos';
  if (area === 'repos') {
    renderRepos();
    return;
  }
  skeleton.runAll.disabled = !!view.running || !view.site;
  skeleton.instructions.disabled = !!view.running;
  renderSteps();
  renderProducts();
  renderTasks();
  renderDetail();
}

function versionBadge(p) {
  const latest = p.latestTag?.replace(/^v/, '');
  if (!latest) return h('span', { class: 'badge', title: 'Dokumentierter Stand; der Precheck fragt das neueste Release ab' }, p.documented);
  if (p.outdated) return h('span', { class: 'badge orange', title: `Neues Release ${p.latestTag}` }, `${p.documented} → ${latest}`);
  return h('span', { class: 'badge green', title: 'Dokumentierter Stand ist das neueste Release' }, `${p.documented} aktuell`);
}

function renderProducts() {
  const products = view.products;
  const on = products.filter((p) => p.enabled).length;
  skeleton.productCount.textContent = products.length ? `${on} von ${products.length} ausgewählt` : '';
  skeleton.productAll.checked = products.length > 0 && on === products.length;
  skeleton.productAll.indeterminate = on > 0 && on < products.length;
  skeleton.productAll.disabled = !!view.running || !products.length;
  skeleton.products.replaceChildren(...(products.length ? [
    h('div', { class: 'row head' }, h('span', null, 'Produkt · Repository'), h('span'), h('span', null, 'Stand')),
    ...products.map((p) => h('div', { class: `row ${p.enabled ? 'on' : ''}` },
    h('label', { class: 'row-name' },
      h('input', { type: 'checkbox', checked: p.enabled, disabled: !!view.running, onchange: (e) => call('set_products', { ids: [p.id], enabled: e.target.checked }) }),
      h('span', { class: 'row-text inline' }, h('strong', null, p.name), h('span', { class: 'muted small mono ellipsis', title: p.repo }, p.repo))),
    p.local ? h('span') : h('span', { class: 'badge orange', title: p.localPath ? `Kein lokaler Klon unter ${p.localPath}` : 'Kein localPath in site.json' }, 'kein Klon'),
    versionBadge(p)))]
    : [h('div', { class: 'note empty-list' }, 'Noch keine Produkte: „Neues Produkt …“ in der Kopfleiste nimmt eins auf.')]));
}

function renderTasks() {
  const head = h('div', { class: 'row head' }, h('span', null, 'Aufgabe'), h('span', null, 'Skill'), h('span', null, 'Effort'), h('span', null, 'Modell'));
  skeleton.tasks.replaceChildren(head, ...view.tasks.map((t) => {
    const save = (enabled, model) => call('set_task', { id: t.id, enabled, model });
    // One row per task; the columns line up across rows (CSS subgrid).
    return h('div', { class: `row ${t.enabled ? 'on' : ''}` },
      h('label', { class: 'row-name' },
        h('input', { type: 'checkbox', checked: t.enabled, disabled: !!view.running, onchange: (e) => save(e.target.checked, t.chosenModel) }),
        h('span', { class: 'row-text' }, h('strong', null, t.label), t.description && h('span', { class: 'muted small' }, t.description))),
      h('span', null, t.skill && h('span', { class: 'badge mono', title: 'Skill im Projekt' }, t.skill)),
      h('span', null, t.effort && h('span', { class: 'badge', title: 'Effort aus site.json' }, t.effort)),
      modelSelect(t.chosenModel, t.defaultModel, (model) => save(t.enabled, model), `task-model-${t.id}`));
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
    const action = running
      ? button('Abbrechen', () => invoke('cancel'), { iconName: 'stop', cls: 'danger' })
      : button('Starten', () => call('run_step', { step: step.id, instructions: skeleton.instructions.value }),
        { iconName: 'play', cls: next === step.id ? 'primary' : null, disabled: !!view.running || !ready(step.id) });
    const summary = state.stale ? 'Nach Update/Upload erneut prüfen' : state.summary;
    return h('div', {
      class: `step ${selected === step.id ? 'selected' : ''}`, role: 'button', tabindex: '0',
      onclick: () => selectStep(step.id), onkeydown: (e) => { if (e.key === 'Enter') selectStep(step.id); },
    },
      h('div', { class: 'step-head' }, h('span', { class: 'step-number' }, i + 1), h('h2', null, step.title), statusChip(state.status, state.stale)),
      h('div', { class: 'desc' }, step.desc),
      h('div', { class: `summary ${state.stale ? 'muted' : state.status === 'error' ? 'red' : state.status === 'warning' ? 'orange' : 'muted'}`, title: summary || null },
        summary || ''),
      action);
  }));
}

function selectStep(step) {
  if (selected === step) return;
  selected = step;
  if (view.running === step) detailTab = 'log';
  renderSteps();
  renderDetail();
  loadLog(step);
}

const RESULTS = { precheck: precheckResult, update: updateResult, upload: uploadResult, verify: verifyResult };
const HAS_RESULT = {
  precheck: () => view.checks.length > 0,
  update: () => view.claudeRuns.length > 0,
  upload: () => !!view.upload,
  verify: () => !!view.verify,
};
const hasResult = (step) => HAS_RESULT[step]?.() ?? false;

function renderDetail() {
  const step = STEPS.find((s) => s.id === selected);
  const state = view.steps[selected];
  skeleton.detailHead.replaceChildren(...[
    h('h3', null, step.title), statusChip(state.status, state.stale),
    state.summary && !state.stale && h('span', { class: `detail-summary ${state.status === 'error' ? 'red' : state.status === 'warning' ? 'orange' : ''}` }, state.summary),
    h('span', { class: 'spacer' }),
    h('span', { class: 'muted small' }, state.finishedAt ? `zuletzt ${formatTime(state.finishedAt)}` : ''),
  ].filter(Boolean));
  const empty = !hasResult(selected) && (state.status === 'idle'
    ? h('div', { class: 'note' }, 'Dieser Schritt ist noch nicht gelaufen.')
    : h('div', { class: 'note' }, view.running === selected ? 'Läuft gerade. Die Ausgabe steht im ' : 'Kein Ergebnis. Was passiert ist, steht im ',
      button('Log', () => showTab('log'), { cls: 'link' }), '.'));
  skeleton.detailResult.replaceChildren(...[...RESULTS[selected](), empty].filter(Boolean));
  renderTabs();
}

function renderTabs() {
  const log = detailTab === 'log';
  const showsLog = !skeleton.log.hidden;
  for (const [tab, on] of [[skeleton.tabResult, !log], [skeleton.tabLog, log]]) {
    tab.setAttribute('aria-selected', String(on));
    tab.tabIndex = on ? 0 : -1;
  }
  skeleton.tabLog.replaceChildren(view.running === selected ? h('span', { class: 'spinner' }) : '', 'Log');
  skeleton.detailResult.hidden = log;
  skeleton.log.hidden = !log;
  // A hidden log loses its scroll position; show the newest lines again.
  if (log && !showsLog) skeleton.log.scrollTop = skeleton.log.scrollHeight;
}

function showTab(tab) {
  if (detailTab === tab) return;
  detailTab = tab;
  renderTabs();
}

/** Arrow keys move between the two tabs, as in any tab bar. */
function tabKey(e) {
  if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(e.key)) return;
  e.preventDefault();
  const next = e.key === 'Home' ? 'result' : e.key === 'End' ? 'log' : detailTab === 'log' ? 'result' : 'log';
  showTab(next);
  (next === 'log' ? skeleton.tabLog : skeleton.tabResult).focus();
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

const usd = (n) => `${n.toLocaleString('de-DE', { minimumFractionDigits: 2, maximumFractionDigits: 2 })} USD`;

/** Claude's answer with the little Markdown it writes: paragraphs, headings, "- " lists, **bold**, `code`.
 *  Built as DOM nodes, never as HTML. */
function markdown(text) {
  const inline = (line) => line.split(/(\*\*[^*]+\*\*|`[^`]+`)/).filter(Boolean).map((part) =>
    part.length > 4 && part.startsWith('**') && part.endsWith('**') ? h('strong', null, part.slice(2, -2))
      : part.length > 2 && part.startsWith('`') && part.endsWith('`') ? h('code', null, part.slice(1, -1))
        : part);
  const blocks = [];
  let list = null;
  for (const line of text.split('\n')) {
    const item = line.match(/^\s*[-*] (.*)$/);
    if (item) {
      if (!list) blocks.push(list = h('ul'));
      list.append(h('li', null, inline(item[1])));
      continue;
    }
    list = null;
    if (!line.trim()) continue;
    const heading = line.match(/^#{1,4} (.*)$/);
    blocks.push(h('p', null, heading ? h('strong', null, inline(heading[1])) : inline(line)));
  }
  return h('div', { class: 'md selectable' }, blocks);
}

// Which Claude runs the user opened or closed; survives re-rendering.
const runOpen = new Map();

function runItem(run, index, count) {
  const c = run.outcome;
  const key = `${index}:${run.task}`;
  const open = runOpen.get(key) ?? (count === 1 || !c.success || c.denials.length > 0);
  return h('details', { class: 'run', open, ontoggle: (e) => runOpen.set(key, e.target.open) },
    h('summary', null,
      h('strong', null, run.task),
      h('span', { class: 'muted small' }, [modelName(run.model === 'Standard' ? null : run.model),
        c.turns != null && `${c.turns} Runden`, c.costUsd != null && usd(c.costUsd)].filter(Boolean).join(' · ')),
      c.denials.length > 0 && h('span', { class: 'badge orange' }, `${c.denials.length} nicht freigegeben`),
      h('span', { class: 'spacer' }),
      h('span', { class: `chip ${c.success ? 'ok' : 'error'}` }, c.success ? 'Abgeschlossen' : 'Nicht abgeschlossen')),
    h('div', { class: 'run-body' },
      c.denials.length > 0 && h('div', { class: 'denials' },
        h('div', null, h('strong', null, 'Nicht freigegeben: '),
          'Claude durfte diese Befehle nicht ausführen und hat ohne sie weitergearbeitet. Dauerhaft erlauben lässt sich ein Befehl über ',
          h('code', null, 'claude.allowedTools'), ' in ', h('code', null, view.siteFile), '.'),
        h('ul', null, c.denials.map((d) => h('li', { class: 'mono selectable', title: d }, h('span', { class: 'clamp' }, d))))),
      c.text ? markdown(c.text) : h('div', { class: 'muted' }, 'Keine Antwort von Claude.')));
}

function updateResult() {
  const runs = view.claudeRuns;
  if (!runs.length) runOpen.clear();
  const items = [h('div', { class: 'facts' },
    h('span', null, h('strong', null, 'Gewählte Aufgaben: '), view.tasks.filter((t) => t.enabled).map((t) => t.label).join(', ') || 'keine'),
    view.site && h('span', null, h('strong', null, 'Build: '), h('span', { class: 'mono' }, view.site.build)))];
  if (runs.length) {
    const cost = runs.reduce((sum, r) => sum + (r.outcome.costUsd ?? 0), 0);
    const turns = runs.reduce((sum, r) => sum + (r.outcome.turns ?? 0), 0);
    items.push(h('div', { class: 'runs' },
      h('div', { class: 'runs-head' }, h('strong', null, runs.length === 1 ? '1 Claude-Lauf' : `${runs.length} Claude-Läufe`),
        h('span', { class: 'muted small' }, `${turns} Runden · ${usd(cost)}`)),
      runs.map((run, i) => runItem(run, i, runs.length))));
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

// MARK: Repositories

const HEALTH = {
  blocked: ['error', 'Blockiert'],
  needsAttention: ['warning', 'Aufmerksamkeit nötig'],
  unknown: ['idle', 'Unbekannt'],
  healthy: ['ok', 'In Ordnung'],
};
const CI_TEXT = { passed: 'erfolgreich', failed: 'fehlgeschlagen', pending: 'läuft noch', noChecks: 'keine Checks gefunden', unknown: 'unklar' };
const LOCAL_TEXT = {
  notConfigured: 'keine lokale Kopie eingetragen', found: 'lokale Kopie vorhanden', missing: 'lokale Kopie fehlt',
  foreign: 'lokale Kopie ungültig', conflict: 'lokale Kopie widersprüchlich',
};
const RISKS = { green: ['green', 'Risiko grün'], yellow: ['orange', 'Risiko gelb'], red: ['red', 'Risiko rot'] };
const badge = (cls, text, title) => h('span', { class: `badge ${cls}`, title }, text);
/** The value of a read part of the facts (`{ state: 'ok', value }`), else undefined. */
const ok = (fetch) => (fetch?.state === 'ok' ? fetch.value : undefined);
/** An id from a repository key, so focus survives re-rendering. */
const domId = (prefix, key) => `${prefix}-${key.replace(/[^a-z0-9]+/gi, '-')}`;
// Care options, shared by the detail view and the dialog while the window is open.
const careOptions = { merge: true, cleanup: true, model: '' };
// Pull request details asked for once per reading of a repository.
const detailsAsked = new Set();

function buildRepos() {
  reposUi = {
    root: h('div', { class: 'area repos-area' }),
    head: h('div', { class: 'section-head wrap' }),
    care: h('section', { class: 'detail care', 'aria-label': 'Pflege' }),
    careHead: h('div', { class: 'detail-head' }),
    careTabResult: h('button', { type: 'button', class: 'tab', role: 'tab', id: 'care-tab-result', 'aria-controls': 'care-result', onclick: () => showCareTab('result') }, 'Ergebnis'),
    careTabLog: h('button', { type: 'button', class: 'tab', role: 'tab', id: 'care-tab-log', 'aria-controls': 'care-log', onclick: () => showCareTab('log') }),
    careResult: h('div', { class: 'detail-result', id: 'care-result', role: 'tabpanel', 'aria-labelledby': 'care-tab-result' }),
    careLog: h('pre', { class: 'log', id: 'care-log', role: 'tabpanel', 'aria-labelledby': 'care-tab-log', tabindex: '0' }),
    body: h('div', { class: 'repos-body' }),
  };
  reposUi.care.append(reposUi.careHead,
    h('div', { class: 'tabs', role: 'tablist', 'aria-label': 'Pflege', onkeydown: careTabKey }, reposUi.careTabResult, reposUi.careTabLog),
    reposUi.careResult, reposUi.careLog);
  reposUi.root.append(
    h('section', { class: 'section' }, reposUi.head,
      h('div', { class: 'note' }, '„Status aktualisieren“ liest nur und kostet nichts. Pflegen erstellt zuerst eine Vorschau mit Claudes Bewertung der offenen PRs (kostet Claude-Nutzung); geändert wird erst, wenn du sie bestätigst.')),
    reposUi.care, reposUi.body);
}

/** Reads every repository once when the area first opens for a project; later only on request. */
function readReposOnce() {
  if (!view.repositories.length || reposReadFor === view.config.projectDir) return;
  reposReadFor = view.config.projectDir;
  const unread = view.repositories.filter((r) => !r.facts && !r.loading).map((r) => r.key);
  if (unread.length) call('refresh_repos', { keys: unread });
}

/** Re-renders, then gives the focus back to the element with the same id. */
function keepFocus(render) {
  const id = document.activeElement?.id;
  render();
  if (id && document.activeElement?.id !== id) document.getElementById(id)?.focus({ preventScroll: true });
}

function renderRepos() {
  keepFocus(() => {
    const repos = view.repositories;
    const reading = repos.some((r) => r.loading || r.detailsLoading);
    const chosen = repos.filter((r) => r.selected);
    reposUi.head.replaceChildren(...[
      h('h2', null, 'Repositories'),
      h('span', { class: 'muted small' }, [repos.length && `${repos.length} aus den Produkten der Website`, view.siteRepo && `Website-Repository ${view.siteRepo}`].filter(Boolean).join(' · ')),
      h('span', { class: 'spacer' }),
      reading && button('Lesen abbrechen', () => call('cancel_repo_refresh'), { id: 'repos-cancel-read' }),
      button(reading ? 'Wird gelesen …' : 'Status aktualisieren', () => call('refresh_repos', { keys: [] }),
        { id: 'repos-refresh', iconName: 'refresh', disabled: reading || !repos.length, title: 'Liest den Stand aller Repositories auf GitHub: nur lesend, ohne Claude' }),
      button('Ausgewählte pflegen …', () => careDialog(chosen),
        { id: 'repos-care', iconName: 'branch', disabled: !!view.running || !chosen.length, title: 'Pflege-Vorschau für die Repositories der ausgewählten Produkte' }),
    ].filter(Boolean));
    renderCare();
    let body;
    if (!view.site) body = h('div', { class: 'note' }, view.siteError ? `site.json nicht lesbar: ${view.siteError}` : 'Kein Projekt geladen.');
    else if (!repos.length) body = h('div', { class: 'note' }, 'In site.json steht noch kein Produkt mit Repository. „Neues Produkt …“ in der Kopfleiste nimmt eins auf.');
    else {
      const repo = repoKey && repos.find((r) => r.key === repoKey);
      if (!repo) repoKey = null;
      body = repo ? repoDetail(repo) : h('div', { class: 'repo-cards' }, repos.map(repoCard));
    }
    reposUi.body.replaceChildren(body);
  });
}

function openRepo(key) {
  const main = document.getElementById('main');
  repoListScroll = main.scrollTop;
  repoKey = key;
  renderRepos();
  main.scrollTop = 0;
  document.getElementById('repo-back')?.focus({ preventScroll: true });
}

function closeRepo() {
  const key = repoKey;
  repoKey = null;
  renderRepos();
  document.getElementById('main').scrollTop = repoListScroll;
  if (key) document.getElementById(domId('repo-row', key))?.focus({ preventScroll: true });
}

// MARK: Repositories: overview

function healthChip(r) {
  if (r.loading && !r.facts) return h('span', { class: 'chip running' }, h('span', { class: 'spinner' }), 'Wird gelesen');
  const [cls, text] = HEALTH[r.health.state];
  return h('span', { class: `chip ${cls}` }, text);
}

function mainReason(r) {
  if (r.loading && !r.facts) return 'GitHub wird abgefragt …';
  return r.health.reasons[0] ?? r.health.gaps[0] ?? 'Alles gelesen, kein Befund';
}

function prSummary(r) {
  const prs = r.facts?.prs;
  if (!prs || prs.state === 'notQueried') return 'PRs nicht abgefragt';
  if (prs.state === 'failed') return 'PR-Abfrage fehlgeschlagen';
  const { items, complete } = prs.value;
  const dependabot = items.filter((p) => p.dependabot).length;
  if (!items.length) return 'keine offenen PRs';
  return `${complete ? '' : 'mind. '}${items.length} offene${items.length === 1 ? 'r PR' : ' PRs'}${dependabot ? `, ${dependabot} von Dependabot` : ''}`;
}

function ciSummary(r) {
  const ci = r.facts?.ci;
  const branch = ok(r.facts?.access)?.defaultBranch ?? 'Default-Branch';
  if (!ci || ci.state === 'notQueried') return `CI ${branch} nicht abgefragt`;
  if (ci.state === 'failed') return `CI ${branch}: Abfrage fehlgeschlagen`;
  return `CI ${branch}: ${CI_TEXT[ci.value.state]}`;
}

function releaseSummary(r) {
  const f = r.facts;
  if (!f) return null;
  if (ok(f.release)) return `Release ${f.release.value}`;
  if (f.release.state === 'failed') return 'Release-Abfrage fehlgeschlagen';
  if (ok(f.tag)) return `kein Release, Tag ${f.tag.value}`;
  return f.release.state === 'ok' ? 'kein veröffentlichtes Release' : null;
}

function checkedSummary(r) {
  const f = r.facts;
  if (!f) return r.error ? `nicht gelesen: ${r.error}` : 'noch nicht gelesen';
  return `gelesen ${ago(f.checkedAt)}${r.health.stale ? ', veraltet' : ''}${r.error ? ', letzte Abfrage fehlgeschlagen' : ''}`;
}

function repoCard(r) {
  const [owner, name] = r.repo.split('/');
  const products = r.products.map((p) => `${p.name}${p.enabled ? '' : ' (abgewählt)'}`).join(', ');
  return h('button', { type: 'button', id: domId('repo-row', r.key), class: `repo-card ${r.loading && !r.facts ? '' : HEALTH[r.health.state][0]}`, onclick: () => openRepo(r.key) },
    h('span', { class: 'repo-card-head' },
      h('strong', { class: 'ellipsis' }, name), h('span', { class: 'muted small ellipsis' }, owner),
      h('span', { class: 'spacer' }),
      r.loading && r.facts && h('span', { class: 'spinner muted', title: 'wird neu gelesen' }),
      healthChip(r)),
    h('span', { class: 'repo-card-reason' }, mainReason(r)),
    h('span', { class: 'repo-card-meta' }, (r.facts ? [prSummary(r), ciSummary(r), releaseSummary(r)] : []).concat(LOCAL_TEXT[r.local.state], checkedSummary(r))
      .filter(Boolean).map((text) => h('span', null, text))),
    h('span', { class: 'repo-card-rel' }, `Produkte: ${products}${r.website ? ' · Website-Repository' : ''}`));
}

// MARK: Repositories: details

function repoDetail(r) {
  const f = r.facts;
  const access = ok(f?.access);
  const branch = access?.defaultBranch ?? 'Default-Branch';
  const plan = view.repoPlan?.plans.find((p) => p.key === r.key);
  if (f && !r.details && !r.detailsLoading && ok(f.prs)?.items.length) {
    const mark = `${r.key}:${f.checkedAt}`;
    if (!detailsAsked.has(mark)) {
      detailsAsked.add(mark);
      queueMicrotask(() => call('load_repo_details', { key: r.key }));
    }
  }
  return h('div', { class: 'repo-detail' },
    h('div', { class: 'repo-detail-head' },
      button('Alle Repositories', closeRepo, { id: 'repo-back', iconName: 'back' }),
      h('h2', { class: 'ellipsis', title: r.repo }, r.repo), healthChip(r),
      h('span', { class: 'spacer' }),
      button(r.loading ? 'Wird gelesen …' : 'Status aktualisieren', () => call('refresh_repos', { keys: [r.key] }),
        { id: 'repo-refresh', iconName: 'refresh', disabled: r.loading, title: 'Nur lesend, ohne Claude' }),
      access?.url && button('Auf GitHub', () => call('open_url', { url: access.url }), { id: 'repo-github', iconName: 'external' })),
    findings(r),
    h('section', { class: 'panel section' }, h('h3', null, 'Status'), statusFacts(r, branch)),
    h('section', { class: 'panel section' }, prSection(r, branch, plan)),
    h('section', { class: 'panel section' }, careSection(r, plan)));
}

function findings(r) {
  const { state, reasons, gaps, notes } = r.health;
  const list = (title, items, cls) => items.length > 0 && h('div', { class: 'finding' },
    h('strong', { class: `small ${cls}` }, title), h('ul', null, items.map((text) => h('li', { class: 'selectable' }, text))));
  const blocks = [
    list('Befunde', reasons, state === 'blocked' ? 'red' : 'orange'),
    list('Datenlücken', gaps, ''),
    list('Hinweise', notes, 'muted'),
    r.error && h('div', { class: 'note orange' }, `Letzte Abfrage fehlgeschlagen, gezeigt wird der ältere Stand: ${r.error}`),
  ].filter(Boolean);
  if (!reasons.length && !gaps.length && r.facts) blocks.unshift(h('div', { class: 'note green' }, 'Alles gelesen, kein Befund.'));
  return h('div', { class: 'findings' }, blocks);
}

function fetchText(fetch, render) {
  if (!fetch) return 'noch nicht gelesen';
  if (fetch.state === 'failed') return `Abfrage fehlgeschlagen: ${fetch.value}`;
  if (fetch.state === 'notQueried') return `nicht abgefragt${fetch.value ? `: ${fetch.value}` : ''}`;
  return render(fetch.value);
}

function releaseText(f) {
  if (f.release.state === 'failed') return `Abfrage fehlgeschlagen: ${f.release.value}`;
  if (ok(f.release)) return `${f.release.value} (veröffentlichtes Release)`;
  if (f.release.state !== 'ok') return 'nicht abgefragt';
  if (ok(f.tag)) return `kein veröffentlichtes Release · höchster passender Tag ${f.tag.value}`;
  if (f.tag.state === 'failed') return `kein veröffentlichtes Release · Tags: ${f.tag.value}`;
  return 'kein veröffentlichtes Release, kein passender Tag';
}

function localText(local) {
  switch (local.state) {
    case 'found': return local.path;
    case 'missing': return `fehlt: ${local.path}`;
    case 'foreign': return `${local.path} ist keine Kopie dieses Repositorys (${local.origin ?? 'ohne GitHub-origin'})`;
    case 'conflict': return `Produkte nennen verschiedene Ordner: ${local.paths.join(', ')}`;
    default: return 'nicht eingetragen (localPath in site.json)';
  }
}

function statusFacts(r, branch) {
  const f = r.facts;
  const access = ok(f?.access);
  const row = (label, value, cls = '') => [h('span', { class: 'label' }, label), h('span', { class: `detail-text selectable ${cls}` }, value)];
  const since = f?.since;
  const sinceLabel = ok(since) ? `Commits seit ${since.value.baseKind === 'release' ? 'Release' : 'Tag'} ${since.value.base}` : 'Commits seit Release';
  return h('div', { class: 'fact-grid' },
    row('Repository', access ? [access.private ? 'privat' : 'öffentlich', access.archived && 'archiviert'].filter(Boolean).join(' · ') : fetchText(f?.access, () => '')),
    access && row('Schreibzugriff', access.canPush == null ? 'unbekannt (ohne gh-Anmeldung meldet GitHub keine Rechte)' : access.canPush ? 'ja' : 'nein, nur lesbar', access.canPush === false ? 'orange' : ''),
    row('Default-Branch', access ? `${branch}${ok(f.head) ? ` · HEAD ${short(f.head.value)}` : ''}` : '–'),
    row('CI', fetchText(f?.ci, (ci) => `${CI_TEXT[ci.state]}: ${ci.detail} (Commit ${short(ci.sha)})`),
      ok(f?.ci)?.state === 'failed' ? 'red' : ok(f?.ci)?.state === 'passed' ? 'green' : ''),
    row('Release', f ? releaseText(f) : '–'),
    row(sinceLabel, !since ? '–' : ok(since) ? String(since.value.commits) : since.state === 'failed' ? since.value : `nicht bestimmbar${since.value ? `: ${since.value}` : ''}`),
    row('Tag-Präfix', r.tagConflict ?? (r.tagPrefix ? `„${r.tagPrefix}“` : 'ohne'), r.tagConflict ? 'orange' : ''),
    row('Lokale Kopie', localText(r.local), ['found', 'notConfigured'].includes(r.local.state) ? '' : 'orange'),
    row('Website', [...r.products.map((p) => `${p.name}: dokumentiert ${p.documented}${p.enabled ? '' : ', nicht ausgewählt'}`),
      r.website && 'zugleich das Website-Repository'].filter(Boolean).join(' · ')),
    row('Gelesen', f ? `${formatTime(f.checkedAt)} (${ago(f.checkedAt)})${r.health.stale ? ', veraltet' : ''}` : 'noch nicht', r.health.stale ? 'orange' : ''));
}

const PLANNED = { merge: 'mergen', close: 'schließen', rebase: 'Rebase anstoßen', keep: 'bleibt offen' };

function prSection(r, branch, plan) {
  const prs = r.facts?.prs;
  const list = ok(prs);
  const head = (info) => h('div', { class: 'section-head wrap' }, h('h3', null, 'Pull Requests'), info, h('span', { class: 'spacer' }),
    list?.items.length > 0 && button(r.detailsLoading ? 'Wird gelesen …' : 'CI und Mergebarkeit neu lesen', () => call('load_repo_details', { key: r.key }),
      { id: 'repo-details', iconName: 'refresh', disabled: r.detailsLoading }));
  if (!list) return [head(), h('div', { class: 'note' }, fetchText(prs, () => ''))];
  if (!list.items.length) return [head(), h('div', { class: 'note' }, 'Keine offenen Pull Requests.')];
  const dependabot = list.items.filter((p) => p.dependabot).length;
  const intoDefault = list.items.filter((p) => p.intoDefault).length;
  const details = new Map((r.details?.prs ?? []).map((d) => [d.number, d]));
  return [
    head(h('span', { class: 'muted small' },
      `${list.complete ? '' : 'mindestens '}${list.items.length} offen, davon ${dependabot} von Dependabot; ${intoDefault} auf ${branch} kommen für die Pflege in Frage`)),
    r.detailsLoading && !r.details && h('div', { class: 'note' }, 'CI und Mergebarkeit werden gelesen …'),
    r.details && !r.details.complete && h('div', { class: 'note' }, `CI und Mergebarkeit gelesen für die ersten ${r.details.prs.length}.`),
    h('div', { class: 'items' }, list.items.map((pr) => prItem(pr, details.get(pr.number), plan?.prs.find((p) => p.number === pr.number), branch))),
  ].filter(Boolean);
}

function prItem(pr, detail, planned, branch) {
  const ci = detail?.ci;
  const ciBadge = !detail ? null : ok(ci)
    ? badge({ passed: 'green', failed: 'red', pending: '', noChecks: 'orange', unknown: 'orange' }[ci.value.state], `CI ${CI_TEXT[ci.value.state]}`, ci.value.detail)
    : badge('orange', 'CI nicht abfragbar', ci?.value);
  const merge = detail && badge(['clean', 'has_hooks'].includes(detail.mergeableState) ? 'green' : detail.mergeableState === 'dirty' ? 'red' : '', detail.mergeable);
  const risk = planned?.risk && RISKS[planned.risk];
  return h('div', { class: 'item' },
    h('div', { class: 'item-line' },
      button(`#${pr.number}`, () => call('open_url', { url: pr.url }), { cls: 'link mono', title: 'Auf GitHub öffnen' }),
      h('span', { class: 'item-title', title: pr.title }, pr.title),
      pr.dependabot ? badge('accent', 'Dependabot') : h('span', { class: 'muted small' }, pr.author),
      pr.draft && badge('', 'Entwurf'),
      h('span', { class: 'spacer' }),
      ciBadge, merge),
    h('div', { class: 'item-reason' }, [
      pr.intoDefault ? `Ziel ${pr.base}` : `Ziel ${pr.base}, nicht ${branch}: kommt nicht in den Pflegeplan`,
      planned && `Vorschau: ${PLANNED[planned.action]}${risk ? `, Claude: ${risk[1]}` : ''}`,
      !detail && 'CI und Mergebarkeit noch nicht gelesen',
    ].filter(Boolean).join(' · ')));
}

/** The care options; `onChange` runs after each change, the model row hides without a merge. */
function careFields(onChange, idPrefix) {
  const modelId = `${idPrefix}-model`;
  const modelRow = h('div', { class: 'field-row', hidden: !careOptions.merge }, h('label', { for: modelId }, 'Modell für die Prüfung'),
    modelSelect(careOptions.model, view.site?.claude?.model, (model) => { careOptions.model = model; }, modelId));
  const option = (key, title, note) => h('label', { class: 'check-line top' },
    h('input', { type: 'checkbox', id: `${idPrefix}-${key}`, checked: careOptions[key],
      onchange: (e) => { careOptions[key] = e.target.checked; modelRow.hidden = !careOptions.merge; onChange(); } }),
    h('span', null, h('strong', null, title), h('div', { class: 'note' }, note)));
  return [
    option('merge', 'Offene PRs prüfen, mergen und Patch-Tag setzen',
      'Claude bewertet jeden offenen PR auf den Default-Branch nach den Regeln des dependabot-pr-Skills (Risikoklasse, Release-Notes, SHA-Pins, zusammengehörige Abhängigkeiten, Nutzung im Code); das kostet Claude-Nutzung. Gemergt wird nur, was GitHub mergebar mit grüner CI meldet und Claude grün oder gelb einstuft. Überflüssige PRs werden mit Begründung geschlossen, Dependabot-PRs mit Konflikt neu aufgebaut. Den Tag x.y.(z+1) setzt die App nur, wenn alle geplanten Merges gelingen und die CI genau dieses Commits grün ist.'),
    modelRow,
    option('cleanup', 'Gemergte Branches löschen',
      'Branches, deren Commits vollständig im Default-Branch sind oder deren PR gemergt wurde, auf GitHub und in der lokalen Kopie. Geschützte, ausgecheckte (auch in Worktrees) und Branches offener PRs bleiben.'),
  ];
}

function careSection(r, plan) {
  const locked = r.capabilities.care;
  const blocked = () => !!view.running || !!locked || (!careOptions.merge && !careOptions.cleanup);
  const check = button('Dieses Repository prüfen', () => checkRepos([r.key]),
    { id: domId('care-check', r.key), cls: 'primary', iconName: 'play', disabled: blocked() });
  return [
    h('div', { class: 'section-head wrap' }, h('h3', null, 'Pflege'), h('span', { class: 'muted small' }, 'erst Vorschau, ausgeführt wird nach Bestätigung')),
    locked && h('div', { class: 'note orange' }, `Pflege gesperrt: ${locked}. Status und Details bleiben lesbar.`),
    r.capabilities.tag && h('div', { class: 'note orange' }, `Kein automatischer Tag: ${r.capabilities.tag}.`),
    r.local.state !== 'notConfigured' && r.capabilities.local && h('div', { class: 'note orange' }, `Lokale Branches bleiben unberührt: ${r.capabilities.local}.`),
    plan && h('div', { class: 'note' }, 'Die Vorschau für dieses Repository steht oben unter „Pflege“.'),
    ...careFields(() => { check.disabled = blocked(); }, domId('care', r.key)),
    h('div', { class: 'line' }, check, view.running && h('span', { class: 'note' }, 'Erst nach dem laufenden Schritt.')),
  ].filter(Boolean);
}

async function checkRepos(keys) {
  await invoke('check_repos', { keys, merge: careOptions.merge, cleanup: careOptions.cleanup, model: careOptions.model });
  careTab = 'log';
  document.getElementById('main').scrollTop = 0;
}

/** Care for the repositories of the selected products: names them first, without the locked ones. */
function careDialog(chosen) {
  const allowed = chosen.filter((r) => !r.capabilities.care);
  const locked = chosen.filter((r) => r.capabilities.care);
  const s = { error: '' };
  const blocked = () => !allowed.length || (!careOptions.merge && !careOptions.cleanup) || !!view.running;
  const entry = (r, extra) => h('li', null, h('span', { class: 'mono' }, r.repo), ' ', h('span', { class: 'muted small' }, extra));
  openDialog('Repositories pflegen', (close, render) => ({
    body: [
      h('h2', null, 'Repositories der ausgewählten Produkte pflegen'),
      h('div', null,
        h('strong', { class: 'small' }, allowed.length ? `Geprüft ${allowed.length === 1 ? 'wird' : 'werden'}:` : 'Keins dieser Repositories lässt sich pflegen.'),
        allowed.length > 0 && h('ul', { class: 'plain-list' }, allowed.map((r) => entry(r, r.products.map((p) => p.name).join(', '))))),
      locked.length > 0 && h('div', null, h('strong', { class: 'small' }, 'Ausgenommen:'),
        h('ul', { class: 'plain-list' }, locked.map((r) => entry(r, r.capabilities.care)))),
      ...careFields(() => { const b = document.getElementById('care-dialog-check'); if (b) b.disabled = blocked(); }, 'care-dialog'),
      h('div', { class: 'note' }, 'Zuerst entsteht nur eine Vorschau; ausgeführt wird erst, wenn du sie bestätigst. Die Produktauswahl der Website bleibt, wie sie ist.'),
      s.error && h('div', { class: 'note error-text' }, s.error),
    ],
    actions: [
      button('Abbrechen', close),
      button('Prüfen', async () => {
        try {
          await checkRepos(allowed.map((r) => r.key));
          close();
        } catch (error) { s.error = String(error); render(); }
      }, { id: 'care-dialog-check', cls: 'primary', disabled: blocked() }),
    ],
  }));
}

// MARK: Repositories: care panel

function renderCare() {
  const state = view.steps.repos;
  const running = view.running === 'repos';
  const visible = running || !!view.repoPlan || view.repoReports.length > 0 || state.status !== 'idle';
  reposUi.care.hidden = !visible;
  if (!visible) return;
  reposUi.careHead.replaceChildren(...[
    h('h3', null, 'Pflege'), statusChip(state.status),
    state.summary && h('span', { class: `detail-summary ${state.status === 'error' ? 'red' : state.status === 'warning' ? 'orange' : ''}` }, state.summary),
    h('span', { class: 'spacer' }),
    h('span', { class: 'muted small' }, state.finishedAt ? `zuletzt ${formatTime(state.finishedAt)}` : ''),
    running && button('Abbrechen', () => invoke('cancel'), { id: 'care-cancel', iconName: 'stop', cls: 'danger' }),
  ].filter(Boolean));
  const content = view.repoPlan ? planView(view.repoPlan)
    : view.repoReports.length ? [h('div', { class: 'note' }, 'Erledigtes bleibt bestehen, auch nach Fehlern oder Abbruch. Für Übersprungenes eine neue Vorschau erstellen.'), reportView(view.repoReports)]
      : [h('div', { class: 'note' }, running ? 'Läuft gerade. Die Ausgabe steht im ' : 'Kein Ergebnis. Was passiert ist, steht im ', button('Log', () => showCareTab('log'), { cls: 'link' }), '.')];
  reposUi.careResult.replaceChildren(...content.filter(Boolean));
  renderCareTabs();
}

function renderCareTabs() {
  const log = careTab === 'log';
  const showsLog = !reposUi.careLog.hidden;
  for (const [tab, on] of [[reposUi.careTabResult, !log], [reposUi.careTabLog, log]]) {
    tab.setAttribute('aria-selected', String(on));
    tab.tabIndex = on ? 0 : -1;
  }
  reposUi.careTabLog.replaceChildren(view.running === 'repos' ? h('span', { class: 'spinner' }) : '', 'Log');
  reposUi.careResult.hidden = log;
  reposUi.careLog.hidden = !log;
  if (log && !showsLog) reposUi.careLog.scrollTop = reposUi.careLog.scrollHeight;
}

function showCareTab(tab) {
  if (careTab === tab) return;
  careTab = tab;
  renderCareTabs();
}

function careTabKey(e) {
  if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(e.key)) return;
  e.preventDefault();
  const next = e.key === 'Home' ? 'result' : e.key === 'End' ? 'log' : careTab === 'log' ? 'result' : 'log';
  showCareTab(next);
  (next === 'log' ? reposUi.careTabLog : reposUi.careTabResult).focus();
}

// Which repository blocks the user opened or closed; survives re-rendering.
const repoOpen = new Map();

/** A collapsible block per repository, as the Claude runs of an update. */
function repoDetails(key, open, summary, body) {
  return h('details', { class: 'run repo', open: repoOpen.get(key) ?? open, ontoggle: (e) => repoOpen.set(key, e.target.open) },
    h('summary', null, summary), h('div', { class: 'run-body' }, body));
}

// What the app checks again right before each kind of write.
const APP_CHECK = {
  merge: 'App-Prüfung direkt davor: noch offen, gleicher Commit und Ziel, mergebar, CI des PR-Commits grün; GitHub bekommt den erwarteten Commit mit.',
  close: 'App-Prüfung direkt davor: noch offen, gleicher Commit und Ziel; ein genannter Ersatz-PR muss offen oder gemergt sein. Kommentar und Schließen werden einzeln gemeldet.',
  rebase: 'App-Prüfung direkt davor: noch offen, von Dependabot, gleicher Commit, weiterhin Konflikt oder Rückstand.',
};

function planPr(pr) {
  const risk = pr.risk && RISKS[pr.risk];
  return h('div', { class: 'item' },
    h('div', { class: 'item-line' },
      button(`#${pr.number}`, () => call('open_url', { url: pr.url }), { cls: 'link mono', title: 'Auf GitHub öffnen' }),
      h('span', { class: 'item-title', title: pr.title }, pr.bump ? `${pr.title.replace(/ from .* to .*$/, '')} ${pr.bump}` : pr.title),
      !pr.dependabot && h('span', { class: 'muted small' }, pr.author),
      h('span', { class: 'spacer' }),
      risk && badge(risk[0], `Claude: ${risk[1]}`)),
    pr.reason && h('div', { class: 'item-reason selectable' }, pr.reviewed ? `Claude: ${pr.reason}` : pr.reason),
    pr.supersededBy && h('div', { class: 'item-reason' }, `Ersetzt durch #${pr.supersededBy}`),
    APP_CHECK[pr.action] && h('div', { class: 'item-check' }, APP_CHECK[pr.action]),
    pr.action === 'merge' && pr.dependabot && h('div', { class: 'item-check' }, 'Bedingt: Verursacht ein vorheriger Merge dieses Laufs einen Konflikt, stößt die App statt des Merges @dependabot rebase an.'));
}

function branchItems(branches) {
  return branches.map((b) => h('div', { class: 'item' },
    h('div', { class: 'item-line' }, h('span', { class: 'mono item-title', title: b.name }, b.name), h('span', { class: 'spacer' }),
      b.proof.kind === 'afterMerge' && badge('', 'bedingt')),
    h('div', { class: 'item-reason' }, `${b.reason}${b.proof.kind === 'afterMerge' ? '; nur wenn dieser Merge gelingt' : ''}. Vorher: gleicher Commit, nicht geschützt, kein offener PR, Nachweis gilt noch.`)));
}

/** Badges for the closed block: what happens in the repository. */
function planBadges(p) {
  const count = (action) => p.prs.filter((pr) => pr.action === action).length;
  const branches = p.branches.length + p.localBranches.length;
  return [
    p.error && badge('red', 'Fehler'),
    count('merge') && badge('green', `${count('merge')} mergen`),
    count('close') && badge('red', `${count('close')} schließen`),
    count('rebase') && badge('accent', `${count('rebase')} Rebase`),
    count('keep') && badge('', `${count('keep')} offen`),
    p.tag && badge('green', `Tag ${p.tag}`),
    branches && badge('', `${branches} Branches`),
  ].filter(Boolean);
}

function repoPlan(p, open) {
  const group = (title, items) => items.length > 0 && [h('div', { class: 'item-group' }, `${title} (${items.length})`), ...items];
  const prs = (action) => p.prs.filter((pr) => pr.action === action).map(planPr);
  const tag = p.tag ? [h('div', { class: 'item' },
    h('div', { class: 'item-line' }, h('strong', null, p.tag), h('span', { class: 'muted small' }, `nach ${p.lastTag}`), h('span', { class: 'spacer' }), badge('', 'bedingt')),
    h('div', { class: 'item-check' }, `Nur wenn alle geplanten Merges gelingen, ${p.defaultBranch} sich sonst nicht bewegt und die CI genau dieses Commits grün ist (bis zu 30 Minuten Warten); ohne CI-Checks kein Tag. Startet gegebenenfalls den Release-Workflow des Repositorys; veröffentlicht ist damit noch nichts.`))] : [];
  return repoDetails(p.key, open,
    [h('strong', null, p.name), h('span', { class: 'muted small mono' }, p.repo), h('span', { class: 'spacer' }), ...planBadges(p)],
    [
      p.error && h('div', { class: 'note error-text' }, p.error),
      p.reviewNote && h('div', { class: 'note orange' }, p.reviewNote),
      h('div', { class: 'items' },
        group('Mergen', prs('merge')),
        group('Schließen', prs('close')),
        group('Rebase anstoßen', prs('rebase')),
        group('Tag', tag),
        !p.tag && p.tagNote && [h('div', { class: 'item-group' }, 'Tag'), h('div', { class: 'item-reason' }, `Kein Tag: ${p.tagNote}`)],
        group('Aufräumen auf GitHub', branchItems(p.branches)),
        p.localSync && [h('div', { class: 'item-group' }, 'Lokale Kopie abgleichen'),
          h('div', { class: 'item-reason' }, `git fetch --prune origin vor dem Löschen, weil ${p.localSync}. Danach prüft die App nur die hier genannten Branches erneut; neu entdeckte brauchen eine neue Vorschau.`)],
        group('Aufräumen lokal', branchItems(p.localBranches)),
        group('Bleibt offen', prs('keep'))),
      p.cleanupNote && h('div', { class: 'note orange' }, p.cleanupNote),
      p.cloneNote && h('div', { class: 'note orange' }, `Lokale Branches übersprungen: ${p.cloneNote}`),
      p.githubDeletesMerged && p.prs.some((pr) => pr.action === 'merge') && h('div', { class: 'note' }, 'GitHub löscht die Branches gemergter PRs in diesem Repository selbst.'),
      !p.error && !p.prs.length && !p.tag && !p.tagNote && !p.branches.length && !p.localBranches.length && h('div', { class: 'note' }, 'Nichts zu tun.'),
    ]);
}

function planView(set) {
  const plans = set.plans;
  const total = (pick) => plans.reduce((sum, p) => sum + pick(p), 0);
  const count = (action) => total((p) => p.prs.filter((pr) => pr.action === action).length);
  const parts = [[count('merge'), 'PR mergen', 'PRs mergen'], [count('close'), 'PR schließen', 'PRs schließen'],
    [count('rebase'), 'Rebase', 'Rebases'], [total((p) => (p.tag ? 1 : 0)), 'Tag', 'Tags'],
    [total((p) => p.branches.length + p.localBranches.length), 'Branch löschen', 'Branches löschen']]
    .filter(([n]) => n > 0).map(([n, one, many]) => `${n} ${n === 1 ? one : many}`);
  const cost = total((p) => p.reviewCost ?? 0);
  const busy = !!view.running;
  return [
    h('div', { class: 'plan-bar' },
      h('div', null, h('strong', null, `Vorschau von ${formatTime(set.createdAt)}`), h('div', { class: 'muted small' },
        `${parts.join(' · ') || 'Nichts zu tun'}${cost ? ` · Claude-Prüfung ${usd(cost)}` : ''}. Vor jeder Aktion prüft die App erneut; was sich geändert hat, wird übersprungen.`)),
      h('span', { class: 'spacer' }),
      button('Verwerfen', () => call('discard_repo_plan', { planId: set.id }), { id: 'plan-discard', disabled: busy }),
      button('Ausführen', () => call('run_repos', { planId: set.id }), { id: 'plan-run', cls: 'primary', iconName: 'play', disabled: busy || !parts.length })),
    view.repoPlanMoved.length > 0 && h('div', { class: 'note orange' },
      `Seit der Vorschau hat sich der Default-Branch bewegt: ${view.repoPlanMoved.join(', ')}. Die Ausführung prüft jede Aktion neu, einen Tag setzt sie dort aber nicht. Für einen Tag neu prüfen.`),
    h('div', { class: 'runs' }, plans.map((p) => repoPlan(p, plans.length === 1))),
  ];
}

const OUTCOMES = { done: ['ok', 'Erledigt'], skipped: ['warning', 'Übersprungen'], failed: ['error', 'Fehlgeschlagen'], cancelled: ['idle', 'Abgebrochen'] };

function reportView(reports) {
  return h('div', { class: 'runs' }, reports.map((r) => {
    const open = r.items.filter((c) => c.outcome !== 'done').length;
    return repoDetails(`report:${r.repo}`, open > 0 || reports.length === 1,
      [h('strong', null, r.name), h('span', { class: 'muted small mono' }, r.repo), h('span', { class: 'spacer' }),
        badge(open ? 'orange' : 'green', open ? `${open} nicht erledigt` : `${r.items.length} erledigt`)],
      r.items.length ? h('div', { class: 'items' }, r.items.map((c) => {
        const [cls, text] = OUTCOMES[c.outcome];
        return h('div', { class: 'item' },
          h('div', { class: 'item-line' }, h('strong', { class: 'small' }, c.label), h('span', { class: 'spacer' }), h('span', { class: `chip ${cls}` }, text)),
          h('div', { class: 'item-reason selectable' }, c.detail));
      })) : h('div', { class: 'note' }, 'Nichts zu tun.'));
  }));
}

// MARK: Log

function clearLog() {
  if (skeleton) skeleton.log.replaceChildren();
}

function appendTo(log, lines) {
  const atBottom = log.scrollHeight - log.scrollTop - log.clientHeight < 40;
  log.append(...lines.map((l) => h('span', { class: `l-${l.kind}` }, `${l.text}\n`)));
  if (atBottom) log.scrollTop = log.scrollHeight;
}

function appendLog(lines) {
  if (skeleton) appendTo(skeleton.log, lines);
}

async function loadLog(step) {
  const lines = await invoke('get_log', { step });
  if (step !== selected || !skeleton) return;
  clearLog();
  appendLog(lines);
}

function clearCareLog() {
  if (reposUi) reposUi.careLog.replaceChildren();
}

async function loadCareLog() {
  const lines = await invoke('get_log', { step: 'repos' });
  if (!reposUi) return;
  clearCareLog();
  appendTo(reposUi.careLog, lines);
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
    updateCheck: !c.noUpdateCheck,
  };
  const input = () => ({
    projectDir: s.projectDir, workspaceDir: s.workspaceDir, host: s.host, port: Number(s.port) || 22, user: s.user, remoteDir: s.remoteDir,
    auth: s.auth, keyPath: s.keyPath, secret: s.secret, updateCheck: s.updateCheck,
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
            view.site && sameProject ? `${view.siteFile} gefunden: ${view.site.name}, Build „${view.site.build}“, Ausgabe ${view.site.output}`
              : view.siteError && sameProject ? view.siteError
                : 'Der Projektordner braucht eine Datei .openreleasepilot/site.json (siehe README); geprüft wird beim Sichern.'),
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
        h('fieldset', null, h('legend', null, 'App'),
          h('label', { class: 'check-line' },
            h('input', { type: 'checkbox', checked: s.updateCheck, onchange: (e) => { s.updateCheck = e.target.checked; } }),
            'Beim Start nach neuen Versionen suchen'),
          h('div', { class: 'note' }, 'Fragt das neueste Release auf GitHub ab. Installiert wird nur auf Klick, nach Prüfung der SHA-256-Prüfsumme, die GitHub für das Paket veröffentlicht. Manuell jederzeit über „Nach Updates suchen“ unten.')),
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
          // A command without a return value resolves to null, so success is "no exception".
          try {
            await invoke('run_add_product', { input: { repo: info.fullName, name: s.name, localPath: s.localPath, instructions: s.instructions, model: s.model } });
            close();
          } catch (error) { s.error = String(error); render(); }
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
    if (event.payload.step === 'repos') {
      if (reposUi) appendTo(reposUi.careLog, [event.payload.line]);
    } else if (event.payload.step === selected) appendLog([event.payload.line]);
  });
  await refresh();
  // Ages and "veraltet" move on without an event; a fresh view keeps them right. Reads nothing remote.
  setInterval(() => { if (area === 'repos' && document.visibilityState === 'visible') refresh(); }, 30000);
}

start();

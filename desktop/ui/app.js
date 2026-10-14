// OpenReleasePilot UI: renders the view the Rust side sends and calls its commands. No framework, no build step.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const STEPS = [
  { id: 'precheck', title: 'Prüfen', desc: 'Repo, Werkzeuge, Releases, SFTP und Live-Seite. Ändert nichts.' },
  { id: 'update', title: 'Inhalte aktualisieren', desc: 'Claude passt die Seite mit den gewählten Aufgaben an, dann Build und lokaler Commit.' },
  { id: 'upload', title: 'Veröffentlichen', desc: 'Build, Push, dann nur geänderte Dateien per SFTP auf den Server.' },
  { id: 'verify', title: 'Live-Seite prüfen', desc: 'Vergleicht die Live-Seite Datei für Datei mit dem Build.' },
];
const stepTitle = (id) => STEPS.find((s) => s.id === id)?.title ?? id;
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
// Main area: 'repos' (start), 'runs' (care jobs, shown as "Aufträge") or 'website' (the optional website's four steps).
let area = 'repos';
let selected = 'precheck';
// Tab of the website's detail panel: 'result' or 'log'.
let detailTab = 'result';
// Repository whose details are open; null = the list. The list's scroll position for the way back.
let repoKey = null;
let repoListScroll = 0;
// Job whose details are open in "Aufträge"; its tab; the list's scroll position.
let jobId = null;
let jobTab = 'result';
let jobListScroll = 0;
// Repositories chosen for the next job; independent of the website's product selection.
const selection = new Set();
let reposRead = false;
let lastRunning = null;
let lastJobRunning = null;
let hostKeyOpen = false;

// MARK: Helpers

const PROPS = new Set(['value', 'checked', 'disabled', 'selected', 'hidden', 'required', 'indeterminate']);

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
  check: '<path d="M5 12l5 5 9-10"/>',
  shield: '<path d="M12 3l8 3v6c0 5-3.5 8-8 9-4.5-1-8-4-8-9V6z"/>',
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
const repoName = (key) => view.repositories.find((r) => r.key === key)?.repo ?? key;

async function call(command, args) {
  try {
    return await invoke(command, args);
  } catch (error) {
    showAlert(String(error));
    return null;
  }
}

/** Runs a command; on success returns true, on error shows it and returns false. */
async function act(command, args) {
  try {
    await invoke(command, args);
    return true;
  } catch (error) {
    showAlert(String(error));
    return false;
  }
}

// Events come in bursts while a job runs; overlapping requests for a view are merged into one.
let refreshing = null;
let refreshAgain = false;

async function refresh() {
  if (refreshing) {
    refreshAgain = true;
    return refreshing;
  }
  refreshing = (async () => {
    do {
      refreshAgain = false;
      setView(await invoke('get_view'));
    } while (refreshAgain);
  })();
  try {
    await refreshing;
  } finally {
    refreshing = null;
  }
}

// MARK: Render

const JOB_FINISHED = new Set(['succeeded', 'partial', 'failed', 'cancelled']);
const openJobs = () => view.jobs.filter((j) => !JOB_FINISHED.has(j.status.state));

function setView(next) {
  view = next;
  if (!view.config.projectDir && area === 'website') area = 'repos';
  if (view.running && view.running !== lastRunning && view.running !== 'jobs') {
    // Follow the website step that just started and watch its log.
    selected = view.running;
    detailTab = 'log';
    loadLog(selected);
  } else if (!view.running && lastRunning === selected && hasResult(selected)) {
    // The step is done: show what came out of it. Without a result the log has the details.
    detailTab = 'result';
  }
  if (lastJobRunning && !view.jobRunning && lastJobRunning === jobId) jobTab = 'result';
  lastRunning = view.running;
  lastJobRunning = view.jobRunning;
  for (const key of [...selection]) if (!view.repositories.some((r) => r.key === key)) selection.delete(key);
  renderToolbar();
  renderBanners();
  renderMain();
  renderFooter();
  if (view.pendingHostKey && !hostKeyOpen) hostKeyDialog();
}

function renderToolbar() {
  const attention = view.repositories.filter((r) => r.health.state === 'blocked' || r.health.state === 'needsAttention').length;
  const decisions = view.jobs.filter((j) => j.status.state === 'needsApproval').length;
  const nav = (id, label, extra) => h('button', {
    type: 'button', class: `nav-item ${area === id ? 'active' : ''}`, 'aria-current': area === id ? 'page' : null,
    onclick: () => showArea(id),
  }, label, extra);
  const badge = (n, text, cls = '') => n > 0 && h('span', { class: `nav-badge ${cls}`, title: text, 'aria-label': text }, n);
  const where = [
    view.repositories.length === 1 ? '1 Repository' : `${view.repositories.length} Repositories`,
    view.site ? `Website ${view.site.name}` : view.config.projectDir ? 'Website nicht lesbar' : 'ohne Website',
  ].join(' · ');
  const started = view.repositories.length > 0 || !!view.config.projectDir;
  document.getElementById('toolbar').replaceChildren(...[
    h('div', { class: 'brand' },
      h('img', { src: 'icon.svg', alt: '' }),
      h('div', null, h('h1', null, 'OpenReleasePilot'), h('div', { class: 'project' }, where))),
    started && h('nav', { class: 'nav', 'aria-label': 'Bereiche' },
      nav('repos', 'Repositories', badge(attention, `${attention} brauchen Aufmerksamkeit`)),
      nav('runs', 'Aufträge', badge(decisions, `${decisions} warten auf eine Entscheidung`, 'decision')),
      view.config.projectDir && nav('website', 'Website')),
    h('span', { class: 'spacer' }),
    area === 'website' && view.site && button(view.site.url.replace(/^https?:\/\//, ''), () => call('open_url', { url: view.site.url }), { iconName: 'external', cls: 'link', title: 'Live-Seite öffnen' }),
    area === 'website' && view.site && button('Produkt zur Website hinzufügen …', addProductDialog,
      { iconName: 'plus', disabled: !!view.running, title: 'Ein Repository als neues Produkt auf der Website aufnehmen' }),
    button('Einstellungen', settingsDialog, { iconName: 'gear', disabled: !!view.running, title: 'Zugänge, Ausführungsregeln, optionale Website' }),
  ].filter(Boolean));
}

function showArea(id) {
  const main = document.getElementById('main');
  if (area === id) {
    if (id === 'repos' && repoKey) closeRepo();
    if (id === 'runs' && jobId) closeJob();
    return;
  }
  if (area === 'repos' && !repoKey) repoListScroll = main.scrollTop;
  if (area === 'runs' && !jobId) jobListScroll = main.scrollTop;
  area = id;
  renderToolbar();
  renderBanners();
  renderMain();
  if (id === 'repos') main.scrollTop = repoKey ? 0 : repoListScroll;
  if (id === 'runs') main.scrollTop = jobId ? 0 : jobListScroll;
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
  if (view.jobsUnreadable.length) items.push(banner('warning', `Gespeicherte Aufträge nicht lesbar: ${view.jobsUnreadable.join('; ')}`));
  if (view.signedIn === false && view.repositories.length) {
    items.push(banner('warning', 'Die GitHub-CLI ist nicht angemeldet: Status nur für öffentliche Repositories, Pflege nicht möglich. Im Terminal: gh auth login'));
  }
  // The website's own warnings only where the website is worked on.
  if (area === 'website' && view.siteError) items.push(banner('warning', `Website: ${view.siteError}`, [['Einstellungen', settingsDialog]]));
  if (area === 'website' && view.site && !view.sftpReady) items.push(banner('warning', 'SFTP ist noch nicht eingerichtet: Veröffentlichen ist erst danach möglich.', [['Einstellungen', settingsDialog]]));
  document.getElementById('banners').replaceChildren(...items);
}

let skeleton = null;
let reposUi = null;
let runsUi = null;

function renderMain() {
  const main = document.getElementById('main');
  if (!view.repositories.length && !view.config.projectDir) {
    skeleton = null;
    reposUi = null;
    runsUi = null;
    area = 'repos';
    main.replaceChildren(h('div', { class: 'empty' },
      h('img', { src: 'icon.svg', alt: '' }),
      h('h2', null, 'Willkommen'),
      h('p', { class: 'muted' }, 'OpenReleasePilot pflegt die Repositories, die du auswählst: Zustand, Pull Requests, Releases, README und Wiki. Du triffst die Entscheidungen, die App erledigt die Arbeit. Eine Website ist optional und kann später dazukommen.'),
      h('div', { class: 'line' },
        button('Repositories auswählen …', addReposDialog, { cls: 'primary', iconName: 'plus' }),
        button('Website einrichten (optional) …', settingsDialog))));
    return;
  }
  if (!reposUi) {
    buildRepos();
    buildRuns();
  }
  if (view.config.projectDir && !skeleton) buildWebsite();
  if (!view.config.projectDir && skeleton) skeleton = null;
  const roots = [reposUi.root, runsUi.root, skeleton?.root].filter(Boolean);
  if (main.children.length !== roots.length || roots.some((r, i) => main.children[i] !== r)) main.replaceChildren(...roots);
  reposUi.root.hidden = area !== 'repos';
  runsUi.root.hidden = area !== 'runs';
  if (skeleton) skeleton.root.hidden = area !== 'website';
  if (area === 'repos') {
    renderRepos();
    readReposOnce();
  } else if (area === 'runs') {
    renderRuns();
  } else if (skeleton) {
    renderWebsite();
  }
}

// MARK: Website (optional)

function buildWebsite() {
  // Built once, so the instruction field keeps focus and the log keeps its scroll position.
  skeleton = {
    steps: h('section', { class: 'steps' }),
    products: h('div', { class: 'list products' }),
    productCount: h('span', { class: 'muted small' }),
    productAll: h('input', { type: 'checkbox', onchange: (e) => call('set_products', { ids: view.products.map((p) => p.id), enabled: e.target.checked }) }),
    tasks: h('div', { class: 'list tasks' }),
    taskTech: h('div', { class: 'checks' }),
    instructions: h('textarea', { id: 'instructions', rows: 1, placeholder: 'z. B. „Ergänze im nudgit-Abschnitt den Hinweis auf die Chrome-Erweiterung“ – leer lassen für ein reines Release-Update' }),
    runAll: null,
    detailHead: h('div', { class: 'detail-head' }),
    tabResult: h('button', { type: 'button', class: 'tab', role: 'tab', id: 'tab-result', 'aria-controls': 'detail-result', onclick: () => showTab('result') }, 'Ergebnis'),
    tabLog: h('button', { type: 'button', class: 'tab', role: 'tab', id: 'tab-log', 'aria-controls': 'log', onclick: () => showTab('log') }),
    detailResult: h('div', { class: 'detail-result', id: 'detail-result', role: 'tabpanel', 'aria-labelledby': 'tab-result' }),
    log: h('pre', { class: 'log', id: 'log', role: 'tabpanel', 'aria-labelledby': 'tab-log', tabindex: '0' }),
  };
  skeleton.runAll = button('Aktualisieren und veröffentlichen', () => call('run_all', { instructions: skeleton.instructions.value }),
    { iconName: 'forward', title: 'Prüfen, Inhalte aktualisieren (wenn nötig), Veröffentlichen und Live-Seite prüfen nacheinander; geht bis zur Live-Seite und stoppt beim ersten Fehler' });
  skeleton.root = h('div', { class: 'area' },
    h('div', { class: 'note' }, 'Der direkte Website-Ablauf. Als letzter Schritt eines Pflegeauftrags läuft dieselbe Website-Aktualisierung mit Vorschau und Freigabe im Bereich Repositories.'),
    h('section', { class: 'section' },
      h('div', { class: 'section-head' }, h('h2', null, 'Ablauf'),
        h('span', { class: 'muted small' }, 'Schritte einzeln starten oder alle nacheinander bis zur Live-Seite'),
        h('span', { class: 'spacer' }), skeleton.runAll),
      skeleton.steps),
    h('div', { class: 'config' },
      h('section', { class: 'section panel' },
        h('div', { class: 'section-head' }, h('h2', null, 'Produkte'), skeleton.productCount, h('span', { class: 'spacer' }),
          h('label', { class: 'check-all', title: 'Alle Produkte an- oder abwählen' }, skeleton.productAll, 'Alle')),
        skeleton.products,
        h('div', { class: 'note' }, 'Nur ausgewählte Produkte werden geprüft und aktualisiert. Welche Repositories die App pflegt, ist davon unabhängig.')),
      h('section', { class: 'section panel' },
        h('div', { class: 'section-head' }, h('h2', null, 'Update-Aufgaben'),
          h('span', { class: 'muted small' }, 'je ein Claude-Lauf, in dieser Reihenfolge')),
        skeleton.tasks,
        h('div', { class: 'field-col' }, h('label', { for: 'instructions' }, 'Zusätzliche Anweisung an Claude (optional)'), skeleton.instructions),
        h('details', { class: 'fold' }, h('summary', null, 'Technische Details der Aufgaben'),
          h('div', { class: 'note' }, 'Skill und Effort stammen aus site.json und lassen sich dort ändern.'), skeleton.taskTech))),
    h('section', { class: 'detail' }, skeleton.detailHead,
      h('div', { class: 'tabs', role: 'tablist', 'aria-label': 'Ansicht', onkeydown: tabKey }, skeleton.tabResult, skeleton.tabLog),
      skeleton.detailResult, skeleton.log));
  loadLog(selected);
}

function renderWebsite() {
  skeleton.runAll.disabled = !!view.running || !view.site;
  skeleton.instructions.disabled = !!view.running;
  renderSteps();
  renderProducts();
  renderTasks();
  renderDetail();
}


function versionBadge(p) {
  const latest = p.latestTag?.replace(/^v/, '');
  if (!latest) return h('span', { class: 'badge', title: 'Dokumentierter Stand; „Prüfen“ fragt das neueste Release ab' }, p.documented);
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
    : [h('div', { class: 'note empty-list' }, 'Noch keine Produkte: „Produkt zur Website hinzufügen …“ in der Kopfleiste nimmt eins auf.')]));
}

function renderTasks() {
  const head = h('div', { class: 'row head' }, h('span', null, 'Aufgabe'), h('span', null, 'Modell'));
  skeleton.taskTech.replaceChildren(...view.tasks.map((t) => [h('span'), h('span', { class: 'label' }, t.label),
    h('span', { class: 'detail-text' }, [t.skill ? `Skill ${t.skill}` : 'ohne Skill', t.effort && `Effort ${t.effort}`].filter(Boolean).join(' · '))]));
  skeleton.tasks.replaceChildren(head, ...view.tasks.map((t) => {
    const save = (enabled, model) => call('set_task', { id: t.id, enabled, model });
    // One row per task; the columns line up across rows (CSS subgrid).
    return h('div', { class: `row ${t.enabled ? 'on' : ''}` },
      h('label', { class: 'row-name' },
        h('input', { type: 'checkbox', checked: t.enabled, disabled: !!view.running, onchange: (e) => save(e.target.checked, t.chosenModel) }),
        h('span', { class: 'row-text' }, h('strong', null, t.label), t.description && h('span', { class: 'muted small' }, t.description))),
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
  if (stale) return h('span', { class: 'chip idle', title: 'Stand vor der letzten Aktualisierung oder Veröffentlichung' }, 'Veraltet');
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
    const summary = state.stale ? 'Nach Aktualisierung oder Veröffentlichung erneut prüfen' : state.summary;
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
  const unknown = view.releaseUnknown.length > 0 && !stale
    && h('div', { class: 'note orange' }, `Release-Stand unklar: ${view.releaseUnknown.join('; ')}. „Inhalte aktualisieren“ und „Aktualisieren und veröffentlichen“ stoppen, bis der Stand bekannt ist.`);
  const outdated = view.outdated?.length
    ? h('div', { class: 'facts' }, h('span', null, h('strong', null, 'Neue Releases: '),
      view.outdated.map((o) => `${o.name} ${o.documented} → ${o.latestTag.replace(/^v/, '')}`).join(', ')))
    : null;
  return [
    stale && h('div', { class: 'note orange' }, 'Dieses Ergebnis stammt von vor der letzten Aktualisierung oder Veröffentlichung (z. B. Remote-Stand und Releases). „Prüfen“ erneut starten für den aktuellen Stand.'),
    outdated && !stale ? outdated : null,
    unknown,
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
  notConfigured: 'keine lokale Kopie eingetragen', found: 'lokale Kopie vorhanden', missing: 'lokale Kopie fehlt', foreign: 'lokale Kopie ungültig',
};
const RISKS = { green: ['green', 'Risiko grün'], yellow: ['orange', 'Risiko gelb'], red: ['red', 'Risiko rot'] };
const JOB_STATE = {
  planned: ['idle', 'Geplant'], preparing: ['running', 'In Vorbereitung'], needsApproval: ['warning', 'Freigabe nötig'], running: ['running', 'Läuft'],
  waiting: ['idle', 'Wartet'], succeeded: ['ok', 'Erfolgreich'], partial: ['warning', 'Teilweise erfolgreich'], blocked: ['error', 'Blockiert'],
  failed: ['error', 'Fehlgeschlagen'], cancelled: ['idle', 'Abgebrochen'],
};
const STEP_STATE = {
  pending: ['idle', 'Geplant'], running: ['running', 'Läuft'], needsApproval: ['warning', 'Freigabe nötig'], approved: ['idle', 'Freigegeben'],
  waiting: ['idle', 'Wartet'], done: ['ok', 'Erledigt'], nothingToDo: ['ok', 'Nichts zu tun'], partial: ['warning', 'Teilweise'],
  skipped: ['warning', 'Übersprungen'], failed: ['error', 'Fehlgeschlagen'], rejected: ['idle', 'Verworfen'], blocked: ['error', 'Blockiert'],
  cancelled: ['idle', 'Abgebrochen'],
};
const badge = (cls, text, title) => h('span', { class: `badge ${cls}`, title }, text);
/** The value of a read part of the facts (`{ state: 'ok', value }`), else undefined. */
const ok = (fetch) => (fetch?.state === 'ok' ? fetch.value : undefined);
/** An id from a key, so focus survives re-rendering. */
const domId = (prefix, key) => `${prefix}-${String(key).replace(/[^a-z0-9]+/gi, '-')}`;
// Pull request details asked for once per reading of a repository.
const detailsAsked = new Set();

function jobChip(status, running) {
  const [cls, text] = JOB_STATE[status.state] ?? ['idle', status.state];
  return h('span', { class: `chip ${cls}`, title: status.reason }, running && h('span', { class: 'spinner' }), text);
}

function stepChip(state) {
  const [cls, text] = STEP_STATE[state] ?? ['idle', state];
  return h('span', { class: `chip ${cls}` }, state === 'running' && h('span', { class: 'spinner' }), text);
}

// Overview filter: a name search and one of the repository states; never changes the selection.
const REPO_FILTERS = [
  ['all', 'Alle', () => true],
  ['attention', 'Aufmerksamkeit', (r) => r.health.state === 'needsAttention'],
  ['blocked', 'Blockiert', (r) => r.health.state === 'blocked'],
  ['unknown', 'Unbekannt', (r) => r.health.state === 'unknown'],
];
let repoQuery = '';
let repoFilter = 'all';

function buildRepos() {
  reposUi = {
    root: h('div', { class: 'area repos-area' }),
    decisions: h('section', { class: 'section', 'aria-label': 'Offene Aufträge' }),
    overview: h('section', { class: 'section' }),
    head: h('div', { class: 'section-head wrap' }),
    note: h('div', { class: 'note' }),
    // Built once, so typing in the search keeps focus and caret.
    search: h('input', {
      type: 'search', id: 'repos-search', placeholder: 'Repositories suchen …', spellcheck: 'false', autocomplete: 'off', 'aria-label': 'Repositories nach Namen suchen',
      oninput: (e) => { repoQuery = e.target.value.trim().toLowerCase(); renderRepos(); },
    }),
    filters: h('div', { class: 'segmented', role: 'group', 'aria-label': 'Nach Zustand filtern' }),
    body: h('div', { class: 'repos-body' }),
  };
  reposUi.filterBar = h('div', { class: 'filter-bar' }, reposUi.search, reposUi.filters);
  reposUi.overview.append(reposUi.head, reposUi.note, reposUi.filterBar);
  reposUi.root.append(reposUi.decisions, reposUi.overview, reposUi.body);
}

const repoVisible = (r) => (REPO_FILTERS.find(([id]) => id === repoFilter)?.[2] ?? (() => true))(r)
  && (!repoQuery || r.repo.toLowerCase().includes(repoQuery));

function resetRepoFilter() {
  repoQuery = '';
  repoFilter = 'all';
  reposUi.search.value = '';
  renderRepos();
  reposUi.search.focus();
}

/** Reads every repository once when the area first opens; later only on request. */
function readReposOnce() {
  if (reposRead || !view.repositories.length) return;
  reposRead = true;
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
    const repo = repoKey && repos.find((r) => r.key === repoKey);
    if (!repo) repoKey = null;
    // The details concern one repository only: no fleet actions, no other repository's jobs above them.
    reposUi.overview.hidden = !!repo;
    if (repo) {
      reposUi.decisions.hidden = true;
      reposUi.body.replaceChildren(repoDetail(repo));
      return;
    }
    const reading = repos.some((r) => r.loading || r.detailsLoading);
    const securing = repos.some((r) => r.securityLoading);
    const chosen = repos.filter((r) => selection.has(r.key));
    const visible = repos.filter(repoVisible);
    const careable = visible.filter((r) => !r.capabilities.care && !r.job);
    const hiddenChosen = chosen.filter((r) => !repoVisible(r)).length;
    const allVisible = careable.length > 0 && careable.every((r) => selection.has(r.key));
    const filtered = repoQuery || repoFilter !== 'all';
    reposUi.head.replaceChildren(...[
      h('h2', null, 'Repositories'),
      repos.length > 0 && h('label', { class: 'check-all', title: filtered ? 'Alle sichtbaren pflegbaren Repositories an- oder abwählen' : 'Alle pflegbaren Repositories an- oder abwählen' },
        h('input', {
          type: 'checkbox', id: 'repos-select-all', checked: allVisible,
          indeterminate: !allVisible && careable.some((r) => selection.has(r.key)), disabled: !careable.length,
          onchange: (e) => { for (const r of careable) e.target.checked ? selection.add(r.key) : selection.delete(r.key); renderRepos(); },
        }), filtered ? 'Alle Treffer' : 'Alle'),
      h('span', { class: 'muted small', role: 'status' }, repos.length
        ? [`${repos.length} verwaltet`, filtered && `${visible.length} angezeigt`,
          chosen.length && `${chosen.length} ausgewählt${hiddenChosen ? `, davon ${hiddenChosen} ausgeblendet` : ''}`].filter(Boolean).join(' · ')
        : ''),
      h('span', { class: 'spacer' }),
      (reading || securing) && button('Lesen abbrechen', () => call('cancel_repo_refresh'), { id: 'repos-cancel-read' }),
      repos.length > 0 && button(reading ? 'Wird gelesen …' : 'Status aktualisieren', () => call('refresh_repos', { keys: [] }),
        { id: 'repos-refresh', iconName: 'refresh', disabled: reading, title: 'Liest den Stand aller Repositories auf GitHub: nur lesend, ohne Claude' }),
      repos.length > 0 && button(securing ? 'Wird geprüft …' : chosen.length ? `Sicherheit prüfen (${chosen.length})` : 'Sicherheit prüfen',
        () => call('check_security', { keys: chosen.map((r) => r.key) }),
        { id: 'repos-security', iconName: 'shield', disabled: securing,
          title: `Prüft ${chosen.length ? 'die ausgewählten' : 'alle'} Repositories auf GitHub: Schutz des Default-Branches (direkte Pushes, Force-Push), Secret- und Abhängigkeits-Scanning, offene Alerts, Workflow-Rechte. Nur lesend, ohne Claude.` }),
      button('Hinzufügen …', addReposDialog, { id: 'repos-add', iconName: 'plus', title: 'Lokale Repositories in die Verwaltung aufnehmen' }),
      repos.length > 0 && button(chosen.length ? `Pflegen (${chosen.length}) …` : 'Pflegen …', () => jobDialog(chosen.map((r) => r.key)),
        { id: 'repos-care', iconName: 'branch', cls: chosen.length ? 'primary' : null, disabled: !chosen.length || !!view.running,
          title: chosen.length ? `Pflegeauftrag für die ${chosen.length} ausgewählten Repositories${hiddenChosen ? `, auch die ${hiddenChosen} ausgeblendeten` : ''}` : 'Erst Repositories auswählen (Häkchen auf den Karten)' }),
    ].filter(Boolean));
    reposUi.note.textContent = repos.length
      ? '„Status aktualisieren“ liest nur. Ein Pflegeauftrag legt dir vor jeder Änderung einen Vorschlag zur Freigabe vor.'
      : '';
    reposUi.filterBar.hidden = repos.length < 2;
    reposUi.filters.replaceChildren(...REPO_FILTERS.map(([id, label, test]) => {
      const n = id === 'all' ? repos.length : repos.filter(test).length;
      return h('button', {
        type: 'button', id: `repos-filter-${id}`, class: `segment ${repoFilter === id ? 'on' : ''}`, 'aria-pressed': String(repoFilter === id),
        onclick: () => { repoFilter = id; renderRepos(); },
      }, label, h('span', { class: 'segment-count' }, n));
    }));
    renderDecisions();
    let body;
    if (!repos.length) {
      body = h('div', { class: 'empty' },
        h('h2', null, 'Repositories auswählen'),
        h('p', { class: 'muted' }, 'Wähle die Repositories, um die sich die App kümmern soll. Eine Website, ein SFTP-Zugang oder ein Release sind dafür nicht nötig.'),
        h('div', { class: 'line' }, button('Repositories auswählen …', addReposDialog, { cls: 'primary', iconName: 'plus' })));
    } else if (!visible.length) {
      body = h('div', { class: 'note-box' },
        h('span', null, 'Kein Repository passt zu Suche und Filter.', hiddenChosen ? ` Die Auswahl (${hiddenChosen}) bleibt erhalten.` : ''),
        h('div', null, button('Filter zurücksetzen', resetRepoFilter, { id: 'repos-filter-reset', cls: 'link' })));
    } else {
      body = h('div', { class: 'repo-cards' }, visible.map(repoCard));
    }
    reposUi.body.replaceChildren(body);
  });
}

/** Open jobs at the top: what needs a decision first, then what runs or waits. */
function renderDecisions() {
  const order = { needsApproval: 0, running: 1, preparing: 1, blocked: 2, waiting: 3, planned: 4 };
  const jobs = openJobs().sort((a, b) => (order[a.status.state] ?? 9) - (order[b.status.state] ?? 9));
  reposUi.decisions.hidden = !jobs.length;
  if (!jobs.length) return;
  reposUi.decisions.replaceChildren(
    h('div', { class: 'section-head' }, h('h2', null, jobs.some((j) => j.status.state === 'needsApproval') ? 'Entscheidungen und laufende Aufträge' : 'Laufende Aufträge')),
    h('div', { class: 'job-cards' }, jobs.map(jobCard)));
}

function jobTitle(j) {
  const names = j.repos.map((key) => repoName(key).split('/').pop());
  return names.length > 3 ? `${names.slice(0, 3).join(', ')} +${names.length - 3}` : names.join(', ');
}

/** The job's steps with their state in words, never by colour alone. */
function stepPills(j, details) {
  return h('ul', { class: 'job-card-steps', 'aria-label': 'Schritte' }, j.steps.map((s) => {
    const state = STEP_STATE[s.state]?.[1] ?? s.state;
    return h('li', { class: `step-pill ${STEP_STATE[s.state]?.[0] ?? ''}`, title: details && s.detail ? `${s.label}: ${state} – ${s.detail}` : null },
      h('span', null, s.label), h('span', { class: 'step-pill-state' }, ` · ${state}`));
  }));
}

function jobCard(j) {
  const decide = j.status.state === 'needsApproval';
  return h('div', { class: `job-card ${JOB_STATE[j.status.state]?.[0] ?? ''}` },
    h('div', { class: 'job-card-head' }, h('strong', { class: 'ellipsis' }, jobTitle(j)), h('span', { class: 'spacer' }), jobChip(j.status, j.running)),
    h('div', { class: 'job-card-reason selectable' }, j.status.reason),
    stepPills(j, true),
    h('div', { class: 'line' },
      h('span', { class: 'muted small' }, `Auftrag vom ${formatTime(j.createdAt)}`),
      h('span', { class: 'spacer' }),
      button(decide ? 'Entscheiden …' : 'Ansehen', () => openJob(j.id), { cls: decide ? 'primary' : null, id: domId('job-open', j.id) })));
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
  manageDrafts.delete(key);
  renderRepos();
  document.getElementById('main').scrollTop = repoListScroll;
  if (key) document.getElementById(domId('repo-row', key))?.focus({ preventScroll: true });
}

// MARK: Repositories: overview

function healthChip(r) {
  if (r.loading && !r.facts) return h('span', { class: 'chip running' }, h('span', { class: 'spinner' }), 'Wird gelesen');
  const [cls, text] = HEALTH[r.health.state];
  return h('span', { class: `chip ${cls}`, title: 'Zustand des Repositorys (unabhängig von Aufträgen)' }, text);
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

/** What the repository feeds: content targets, website products. */
function relations(r) {
  const targets = r.targets.map((id) => view.targets.find((t) => t.id === id)?.name).filter(Boolean);
  return [
    targets.length && `Inhalte: ${targets.join(', ')}`,
    r.products.length && `Website: ${r.products.map((p) => `${p.name}${p.enabled ? '' : ' (abgewählt)'}`).join(', ')}`,
    r.website && 'Website-Repository',
    r.release ? `Release-Regel: Tags ${r.release.tagPrefix}x.y.z${r.release.publishesRelease ? ', mit GitHub-Release' : ''}` : 'keine Release-Regel',
  ].filter(Boolean).join(' · ');
}

function repoCard(r) {
  const [owner, name] = r.repo.split('/');
  const locked = r.capabilities.care ?? (r.job ? 'gehört zu einem offenen Auftrag' : null);
  return h('div', { class: `repo-card ${r.loading && !r.facts ? '' : HEALTH[r.health.state][0]} ${r.enabled ? '' : 'off'}`, onclick: () => openRepo(r.key) },
    h('div', { class: 'repo-card-head' },
      h('input', {
        type: 'checkbox', id: domId('repo-pick', r.key), checked: selection.has(r.key) && !locked, disabled: !!locked,
        title: locked ? `Nicht wählbar: ${locked}` : 'Für den nächsten Pflegeauftrag auswählen', 'aria-label': `${r.repo} auswählen`,
        onclick: (e) => e.stopPropagation(),
        onchange: (e) => { e.target.checked ? selection.add(r.key) : selection.delete(r.key); renderRepos(); },
      }),
      h('button', { type: 'button', class: 'repo-name', id: domId('repo-row', r.key), onclick: (e) => { e.stopPropagation(); openRepo(r.key); } },
        h('strong', { class: 'ellipsis' }, name), h('span', { class: 'muted small ellipsis' }, owner)),
      h('span', { class: 'spacer' }),
      r.loading && r.facts && h('span', { class: 'spinner muted', title: 'wird neu gelesen' }),
      r.job && jobChip(r.job.status, r.job.status.state === 'running' || r.job.status.state === 'preparing'),
      healthChip(r)),
    h('span', { class: 'repo-card-reason' }, r.enabled ? mainReason(r) : 'deaktiviert: wird gelesen, aber nicht gepflegt'),
    h('span', { class: 'repo-card-meta' }, (r.facts ? [prSummary(r), ciSummary(r), releaseSummary(r)] : []).concat(LOCAL_TEXT[r.local.state], checkedSummary(r))
      .filter(Boolean).map((text) => h('span', null, text)), securitySummary(r)),
    h('span', { class: 'repo-card-rel' }, relations(r)));
}

// MARK: Repositories: details

// Which folded parts of a repository's details the user opened, `key:part` → open; survives re-rendering.
const repoFold = new Map();

/** A collapsible part of the details that keeps its open state across re-renders. */
function fold(key, part, summary, body, open = false) {
  const id = `${key}:${part}`;
  return h('details', { class: 'panel fold-panel', id: domId('fold', id), open: repoFold.get(id) ?? open, ontoggle: (e) => repoFold.set(id, e.target.open) },
    h('summary', null, summary), h('div', { class: 'fold-body' }, body));
}

/** CI, release and data state in one line, for the top of the details. */
function statusLine(r) {
  if (!r.facts) return h('div', { class: 'facts' }, h('span', null, checkedSummary(r)));
  return h('div', { class: 'facts' }, [ciSummary(r), releaseSummary(r), checkedSummary(r)].filter(Boolean).map((text, i) =>
    h('span', { class: i === 2 && r.health.stale ? 'orange' : null }, text)));
}

function repoDetail(r) {
  const f = r.facts;
  const access = ok(f?.access);
  const branch = access?.defaultBranch ?? 'Default-Branch';
  if (f && !r.details && !r.detailsLoading && ok(f.prs)?.items.length) {
    const mark = `${r.key}:${f.checkedAt}`;
    if (!detailsAsked.has(mark)) {
      detailsAsked.add(mark);
      queueMicrotask(() => call('load_repo_details', { key: r.key }));
    }
  }
  const locked = r.capabilities.care ?? (r.job ? 'gehört zu einem offenen Auftrag' : null);
  // Security findings that matter go up front; otherwise the check follows the content targets.
  const urgent = r.security?.checks.some((c) => c.level === 'high' || c.level === 'medium');
  const security = h('section', { class: `panel section ${urgent ? 'urgent' : ''}`, id: 'repo-security-section' }, securitySection(r));
  return h('div', { class: 'repo-detail' },
    h('div', { class: 'repo-detail-head' },
      h('nav', { class: 'crumbs', 'aria-label': 'Pfad' },
        button('Alle Repositories', closeRepo, { id: 'repo-back', iconName: 'back', cls: 'link' }),
        h('span', { class: 'muted', 'aria-hidden': 'true' }, '›'),
        h('h2', { class: 'ellipsis', title: r.repo }, r.repo)),
      healthChip(r),
      h('span', { class: 'spacer' }),
      button(r.loading ? 'Wird gelesen …' : 'Status aktualisieren', () => call('refresh_repos', { keys: [r.key] }),
        { id: 'repo-refresh', iconName: 'refresh', disabled: r.loading, title: `Liest den Stand von ${r.repo} auf GitHub: nur lesend, ohne Claude` }),
      access?.url && button('Auf GitHub', () => call('open_url', { url: access.url }), { id: 'repo-github', iconName: 'external' }),
      button('Pflegen …', () => jobDialog([r.key]), { id: 'repo-care', cls: 'primary', iconName: 'branch', disabled: !!locked || !!view.running, title: locked ? `Nicht möglich: ${locked}` : `Pflegeauftrag für ${r.repo}` })),
    statusLine(r),
    r.job && h('div', { class: `note-box ${JOB_STATE[r.job.status.state]?.[0] ?? ''}` },
      h('span', null, h('strong', null, 'Offener Auftrag: '), jobChip(r.job.status, false), h('span', { class: 'selectable' }, ` ${r.job.status.reason}`)),
      h('span', { class: 'muted small' }, 'Pflege, Sicherheitskorrekturen und Verwaltung dieses Repositorys sind gesperrt, bis der Auftrag abgeschlossen oder beendet ist.'),
      h('div', null, button('Auftrag ansehen', () => openJob(r.job.id), { id: 'repo-job', cls: 'link' }))),
    findings(r),
    urgent && security,
    h('section', { class: 'panel section' }, prSection(r, branch)),
    h('section', { class: 'panel section' }, contentSection(r)),
    !urgent && security,
    fold(r.key, 'tech', 'Technische Details', statusFacts(r, branch)),
    fold(r.key, 'manage', 'Verwaltung', manageSection(r), manageDrafts.get(r.key)?.dirty));
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
    default: return 'nicht eingetragen; Status und PR-Pflege gehen auch ohne';
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
    row('Release-Regel', r.release ? `Patch-Tags ${r.release.tagPrefix}x.y.z${r.release.publishesRelease ? ', danach wartet die App auf das GitHub-Release' : ''}` : 'keine: die App setzt keine Tags', r.release ? '' : 'muted'),
    row('Lokale Kopie', localText(r.local), ['found', 'notConfigured'].includes(r.local.state) ? '' : 'orange'),
    r.products.length > 0 && row('Website', [...r.products.map((p) => `${p.name}: dokumentiert ${p.documented}${p.enabled ? '' : ', nicht ausgewählt'}`)].join(' · ')),
    r.website && row('Rolle', 'zugleich das Repository der Website'),
    row('Gelesen', f ? `${formatTime(f.checkedAt)} (${ago(f.checkedAt)})${r.health.stale ? ', veraltet' : ''}` : 'noch nicht', r.health.stale ? 'orange' : ''));
}

function prSection(r, branch) {
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
    h('div', { class: 'items' }, list.items.map((pr) => prItem(pr, details.get(pr.number), branch))),
  ].filter(Boolean);
}

function prItem(pr, detail, branch) {
  const ci = detail?.ci;
  const ciBadge = !detail ? null : ok(ci)
    ? badge({ passed: 'green', failed: 'red', pending: '', noChecks: 'orange', unknown: 'orange' }[ci.value.state], `CI ${CI_TEXT[ci.value.state]}`, ci.value.detail)
    : badge('orange', 'CI nicht abfragbar', ci?.value);
  const merge = detail && badge(['clean', 'has_hooks'].includes(detail.mergeableState) ? 'green' : detail.mergeableState === 'dirty' ? 'red' : '', detail.mergeable);
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
      !detail && 'CI und Mergebarkeit noch nicht gelesen',
    ].filter(Boolean).join(' · ')));
}

// MARK: Repositories: security

const SEC_LEVEL = {
  high: ['red', 'Kritisch'], medium: ['orange', 'Wichtig'], low: ['', 'Hinweis'], unknown: ['', 'Nicht prüfbar'], ok: ['green', 'In Ordnung'],
};
const FINDING_LEVELS = ['high', 'medium', 'low'];

/** One line on the card: what the last security check found. */
function securitySummary(r) {
  if (r.securityLoading && !r.security) return h('span', null, 'Sicherheit wird geprüft …');
  if (!r.security) return null;
  const [high, medium, low] = FINDING_LEVELS.map((level) => r.security.checks.filter((c) => c.level === level).length);
  const parts = [high && `${high} kritisch`, medium && `${medium} wichtig`, low && `${low} Hinweis${low === 1 ? '' : 'e'}`].filter(Boolean);
  return h('span', { class: high ? 'red' : medium ? 'orange' : parts.length ? null : 'green', title: 'Ergebnis der letzten Sicherheitsprüfung' },
    `Sicherheit: ${parts.length ? parts.join(', ') : 'kein Befund'}`);
}

/** The fixes the report offers, each once (with the first check that offers it). */
function securityOffers(report) {
  const offers = new Map();
  for (const c of report?.checks ?? []) if (c.fix && !offers.has(c.fix.fix)) offers.set(c.fix.fix, { ...c.fix, check: c });
  return [...offers.values()];
}

function securityItem(r, c) {
  const [cls, text] = SEC_LEVEL[c.level];
  const locked = r.job ? 'gehört zu einem offenen Auftrag' : null;
  return h('div', { class: 'item' },
    h('div', { class: 'item-line' },
      badge(cls, text), h('strong', { class: 'item-title', title: c.title }, c.title), h('span', { class: 'spacer' }),
      c.fix && button(`${c.fix.label} …`, () => securityFixDialog(r, c.fix.fix), {
        cls: 'link', disabled: !!locked, title: locked ? `Nicht möglich: ${locked}` : c.fix.effect, id: domId('sec-fix', `${r.key}-${c.id}`),
      }),
      c.url && button('Auf GitHub', () => call('open_url', { url: c.url }), { cls: 'link', title: 'Einstellung oder Alerts auf GitHub öffnen' })),
    h('div', { class: 'item-reason selectable' }, c.detail));
}

function securitySection(r) {
  const report = r.security;
  const offers = securityOffers(report);
  const locked = r.job ? 'gehört zu einem offenen Auftrag' : null;
  const head = h('div', { class: 'section-head wrap' }, h('h3', null, 'Sicherheit'),
    report && h('span', { class: 'muted small' }, `geprüft ${ago(report.checkedAt)}`),
    h('span', { class: 'spacer' }),
    offers.length > 0 && button(`Beheben (${offers.length}) …`, () => securityFixDialog(r), {
      id: 'repo-security-fix', iconName: 'shield', disabled: !!locked,
      title: locked ? `Nicht möglich: ${locked}` : 'Einstellungen auf GitHub ändern; du wählst, welche',
    }),
    button(r.securityLoading ? 'Wird geprüft …' : report ? 'Neu prüfen' : 'Sicherheit prüfen', () => call('check_security', { keys: [r.key] }),
      { id: 'repo-security-check', iconName: report ? 'refresh' : 'shield', disabled: r.securityLoading, title: 'Nur lesend, ohne Claude' }));
  if (!report) {
    return [head,
      h('div', { class: 'note' }, r.securityLoading ? 'Schutzregeln, Einstellungen und Alerts werden gelesen …'
        : 'Noch nicht geprüft. Gelesen werden Branch-Schutz, Scanning-Einstellungen, offene Alerts und die Rechte des Workflow-Tokens; nur lesend, ohne Claude.'),
      r.securityError && h('div', { class: 'note error-text' }, `Prüfung fehlgeschlagen: ${r.securityError}`)].filter(Boolean);
  }
  const findings = report.checks.filter((c) => FINDING_LEVELS.includes(c.level));
  const rest = report.checks.filter((c) => !FINDING_LEVELS.includes(c.level));
  const okCount = rest.filter((c) => c.level === 'ok').length;
  const unknownCount = rest.length - okCount;
  return [head,
    r.securityError && h('div', { class: 'note orange' }, `Letzte Prüfung fehlgeschlagen, gezeigt wird die ältere: ${r.securityError}`),
    report.archived ? h('div', { class: 'note' }, 'Archiviert: Einstellungen lassen sich nicht ändern.')
      : report.admin !== true && h('div', { class: 'note orange' },
        report.admin == null ? 'Ohne gh-Anmeldung ist vieles nicht lesbar, und nichts lässt sich in der App beheben (gh auth login).'
          : 'Ohne Admin-Rechte sind manche Einstellungen nicht lesbar, und nichts lässt sich in der App beheben.'),
    findings.length ? h('div', { class: 'items' }, findings.map((c) => securityItem(r, c))) : h('div', { class: 'note green' }, 'Kein Befund.'),
    rest.length > 0 && h('details', { class: 'sec-more' },
      h('summary', null, [okCount && `${okCount} in Ordnung`, unknownCount && `${unknownCount} nicht prüfbar`].filter(Boolean).join(', ')),
      h('div', { class: 'items' }, rest.map((c) => securityItem(r, c)))),
  ].filter(Boolean);
}

/** Asks which fixes to carry out (`only`: just that one ticked), then shows what GitHub answered. */
function securityFixDialog(r, only) {
  const offers = securityOffers(r.security);
  // Without `only`, what can break something stays unticked until you tick it.
  const picked = new Set(offers.filter((o) => (only ? o.fix === only : !o.warning)).map((o) => o.fix));
  const s = { busy: false, error: '', results: null };
  const name = (fix) => offers.find((o) => o.fix === fix)?.label ?? fix;
  openDialog('Sicherheit verbessern', (close, render) => {
    if (s.results) {
      const failed = s.results.filter((x) => !x.ok).length;
      return {
        body: [
          h('h2', null, failed ? `${s.results.length - failed} von ${s.results.length} geändert` : 'Geändert'),
          h('div', { class: 'items' }, s.results.map((x) => h('div', { class: 'item' },
            h('div', { class: 'item-line' }, badge(x.ok ? 'green' : 'red', x.ok ? 'Erledigt' : 'Abgelehnt'), h('strong', null, name(x.fix))),
            h('div', { class: 'item-reason selectable' }, x.text)))),
          h('div', { class: 'note' }, 'Die Sicherheit des Repositorys wird jetzt neu geprüft.'),
        ],
        actions: [button('Schließen', close, { cls: 'primary' })],
      };
    }
    return {
      body: [
        h('h2', null, `${r.repo}: Sicherheit verbessern`),
        h('div', { class: 'note' }, 'Jeder Punkt ändert eine Einstellung auf GitHub. Er fügt Schutz hinzu, entfernt nichts und lässt sich in den Einstellungen des Repositorys auf GitHub wieder zurücknehmen.'),
        h('div', { class: 'sec-choices' }, offers.map((o) => h('label', { class: 'check-line top' },
          h('input', { type: 'checkbox', id: domId('sec-pick', o.fix), checked: picked.has(o.fix), disabled: s.busy, onchange: (e) => { e.target.checked ? picked.add(o.fix) : picked.delete(o.fix); render(); } }),
          h('span', null,
            h('strong', null, o.label), ' ', h('span', { class: 'muted small' }, `· ${o.check.title}`),
            h('div', { class: 'note' }, o.effect),
            o.warning && h('div', { class: 'note orange' }, o.warning))))),
        s.error && h('div', { class: 'note error-text', role: 'alert' }, s.error),
      ],
      actions: [
        button('Abbrechen', close, { disabled: s.busy }),
        button(s.busy ? 'Wird geändert …' : picked.size > 1 ? `${picked.size} Änderungen ausführen` : 'Auf GitHub ändern', async () => {
          s.busy = true; s.error = ''; render();
          try {
            s.results = await invoke('fix_security', { key: r.key, fixes: [...picked] });
          } catch (error) { s.error = String(error); }
          s.busy = false; render();
        }, { cls: 'primary', iconName: 'shield', disabled: s.busy || !picked.size }),
      ],
    };
  });
}

// MARK: Repositories: content targets

const KIND_TEXT = { docs: 'README/Dokumentation', wiki: 'GitHub-Wiki' };
const BASIS_TEXT = { release: 'neuestes Release', commit: 'aktueller Stand des Default-Branches' };

function deliveryText(d) {
  if (!d) return null;
  const sources = d.sources.map((s) => s.label.includes(' ') ? `${s.repo} @ ${s.label}` : `${s.repo} ${s.label}`).join(', ');
  return `${d.what || 'Vorschlag'} für ${sources} (${ago(d.at)})`;
}

function targetSummary(t) {
  const s = t.state;
  return [
    s.published ? `Veröffentlicht: ${deliveryText(s.published)}` : 'Noch nie über die App veröffentlicht',
    s.delivered && s.delivered.pr != null && `Wartet auf Merge: ${deliveryText(s.delivered)}`,
    s.prepared && (!s.delivered || s.prepared.at > s.delivered.at) && `Zuletzt vorbereitet: ${deliveryText(s.prepared)}`,
  ].filter(Boolean);
}

function targetItem(t, actions = true) {
  return h('div', { class: 'item target-item' },
    h('div', { class: 'item-line' },
      h('strong', { class: 'item-title' }, t.name), badge(t.kind === 'wiki' ? 'accent' : '', KIND_TEXT[t.kind]),
      h('span', { class: 'muted small mono' }, t.repo),
      h('span', { class: 'spacer' }),
      t.job && jobChip(t.job.status, false),
      actions && button('Bearbeiten …', () => targetDialog(t), { cls: 'link', disabled: !!t.job, title: t.job ? 'Gehört zu einem offenen Auftrag' : null }),
      actions && button('Entfernen', () => confirmDialog('Inhaltsziel entfernen', `„${t.name}“ aus der App entfernen? Im Repository und im Wiki ändert sich nichts.`, 'Entfernen',
        () => act('remove_target', { id: t.id })), { cls: 'link danger', disabled: !!t.job })),
    h('div', { class: 'item-reason' }, `Quellen: ${t.sources.map(repoName).join(', ')} · Basis: ${BASIS_TEXT[t.basis]} · Pfade: ${t.paths.join(', ')}${t.kind === 'docs' ? ` · Branch: ${t.branch || 'Default-Branch'}` : ''}${t.mergeWhenGreen ? ' · mergt bei grüner CI' : ''}`),
    targetSummary(t).map((line) => h('div', { class: 'item-reason' }, line)),
    t.freshness.map((line) => h('div', { class: 'item-reason orange' }, line)),
    t.problem && h('div', { class: 'note error-text' }, `Nicht nutzbar: ${t.problem}`));
}

function contentSection(r) {
  const targets = view.targets.filter((t) => t.sources.includes(r.key));
  const feedsWebsite = r.products.length > 0;
  return [
    h('div', { class: 'section-head wrap' }, h('h3', null, 'Zugeordnete Inhalte'),
      h('span', { class: 'muted small' }, 'README, Wiki und Website; jedes Ziel ist für sich aktuell oder nicht'),
      h('span', { class: 'spacer' }),
      button('Ziel hinzufügen …', () => targetDialog(null, r.key), { id: 'target-add', iconName: 'plus' })),
    targets.length ? h('div', { class: 'items' }, targets.map((t) => targetItem(t))) : h('div', { class: 'note' }, 'Noch kein README-, Dokumentations- oder Wiki-Ziel für dieses Repository.'),
    feedsWebsite && h('div', { class: 'item' },
      h('div', { class: 'item-line' }, h('strong', null, view.site?.name ?? 'Website'), badge('', 'Website'), h('span', { class: 'spacer' }),
        button('Website-Bereich', () => showArea('website'), { cls: 'link' })),
      h('div', { class: 'item-reason' }, `Über die Produkte ${r.products.map((p) => p.name).join(', ')}; als letzter Schritt eines Auftrags wählbar, mit Vorschau vor dem Veröffentlichen.`)),
  ].filter(Boolean);
}

// MARK: Repositories: management

// Unsaved edits of the management form, per repository; a re-render (new view) keeps them.
const manageDrafts = new Map();

function manageSection(r) {
  const saved = { enabled: r.enabled, localPath: r.local.path ?? '', release: !!r.release, prefix: r.release?.tagPrefix ?? (r.products[0]?.tagPrefix ?? ''), publishes: r.release?.publishesRelease ?? false };
  const s = manageDrafts.get(r.key) ?? { ...saved };
  manageDrafts.set(r.key, s);
  const locked = !!r.job;
  // "Speichern" only matters once something differs from what is saved.
  const changed = () => s.enabled !== saved.enabled || s.localPath.trim() !== saved.localPath || s.release !== saved.release
    || (s.release && (s.prefix.trim() !== saved.prefix || s.publishes !== saved.publishes));
  const saveButton = button('Speichern', () => save(), { cls: 'primary', id: 'manage-save' });
  const state = h('span', { class: 'muted small', role: 'status' });
  const update = () => {
    s.dirty = changed();
    saveButton.disabled = locked || !s.dirty;
    state.textContent = s.dirty ? 'Ungespeicherte Änderungen' : '';
  };
  const prefixInput = h('input', { type: 'text', id: 'manage-prefix', value: s.prefix, placeholder: 'leer für 1.2.3, „v“ für v1.2.3', spellcheck: 'false', disabled: !s.release || locked, oninput: (e) => { s.prefix = e.target.value; update(); } });
  const publishes = h('input', { type: 'checkbox', id: 'manage-publishes', checked: s.publishes, disabled: !s.release || locked, onchange: (e) => { s.publishes = e.target.checked; update(); } });
  const localInput = h('input', { type: 'text', id: 'manage-local', value: s.localPath, placeholder: 'kein lokaler Klon', spellcheck: 'false', disabled: locked, oninput: (e) => { s.localPath = e.target.value; update(); } });
  const save = async () => {
    const ok = await act('update_repository', {
      key: r.key,
      settings: { enabled: s.enabled, localPath: s.localPath, release: s.release ? { tagPrefix: s.prefix.trim(), publishesRelease: s.publishes } : null },
    });
    if (ok) {
      manageDrafts.delete(r.key);
      call('refresh_repos', { keys: [r.key] });
    }
  };
  update();
  return [
    h('div', { class: 'note' }, locked ? 'Gesperrt, solange ein offener Auftrag das Repository hält.' : 'Gilt für alle künftigen Aufträge.'),
    h('label', { class: 'check-line' }, h('input', { type: 'checkbox', id: 'manage-enabled', checked: s.enabled, disabled: locked, onchange: (e) => { s.enabled = e.target.checked; update(); } }),
      'Aktiv: Aufträge dürfen dieses Repository pflegen'),
    h('div', { class: 'field-row' }, h('label', { for: 'manage-local' }, 'Lokale Kopie'),
      h('div', { class: 'line' }, localInput,
        button('Auswählen …', async () => { const p = await call('pick_folder'); if (p) { s.localPath = p; localInput.value = p; update(); } }, { id: 'manage-pick', disabled: locked }),
        !s.localPath && button('Klonen', async () => {
          const path = await call('clone_repo', { repo: r.repo, workspaceDir: '' });
          if (path) { s.localPath = path; localInput.value = path; update(); }
        }, { id: 'manage-clone', disabled: locked, title: `Nach ${view.workspaceDisplay} klonen` }))),
    h('div', { class: 'note' }, 'Optional. Nur fürs Aufräumen lokaler Branches und für Claudes Prüfung im Code.'),
    h('label', { class: 'check-line top' }, h('input', {
      type: 'checkbox', id: 'manage-release', checked: s.release, disabled: locked,
      onchange: (e) => { s.release = e.target.checked; prefixInput.disabled = !s.release; publishes.disabled = !s.release; update(); },
    }), h('span', null, h('strong', null, 'Release-Regel: die App darf die nächste Patch-Version taggen'),
      h('div', { class: 'note' }, 'Die App errät keine Versionierung. Getaggt wird nur auf grüner CI genau des Ziel-Commits.'))),
    h('div', { class: 'field-row' }, h('label', { for: 'manage-prefix' }, 'Tag-Präfix'), prefixInput),
    h('label', { class: 'check-line' }, publishes, 'Ein Workflow des Repositorys veröffentlicht zu jedem Tag ein GitHub-Release; die App wartet darauf'),
    r.conflicts.length > 0 && h('div', { class: 'note orange' }, r.conflicts.join(' · ')),
    h('div', { class: 'line' },
      saveButton, state,
      h('span', { class: 'spacer' }),
      button('Aus der Verwaltung entfernen …', () => confirmDialog('Aus der Verwaltung entfernen',
        `${r.repo} aus der Verwaltung nehmen? Gelöscht wird nichts: nicht auf GitHub, nicht die lokale Kopie, nicht die Website-Konfiguration.`, 'Entfernen',
        async () => { if (await act('remove_repository', { key: r.key })) { selection.delete(r.key); closeRepo(); } }), { id: 'manage-remove', cls: 'danger', disabled: locked })),
  ].filter(Boolean);
}

// MARK: Runs (care jobs)

// Full jobs as `get_job` returns them, by id; reloaded when the job's `updatedAt` changes.
const jobCache = new Map();
const jobLoading = new Set();
// The user's marks on a plan before approving it, `jobId:planId` → { excluded, included }: items left
// out, and merges chosen among those the plan offers for choice.
const exclusions = new Map();
// Which step blocks the user opened or closed; survives re-rendering.
const stepOpen = new Map();
// What the result panel shows, so an unchanged job is not rebuilt (diffs are large).
let renderedJob = '';

function buildRuns() {
  runsUi = {
    root: h('div', { class: 'area runs-area' }),
    list: h('div', { class: 'runs-list' }),
    detail: h('div', { class: 'job-detail' }),
    head: h('div', { class: 'repo-detail-head' }),
    status: h('div', { class: 'job-status' }),
    facts: h('div', { class: 'facts' }),
    tabResult: h('button', { type: 'button', class: 'tab', role: 'tab', id: 'job-tab-result', 'aria-controls': 'job-result', onclick: () => showJobTab('result') }, 'Ergebnis'),
    tabLog: h('button', { type: 'button', class: 'tab', role: 'tab', id: 'job-tab-log', 'aria-controls': 'job-log', onclick: () => showJobTab('log') }),
    result: h('div', { class: 'detail-result', id: 'job-result', role: 'tabpanel', 'aria-labelledby': 'job-tab-result' }),
    log: h('pre', { class: 'log', id: 'job-log', role: 'tabpanel', 'aria-labelledby': 'job-tab-log', tabindex: '0' }),
  };
  runsUi.detail.append(runsUi.head, runsUi.status, runsUi.facts,
    h('section', { class: 'detail' },
      h('div', { class: 'tabs', role: 'tablist', 'aria-label': 'Auftrag', onkeydown: jobTabKey }, runsUi.tabResult, runsUi.tabLog),
      runsUi.result, runsUi.log));
  runsUi.root.append(runsUi.list, runsUi.detail);
}

function renderRuns() {
  const summary = jobId && view.jobs.find((j) => j.id === jobId);
  if (jobId && !summary) jobId = null;
  runsUi.list.hidden = !!jobId;
  runsUi.detail.hidden = !jobId;
  if (!jobId) {
    keepFocus(renderJobList);
    return;
  }
  const cached = jobCache.get(jobId);
  if ((!cached || cached.updatedAt !== summary.updatedAt || cached.running !== summary.running) && !jobLoading.has(jobId)) loadJob(jobId);
  keepFocus(() => renderJob(summary, jobCache.get(jobId)));
}

async function loadJob(id) {
  jobLoading.add(id);
  try {
    jobCache.set(id, await invoke('get_job', { id }));
  } catch (error) {
    jobCache.delete(id);
  } finally {
    jobLoading.delete(id);
  }
  if (area === 'runs' && jobId === id) renderRuns();
}

function renderJobList() {
  const open = openJobs();
  const done = view.jobs.filter((j) => JOB_FINISHED.has(j.status.state));
  const row = (j) => h('div', { class: `job-row ${JOB_STATE[j.status.state]?.[0] ?? ''}`, onclick: () => openJob(j.id) },
    h('div', { class: 'job-card-head' },
      h('button', { type: 'button', class: 'repo-name', id: domId('job-row', j.id), onclick: (e) => { e.stopPropagation(); openJob(j.id); } }, h('strong', { class: 'ellipsis' }, jobTitle(j))),
      h('span', { class: 'muted small' }, formatTime(j.createdAt)),
      h('span', { class: 'spacer' }), jobChip(j.status, j.running)),
    h('div', { class: 'job-card-reason selectable' }, j.status.reason),
    stepPills(j, false));
  runsUi.list.replaceChildren(
    h('section', { class: 'section' },
      h('div', { class: 'section-head' }, h('h2', null, 'Offene Aufträge'), h('span', { class: 'muted small' }, 'geplant, in Vorbereitung, mit Freigabe, laufend, wartend oder blockiert')),
      open.length ? h('div', { class: 'job-list' }, open.map(row)) : h('div', { class: 'note' }, 'Keine. Einen Auftrag startest du unter Repositories: Repositories auswählen, dann „Pflegen …“.')),
    h('section', { class: 'section' },
      h('div', { class: 'section-head' }, h('h2', null, 'Bisherige Aufträge'), h('span', { class: 'muted small' }, 'die letzten 50 bleiben erhalten')),
      done.length ? h('div', { class: 'job-list' }, done.map(row)) : h('div', { class: 'note' }, 'Noch keine.')));
}

function openJob(id) {
  const main = document.getElementById('main');
  if (area === 'runs' && !jobId) jobListScroll = main.scrollTop;
  const changedJob = jobId !== id;
  jobId = id;
  renderedJob = '';
  if (changedJob) {
    jobTab = view.jobs.find((j) => j.id === id)?.running ? 'log' : 'result';
    runsUi.log.replaceChildren();
    loadJobLog(id);
  }
  if (area !== 'runs') showArea('runs');
  else renderRuns();
  main.scrollTop = 0;
  document.getElementById('job-back')?.focus({ preventScroll: true });
}

function closeJob() {
  const id = jobId;
  jobId = null;
  renderRuns();
  document.getElementById('main').scrollTop = jobListScroll;
  if (id) document.getElementById(domId('job-row', id))?.focus({ preventScroll: true });
}

async function loadJobLog(id) {
  const lines = await invoke('get_job_log', { id });
  if (jobId !== id) return;
  runsUi.log.replaceChildren();
  appendTo(runsUi.log, lines);
}

function renderJob(summary, job) {
  const status = summary.status;
  const running = summary.running;
  const finished = JOB_FINISHED.has(status.state);
  const canContinue = !running && !finished && ['planned', 'waiting', 'blocked'].includes(status.state);
  runsUi.head.replaceChildren(...[
    button('Alle Aufträge', closeJob, { id: 'job-back', iconName: 'back' }),
    h('h2', { class: 'ellipsis' }, `Auftrag: ${jobTitle(summary)}`),
    jobChip(status, running),
    h('span', { class: 'spacer' }),
    canContinue && button(status.state === 'waiting' ? 'Erneut prüfen' : 'Fortsetzen', () => act('continue_job', { id: summary.id }),
      { id: 'job-continue', cls: 'primary', iconName: 'play', disabled: !!view.running, title: view.running ? 'Erst nach dem laufenden Schritt' : null }),
    running && button('Abbrechen', () => act('end_job', { id: summary.id }), { id: 'job-cancel', iconName: 'stop', cls: 'danger', title: 'Hält nach dem laufenden Teilschritt an; Erledigtes bleibt' }),
    !running && !finished && button('Auftrag beenden …', () => confirmDialog('Auftrag beenden', 'Den Auftrag beenden? Was noch nicht erledigt ist, bleibt unerledigt; nichts Erledigtes wird zurückgenommen.', 'Beenden',
      () => act('end_job', { id: summary.id })), { id: 'job-end' }),
    finished && button('Auftragsverlauf löschen …', () => confirmDialog('Auftragsverlauf löschen',
      'Gelöscht werden die gespeicherten Daten dieses Auftrags, sein Log und seine temporären Arbeitsdaten. Was der Auftrag auf GitHub, im Wiki oder auf der Website geändert hat, bleibt bestehen und wird nicht zurückgenommen.',
      'Verlauf löschen', () => act('delete_job', { id: summary.id }).then((ok) => ok && closeJob()), { danger: true }), { id: 'job-delete', cls: 'danger' }),
  ].filter(Boolean));
  runsUi.status.replaceChildren(
    h('div', { class: `note-box ${JOB_STATE[status.state]?.[0] ?? ''}` },
      h('strong', null, status.reason), status.next && h('div', { class: 'muted small' }, `Nächster Schritt: ${status.next}`)));
  const o = summary.options;
  const activities = [o.prs && 'Pull Requests', o.cleanup && 'Aufräumen', o.release && 'Release'].filter(Boolean);
  const targetNames = o.targets.map((t) => `${view.targets.find((x) => x.id === t.id)?.name ?? t.id} (${t.basis === 'release' ? 'Release' : 'aktueller Stand'})`);
  runsUi.facts.replaceChildren(...[
    h('span', null, h('strong', null, 'Repositories: '), summary.repos.map(repoName).join(', ')),
    activities.length > 0 && h('span', null, h('strong', null, 'Pflege: '), activities.join(', ')),
    targetNames.length > 0 && h('span', null, h('strong', null, 'Inhalte: '), targetNames.join(', ')),
    o.website && h('span', null, h('strong', null, 'Website: '), 'als letzter Schritt'),
    o.model && h('span', null, h('strong', null, 'Modell: '), modelName(o.model)),
    h('span', null, h('strong', null, 'Begonnen: '), formatTime(summary.createdAt)),
  ].filter(Boolean));
  const key = `${summary.id}:${job?.updatedAt}:${running}:${!!view.running}`;
  if (key !== renderedJob) {
    renderedJob = key;
    runsUi.result.replaceChildren(...(job ? job.steps.map((step) => stepBlock(job, step, running)) : [h('div', { class: 'note' }, 'Wird geladen …')]));
  }
  renderJobTabs(running);
}

function renderJobTabs(running) {
  const log = jobTab === 'log';
  const showsLog = !runsUi.log.hidden;
  for (const [tab, on] of [[runsUi.tabResult, !log], [runsUi.tabLog, log]]) {
    tab.setAttribute('aria-selected', String(on));
    tab.tabIndex = on ? 0 : -1;
  }
  runsUi.tabLog.replaceChildren(running ? h('span', { class: 'spinner' }) : '', 'Log');
  runsUi.result.hidden = log;
  runsUi.log.hidden = !log;
  if (log && !showsLog) runsUi.log.scrollTop = runsUi.log.scrollHeight;
}

function showJobTab(tab) {
  if (jobTab === tab) return;
  jobTab = tab;
  renderJobTabs(view.jobs.find((j) => j.id === jobId)?.running);
}

function jobTabKey(e) {
  if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(e.key)) return;
  e.preventDefault();
  const next = e.key === 'Home' ? 'result' : e.key === 'End' ? 'log' : jobTab === 'log' ? 'result' : 'log';
  showJobTab(next);
  (next === 'log' ? runsUi.tabLog : runsUi.tabResult).focus();
}

// MARK: Runs: steps

function stepBlock(job, step, running) {
  const key = `${job.id}:${step.id}`;
  const decide = step.state === 'needsApproval';
  const open = stepOpen.get(key) ?? (decide || ['failed', 'blocked', 'partial', 'running', 'waiting'].includes(step.state) || job.steps.length === 1);
  const body = {
    care: careBlock, release: releaseBlock, docs: contentBlock, wiki: contentBlock, website: websiteBlock,
  }[step.kind](job, step, running);
  return h('details', { class: `run step-block ${decide ? 'decide' : ''}`, open, ontoggle: (e) => stepOpen.set(key, e.target.open) },
    h('summary', null,
      h('strong', null, step.label), stepChip(step.state),
      h('span', { class: 'muted small ellipsis step-detail', title: step.detail }, step.detail),
      h('span', { class: 'spacer' }),
      step.links.map((l) => button(l.label, () => call('open_url', { url: l.url }), { cls: 'link', iconName: 'external' }))),
    h('div', { class: 'run-body' }, body));
}

function waitsFor(job, step) {
  const pending = step.after.map((id) => job.steps.find((s) => s.id === id)).filter((s) => s && !['done', 'nothingToDo', 'partial', 'rejected'].includes(s.state));
  return pending.length > 0 && step.state === 'pending' && h('div', { class: 'note' }, `Wartet auf: ${pending.map((s) => s.label).join(', ')}`);
}

/** Decision buttons; disabled while anything runs. */
function decisionBar(...buttons) {
  return h('div', { class: 'decision-bar' }, buttons.filter(Boolean),
    view.running && h('span', { class: 'note' }, 'Entscheiden geht, sobald der laufende Schritt fertig ist.'));
}

// What the app checks again right before each kind of write.
const APP_CHECK = {
  merge: 'Direkt davor prüft die App: noch offen, gleicher Commit und Ziel, mergebar, CI des PR-Commits grün; GitHub bekommt den erwarteten Commit mit.',
  close: 'Direkt davor: noch offen, gleicher Commit und Ziel; ein genannter Ersatz-PR muss offen oder gemergt sein. Kommentar und Schließen werden einzeln gemeldet.',
  rebase: 'Direkt davor: noch offen, von Dependabot, gleicher Commit, weiterhin Konflikt oder Rückstand.',
};
const OUTCOMES = {
  done: ['ok', 'Erledigt'], alreadyDone: ['ok', 'Schon erledigt'], skipped: ['warning', 'Übersprungen'], failed: ['error', 'Fehlgeschlagen'],
  cancelled: ['idle', 'Abgebrochen'], excluded: ['idle', 'Ausgenommen'],
};
// What a carried-out item became, per kind, for the repository's summary: "3 gemergt", "2 bereits gelöscht".
const DONE_TEXT = { merge: 'gemergt', close: 'geschlossen', rebase: 'Rebase angestoßen', branch: 'gelöscht', localBranch: 'lokal gelöscht', localSync: 'abgeglichen' };

function careBlock(job, step) {
  const set = job.plan;
  if (!set) return [waitsFor(job, step) || h('div', { class: 'note' }, step.state === 'running' ? 'Die App liest GitHub und lässt Claude die offenen PRs bewerten …' : 'Noch nicht geprüft.')];
  const decide = step.state === 'needsApproval';
  const markKey = `${job.id}:${set.id}`;
  const marks = exclusions.get(markKey) ?? { excluded: new Set(), included: new Set() };
  exclusions.set(markKey, marks);
  const cost = set.plans.reduce((sum, p) => sum + (p.reviewCost ?? 0), 0);
  const actionable = job.items.filter((i) => (i.optional ? marks.included.has(i.id) : !marks.excluded.has(i.id))).length;
  const offered = job.items.filter((i) => i.optional).length;
  return [
    h('div', { class: 'note' }, `Plan von ${formatTime(set.createdAt)}${cost ? `, Claude-Prüfung ${usd(cost)}` : ''}. ${decide
      ? `Häkchen entfernen nimmt einzelne Aktionen aus${offered ? '; unter „Zur Wahl“ kannst du Merges anhaken, von denen Claude abrät' : ''}. Mehr kann die Freigabe nicht hinzufügen. Vor jeder Aktion prüft die App erneut, was sich geändert hat, wird übersprungen.`
      : 'Erledigtes bleibt bestehen, auch nach Fehlern oder Abbruch.'}`),
    h('div', { class: 'runs' }, set.plans.map((p) => planRepo(job, p, decide, marks, set.plans.length === 1))),
    decide && decisionBar(
      button(actionable ? `Plan freigeben (${actionable} Aktion${actionable === 1 ? '' : 'en'})` : 'Freigeben (nichts ausführen)', async () => {
        if (await act('approve_plan', { id: job.id, planId: set.id, excluded: [...marks.excluded], included: [...marks.included] })) exclusions.delete(markKey);
      }, { cls: 'primary', iconName: 'check', disabled: !!view.running, id: 'plan-approve' }),
      button('Plan verwerfen', () => act('reject_plan', { id: job.id, planId: set.id }), { disabled: !!view.running }),
      button('Neu prüfen', () => act('redo_step', { id: job.id, step: 'care' }), { disabled: !!view.running, title: 'Liest GitHub erneut und lässt Claude neu bewerten (kostet Claude-Nutzung)' })),
    !decide && ['blocked', 'failed'].includes(step.state) && decisionBar(
      button('Neu prüfen', () => act('redo_step', { id: job.id, step: 'care' }), { disabled: !!view.running })),
  ].filter(Boolean);
}

/** Whether an item runs with the marks so far: an offered merge only when chosen, anything else unless left out. */
const runs = (item, marks) => (item.optional ? marks.included.has(item.id) : !marks.excluded.has(item.id));

function itemControl(item, decide, marks, rerender) {
  if (decide) {
    const set = item.optional ? marks.included : marks.excluded;
    return h('input', {
      type: 'checkbox', id: domId('item', item.id), checked: runs(item, marks), 'aria-label': `${item.label} ausführen`,
      onchange: (e) => { (e.target.checked === item.optional) ? set.add(item.id) : set.delete(item.id); rerender(); },
    });
  }
  if (!item.outcome) return item.writing ? h('span', { class: 'chip running' }, h('span', { class: 'spinner' }), 'Läuft') : h('span', { class: 'chip idle' }, 'Offen');
  const [cls, text] = OUTCOMES[item.outcome];
  return h('span', { class: `chip ${cls}` }, text);
}

function planRepo(job, p, decide, marks, single) {
  const items = job.items.filter((i) => i.repo === p.key);
  const rerender = () => { renderedJob = ''; renderRuns(); };
  const line = (item, main, extra = []) => h('div', { class: `item ${decide && !runs(item, marks) ? 'excluded' : ''}` },
    h('div', { class: 'item-line' }, itemControl(item, decide, marks, rerender), main),
    extra, !decide && item.detail && h('div', { class: 'item-reason selectable' }, item.detail));
  const prLine = (item) => {
    const pr = p.prs.find((x) => String(x.number) === item.reference);
    if (!pr) return line(item, h('span', null, item.label));
    const risk = pr.risk && RISKS[pr.risk];
    return line(item, [
      button(`#${pr.number}`, () => call('open_url', { url: pr.url }), { cls: 'link mono', title: 'Auf GitHub öffnen' }),
      h('span', { class: 'item-title', title: pr.title }, pr.bump ? `${pr.title.replace(/ from .* to .*$/, '')} ${pr.bump}` : pr.title),
      !pr.dependabot && h('span', { class: 'muted small' }, pr.author),
      h('span', { class: 'spacer' }), risk && badge(risk[0], `Claude: ${risk[1]}`)], [
      pr.reason && h('div', { class: 'item-reason selectable' }, pr.reviewed ? `Claude: ${pr.reason}` : pr.reason),
      pr.supersededBy && h('div', { class: 'item-reason' }, `Ersetzt durch #${pr.supersededBy}`),
      decide && item.optional && h('div', { class: 'item-check' }, 'Claude rät, ihn offen zu lassen; nach den Regeln der App wäre er mergebar. Anhaken heißt: nach der Freigabe mergen, mit denselben Prüfungen direkt davor (noch offen, gleicher Commit, mergebar, CI des PR-Commits grün).'),
      decide && APP_CHECK[pr.action] && h('div', { class: 'item-check' }, APP_CHECK[pr.action]),
      decide && pr.action === 'merge' && pr.dependabot && h('div', { class: 'item-check' }, 'Bedingt: Verursacht ein vorheriger Merge dieses Laufs einen Konflikt, stößt die App statt des Merges @dependabot rebase an.')]);
  };
  const branchLine = (item, list) => {
    const b = list.find((x) => x.name === item.reference);
    return line(item, [h('span', { class: 'mono item-title', title: item.reference }, item.reference), h('span', { class: 'spacer' }), decide && b?.proof.kind === 'afterMerge' && badge('', 'bedingt')],
      decide && b && h('div', { class: 'item-reason' }, `${b.reason}${b.proof.kind === 'afterMerge' ? '; nur wenn dieser Merge gelingt' : ''}. Vorher: gleicher Commit, nicht geschützt, kein offener PR, Nachweis gilt bei GitHub noch.`));
  };
  const group = (title, list) => list.length > 0 && [h('div', { class: 'item-group' }, `${title} (${list.length})`), ...list];
  const of = (kind) => items.filter((i) => i.kind === kind);
  const keep = p.prs.filter((pr) => pr.action === 'keep' && !pr.optional);
  const choice = items.filter((i) => i.optional);
  const counts = ['merge', 'close', 'rebase'].map((k) => [k, of(k).filter((i) => !i.optional).length]).filter(([, n]) => n);
  const branches = of('branch').length + of('localBranch').length;
  // Once carried out, the summary tells what happened, not what the plan intended.
  const carried = items.some((i) => i.outcome && i.outcome !== 'excluded');
  const results = () => {
    const counted = items.filter((i) => i.kind !== 'tag' && i.outcome && i.outcome !== 'excluded');
    const n = (test) => counted.filter(test).length;
    const done = Object.entries(DONE_TEXT).map(([kind, text]) => [n((i) => i.kind === kind && i.outcome === 'done'), text, 'green']);
    const gone = n((i) => i.outcome === 'alreadyDone');
    return [...done, [gone, 'bereits gelöscht', ''], [n((i) => i.outcome === 'skipped'), 'übersprungen', 'orange'],
      [n((i) => i.outcome === 'failed'), 'fehlgeschlagen', 'red'], [n((i) => i.outcome === 'cancelled'), 'abgebrochen', '']]
      .filter(([count]) => count > 0).map(([count, text, cls]) => badge(cls, `${count} ${text}`));
  };
  return h('details', { class: 'run repo', open: stepOpen.get(`${job.id}:plan:${p.key}`) ?? (single || !!p.error || decide), ontoggle: (e) => stepOpen.set(`${job.id}:plan:${p.key}`, e.target.open) },
    h('summary', null, h('strong', null, p.name), h('span', { class: 'muted small mono' }, p.repo), h('span', { class: 'spacer' }),
      p.error && badge('red', 'nicht lesbar'),
      carried ? results() : [
        counts.map(([k, n]) => badge(k === 'merge' ? 'green' : k === 'close' ? 'red' : 'accent', `${n} ${{ merge: 'mergen', close: 'schließen', rebase: 'Rebase' }[k]}`)),
        choice.length > 0 && badge('orange', `${choice.length} zur Wahl`),
        of('tag').length > 0 && badge('green', `Tag ${p.tag ?? of('tag')[0].reference} geplant`),
        branches > 0 && badge('', `${branches} Branches löschen`)],
      keep.length > 0 && badge('', `${keep.length} bleiben offen`)),
    h('div', { class: 'run-body' },
      p.error && h('div', { class: 'note error-text' }, p.error),
      p.reviewNote && h('div', { class: 'note orange' }, p.reviewNote),
      h('div', { class: 'items' },
        group('Mergen', of('merge').filter((i) => !i.optional).map(prLine)),
        group('Zur Wahl: Claude rät, offen zu lassen', choice.map(prLine)),
        group('Schließen', of('close').map(prLine)),
        group('Rebase anstoßen', of('rebase').map(prLine)),
        group('Tag', of('tag').map((item) => line(item, [h('strong', null, item.reference), h('span', { class: 'muted small' }, `nach ${p.lastTag ?? '?'}`), h('span', { class: 'spacer' }), decide && badge('', 'bedingt')],
          decide && h('div', { class: 'item-check' }, `Im Release-Schritt: nur wenn alle geplanten Merges gelingen, ${p.defaultBranch} sich sonst nicht bewegt und die CI genau dieses Commits grün ist (bis zu 30 Minuten). Direkt vor dem Schreiben prüft die App die Release-Linie erneut.`)))),
        !p.tag && p.tagNote && job.options.release && [h('div', { class: 'item-group' }, 'Tag'), h('div', { class: 'item-reason' }, `Kein Tag: ${p.tagNote}`)],
        group('Aufräumen auf GitHub', of('branch').map((i) => branchLine(i, p.branches))),
        group('Lokale Kopie abgleichen', of('localSync').map((i) => line(i, h('span', null, 'git fetch --prune origin'), decide && h('div', { class: 'item-reason' }, `Vor dem Löschen, weil ${p.localSync}. Misslingt der Abgleich, löscht die App lokal nichts.`)))),
        group('Aufräumen lokal', of('localBranch').map((i) => branchLine(i, p.localBranches))),
        keep.length > 0 && [h('div', { class: 'item-group' }, `Bleibt offen (${keep.length})`), ...keep.map((pr) => h('div', { class: 'item' },
          h('div', { class: 'item-line' }, button(`#${pr.number}`, () => call('open_url', { url: pr.url }), { cls: 'link mono' }), h('span', { class: 'item-title' }, pr.title), h('span', { class: 'spacer' }), pr.risk && badge(RISKS[pr.risk][0], `Claude: ${RISKS[pr.risk][1]}`)),
          h('div', { class: 'item-reason selectable' }, pr.reviewed ? `Claude: ${pr.reason}` : pr.reason)))]),
      p.cleanupNote && h('div', { class: 'note orange' }, p.cleanupNote),
      p.cloneNote && h('div', { class: 'note orange' }, `Lokale Branches übersprungen: ${p.cloneNote}`),
      p.githubDeletesMerged && of('merge').length > 0 && h('div', { class: 'note' }, 'GitHub löscht die Branches gemergter PRs in diesem Repository selbst.'),
      !p.error && !items.length && !keep.length && h('div', { class: 'note' }, 'Nichts zu tun.')));
}

function releaseBlock(job, step) {
  const plan = job.plan?.plans.find((p) => p.key === step.subject);
  const rule = view.repositories.find((r) => r.key === step.subject)?.release;
  return [
    waitsFor(job, step),
    h('div', { class: 'facts' },
      h('span', null, h('strong', null, 'Repository: '), repoName(step.subject)),
      plan?.tag && h('span', null, h('strong', null, 'Geplant: '), `${plan.tag} nach ${plan.lastTag}`),
      step.tag && h('span', null, h('strong', null, 'Gesetzt: '), `${step.tag} auf ${short(step.commit)}`),
      rule && h('span', null, h('strong', null, 'GitHub-Release: '), rule.publishesRelease ? 'die App wartet darauf (bis zu 30 Minuten je Prüfung)' : 'laut Regel keins')),
    step.state === 'waiting' && h('div', { class: 'note orange' }, `${step.detail}. „Erneut prüfen“ oben fragt GitHub wieder; abhängige Schritte warten so lange.`),
  ].filter(Boolean);
}

function proposalFacts(p) {
  return h('div', { class: 'facts' },
    h('span', null, h('strong', null, 'Quellen: '), p.sources.map((s) => `${s.label.includes(' ') ? `${s.repo} @ ${s.label}` : `${s.repo} ${s.label}`} (${short(s.rev)})`).join(', ')),
    h('span', null, h('strong', null, 'Ausgangsstand: '), `${p.baseBranch} ${short(p.baseCommit)}`),
    p.branch && h('span', null, h('strong', null, 'Neuer Branch: '), h('span', { class: 'mono' }, p.branch)),
    p.costUsd != null && h('span', null, h('strong', null, 'Claude: '), usd(p.costUsd)));
}

function checksView(checks) {
  return h('div', { class: 'checks' }, checks.map((c) => [
    h('span', { class: `status-dot ${c.ok ? 'ok' : 'error'}` }), h('span', { class: 'label' }, c.name), h('span', { class: 'detail-text selectable' }, c.detail)]));
}

function filesView(files) {
  return h('ul', { class: 'plain-list' }, files.map((f) => h('li', null, h('span', { class: 'mono' }, f.path), ' ',
    h('span', { class: 'muted small' }, [f.added ? 'neu' : 'geändert', f.binary ? 'Binärdatei' : null, `${Math.max(1, Math.round(f.size / 1024))} KB`].filter(Boolean).join(', ')))));
}

function diffView(text) {
  const pre = h('pre', { class: 'diff selectable', tabindex: '0', 'aria-label': 'Änderungen' });
  const lines = text.split('\n');
  const parts = lines.map((line) => {
    const cls = line.startsWith('diff --git') ? 'd-head' : line.startsWith('+++') || line.startsWith('---') ? 'd-file'
      : line.startsWith('@@') ? 'd-hunk' : line.startsWith('+') ? 'd-add' : line.startsWith('-') ? 'd-del' : '';
    return h('span', { class: cls || null }, `${line}\n`);
  });
  pre.append(...parts);
  return pre;
}

/** The tail line the app adds to a diff it cut short, e.g. "… gekürzt; vollständig: git -C … show abc1234". */
function diffCut(diff) {
  const at = diff.lastIndexOf('\n… gekürzt');
  return at < 0 ? null : diff.slice(at + 1).trim();
}

/** The technical side of a proposal, after the decision: files, statistics, the diff and its limits. */
function technicalPreview(key, { stat, files, diff }) {
  const cut = diffCut(diff);
  return h('details', { class: 'run tech', open: stepOpen.get(key) ?? true, ontoggle: (e) => stepOpen.set(key, e.target.open) },
    h('summary', null, h('strong', null, 'Technische Änderungsvorschau'), cut && badge('orange', 'gekürzt')),
    h('div', { class: 'run-body' },
      files && h('div', { class: 'section' }, h('strong', { class: 'small' }, `Geänderte Dateien (${files.length})`), filesView(files)),
      stat && h('details', { class: 'fold' }, h('summary', null, 'Git-Statistik'), h('pre', { class: 'result-text mono' }, stat)),
      cut && h('div', { class: 'note orange' }, `Die Vorschau ist gekürzt. Vollständig lokal: ${cut.replace(/^… gekürzt; vollständig: /, '')}`),
      diffView(diff)));
}

/** What a decision is about, before any technical detail: the subject, the change, the effect of approving. */
function decisionBrief(rows, summary) {
  return h('div', { class: 'brief' },
    h('dl', { class: 'brief-facts' }, rows.filter(Boolean).map(([label, value]) => [h('dt', null, label), h('dd', { class: 'selectable' }, value)])),
    summary && h('div', { class: 'brief-summary' }, h('strong', { class: 'small' }, 'Claudes Zusammenfassung'), markdown(summary)));
}

function contentBlock(job, step) {
  const target = view.targets.find((t) => t.id === step.subject);
  const p = step.content;
  const decide = step.state === 'needsApproval';
  const redoable = ['needsApproval', 'blocked', 'failed', 'nothingToDo'].includes(step.state) && !step.pr && !step.writing;
  const docs = step.kind === 'docs';
  const effect = docs
    ? `Ein neuer Branch und ein Pull Request mit genau diesem Stand${target?.mergeWhenGreen ? '; bei grüner CI mergt die App ihn' : '; gemergt wird nicht automatisch'}. Ist die Datei auf dem Zielbranch inzwischen geändert, stoppt die App.`
    : 'Genau dieser Stand wird per Push ohne Force ins Wiki veröffentlicht. Hat jemand das Wiki inzwischen geändert, stoppt die App und überschreibt nichts.';
  const failedChecks = p?.checks.filter((c) => !c.ok) ?? [];
  return [
    waitsFor(job, step),
    !p && h('div', { class: 'facts' },
      h('span', null, h('strong', null, 'Ziel: '), target ? `${target.name} (${KIND_TEXT[target.kind]}) in ${target.repo}` : `${step.subject} (gelöscht)`),
      h('span', null, h('strong', null, 'Basis: '), BASIS_TEXT[step.basis ?? target?.basis ?? 'commit'])),
    p && decisionBrief([
      ['Ziel', target ? `${target.name} (${KIND_TEXT[target.kind]}) in ${target.repo}` : `${step.subject} (gelöscht)`],
      ['Auf Grundlage von', `${p.sources.map((s) => (s.label.includes(' ') ? `${s.repo} @ ${s.label}` : `${s.repo} ${s.label}`)).join(', ')} (${BASIS_TEXT[step.basis ?? target?.basis ?? 'commit']})`],
      ['Änderung', `${p.files.length} ${p.files.length === 1 ? 'Datei' : 'Dateien'}: ${p.files.slice(0, 4).map((f) => f.path).join(', ')}${p.files.length > 4 ? ` und ${p.files.length - 4} weitere` : ''}`],
      ['Prüfungen der App', failedChecks.length ? h('span', { class: 'red' }, `${failedChecks.length} von ${p.checks.length} nicht bestanden`) : `alle ${p.checks.length} bestanden`],
      decide && ['Freigeben bewirkt', effect],
    ], p.summary),
    decide && decisionBar(
      button(docs ? (target?.mergeWhenGreen ? 'Freigeben: PR erstellen und bei grüner CI mergen' : 'Freigeben: Pull Request erstellen') : 'Freigeben: im Wiki veröffentlichen',
        () => act('approve_step', { id: job.id, step: step.id, fingerprint: p.fingerprint }), { cls: 'primary', iconName: 'check', disabled: !!view.running }),
      button('Verwerfen', () => act('reject_step', { id: job.id, step: step.id }), { disabled: !!view.running }),
      button('Neu vorbereiten', () => act('redo_step', { id: job.id, step: step.id }), { disabled: !!view.running, title: 'Neue Quellstände, neuer Claude-Lauf (kostet Claude-Nutzung)' })),
    !decide && redoable && decisionBar(
      button('Neu vorbereiten', () => act('redo_step', { id: job.id, step: step.id }), { disabled: !!view.running }),
      step.state !== 'nothingToDo' && button('Verwerfen', () => act('reject_step', { id: job.id, step: step.id }), { disabled: !!view.running })),
    p && h('details', { class: 'fold' }, h('summary', null, 'Prüfungen und Stände im Einzelnen'),
      proposalFacts(p), checksView(p.checks)),
    p && technicalPreview(`${job.id}:${step.id}:tech`, { files: p.files, diff: p.diff }),
  ].filter(Boolean);
}

function websiteBlock(job, step) {
  const w = step.website;
  const decide = step.state === 'needsApproval';
  const site = view.site?.name ?? 'die Website';
  return [
    waitsFor(job, step),
    !w && h('div', { class: 'note' }, 'Aktualisiert die Website für die Produkte der Repositories dieses Auftrags (mit den Website-Aufgaben), baut sie und committet lokal. Veröffentlicht wird erst nach deiner Freigabe: Push, SFTP-Upload, Prüfung der Live-Seite.'),
    w && decisionBrief([
      ['Betroffen', w.products.length ? `${w.products.join(', ')} auf ${site}` : `${site} (keine Produktversion geändert)`],
      ['Änderung', `lokaler Commit ${short(w.commit)} auf ${short(w.base)}`],
      decide && ['Freigeben bewirkt', `Genau dieser Commit wird gepusht, die geänderten Dateien per SFTP auf ${view.site?.url?.replace(/^https?:\/\//, '') ?? 'den Server'} hochgeladen und die Live-Seite Datei für Datei geprüft.`],
      decide && ['Verwerfen bewirkt', 'Nichts wird veröffentlicht; der Commit bleibt lokal im Website-Projekt stehen.'],
    ], w.summary),
    decide && decisionBar(
      button('Freigeben: Website veröffentlichen', () => act('approve_step', { id: job.id, step: step.id, fingerprint: w.commit }), { cls: 'primary', iconName: 'check', disabled: !!view.running }),
      button('Verwerfen', () => act('reject_step', { id: job.id, step: step.id }), { disabled: !!view.running }),
      button('Neu vorbereiten', () => act('redo_step', { id: job.id, step: step.id }), { disabled: !!view.running, title: 'Neuer Claude-Lauf (kostet Claude-Nutzung)' })),
    !decide && ['blocked', 'failed', 'nothingToDo'].includes(step.state) && !step.writing && decisionBar(
      button('Neu vorbereiten', () => act('redo_step', { id: job.id, step: step.id }), { disabled: !!view.running })),
    w && technicalPreview(`${job.id}:${step.id}:tech`, { stat: w.stat, diff: w.diff }),
  ].filter(Boolean);
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

// MARK: Dialogs

/** A modal dialog whose content `build` returns; `render` builds it again. A re-render keeps the
 *  scroll position and gives the focus back to the element with the same id (controls that change
 *  the dialog need a stable id). */
function openDialog(title, build) {
  const dialog = h('dialog', { 'aria-label': title, tabindex: '-1' });
  const close = () => dialog.close();
  // The id that last had the focus; a control disabled while busy gets it back once enabled again.
  let wanted = null;
  const render = () => {
    const focused = dialog.contains(document.activeElement) ? document.activeElement : null;
    if (focused && focused !== dialog) wanted = focused.id || null;
    const focusId = wanted;
    const scroll = dialog.querySelector('.dialog-body')?.scrollTop ?? 0;
    const { body, actions } = build(close, render);
    dialog.replaceChildren(h('form', { method: 'dialog', onsubmit: (e) => e.preventDefault() },
      h('div', { class: 'dialog-body' }, body), h('div', { class: 'dialog-actions' }, actions)));
    const bodyEl = dialog.querySelector('.dialog-body');
    if (bodyEl) bodyEl.scrollTop = scroll;
    if (focused) {
      const again = focusId && dialog.querySelector(`#${CSS.escape(focusId)}`);
      // Without the same element, the dialog itself keeps the focus instead of the page behind it.
      (again && !again.disabled ? again : dialog).focus({ preventScroll: true });
    }
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

/** Asks before something that cannot be undone from here; `action` runs on confirmation. Cancel has
 *  the focus, so Enter never confirms by accident. */
function confirmDialog(title, text, label, action, { danger = false } = {}) {
  const { dialog } = openDialog(title, (close) => ({
    body: [h('h2', null, title), h('p', { class: 'selectable', style: 'margin:0' }, text)],
    actions: [button('Abbrechen', close, { id: 'confirm-cancel' }), button(label, async () => { close(); await action(); }, { cls: danger ? 'primary danger-fill' : 'primary' })],
  }));
  dialog.querySelector('#confirm-cancel')?.focus();
}

function row(label, control, id) {
  if (id) control.id = id;
  return h('div', { class: 'field-row' }, h('label', { for: control.id || null }, label), control);
}

// MARK: Repository picker

let repoCache = null;

async function loadRepos(force = false) {
  if (!repoCache || force) repoCache = await invoke('list_repos');
  return repoCache;
}

/** Filterable list of the user's repositories. Keeps its own state, so typing never re-renders the
 *  dialog. With `multi`, rows toggle in `picked` (a Set of full names); else `onPick` gets the row. */
function repoPicker({ include = () => true, onPick, multi = false, picked = new Set(), locked = () => null }) {
  const state = { query: '', data: null, error: null, picked: null };
  const list = h('div', { class: 'repo-list', role: 'listbox', 'aria-multiselectable': multi ? 'true' : null });
  const note = h('div', { class: 'note' });
  const search = h('input', { type: 'search', id: 'pick-search', placeholder: 'Repositories filtern …', spellcheck: 'false', autocomplete: 'off', 'aria-label': 'Repositories filtern',
    oninput: (e) => { state.query = e.target.value.trim().toLowerCase(); renderList(); } });
  const tag = (text, cls = '') => h('span', { class: `badge ${cls}` }, text);
  const renderList = () => {
    const focusId = list.contains(document.activeElement) ? document.activeElement.id : null;
    renderRows();
    if (focusId) document.getElementById(focusId)?.focus({ preventScroll: true });
  };
  const renderRows = () => {
    if (state.error) { list.replaceChildren(h('div', { class: 'note error-text' }, state.error)); return; }
    if (!state.data) { list.replaceChildren(h('div', { class: 'note' }, 'Repositories werden geladen …')); return; }
    const repos = state.data.repos.filter(include).filter((r) => !state.query
      || r.fullName.toLowerCase().includes(state.query) || (r.description ?? '').toLowerCase().includes(state.query));
    note.textContent = [state.data.note, `Lokale Klone werden in ${state.data.workspace} gesucht.`].filter(Boolean).join(' ');
    list.replaceChildren(...(repos.length ? repos.map((r) => {
      const why = locked(r);
      const on = multi ? picked.has(r.fullName) : state.picked === r.fullName;
      return h('button', {
        type: 'button', role: 'option', id: domId('pick-row', r.fullName), 'aria-selected': String(on), class: `repo-row ${on ? 'picked' : ''}`, disabled: !!why, title: why,
        onclick: () => {
          if (multi) on ? picked.delete(r.fullName) : picked.add(r.fullName);
          else state.picked = r.fullName;
          renderList();
          onPick?.(r);
        },
      },
        h('div', { class: 'repo-head' },
          multi && h('input', { type: 'checkbox', checked: on, disabled: !!why, tabindex: '-1', 'aria-hidden': 'true' }),
          h('strong', null, r.fullName),
          r.managed && tag('verwaltet', 'green'),
          r.private && tag('privat', 'orange'),
          r.isWebsite && tag('Website', 'accent'),
          r.onSite && tag('Produkt der Website', ''),
          r.archived && tag('archiviert', 'orange'),
          r.localPath ? tag('lokal', 'green') : tag('nicht lokal')),
        r.description && h('div', { class: 'muted small' }, r.description));
    }) : [h('div', { class: 'note' }, 'Keine passenden Repositories.')]));
  };
  const load = async (force) => {
    state.error = null;
    state.data = force ? null : state.data;
    renderList();
    try { state.data = await loadRepos(force); } catch (e) { state.error = String(e); }
    renderList();
  };
  const el = h('div', { class: 'repo-picker' },
    h('div', { class: 'line' }, search, button('Neu laden', () => load(true), { id: 'pick-reload', iconName: 'refresh', iconOnly: true })),
    list, note);
  load(false);
  return el;
}

// MARK: Adding repositories

/** Picks repositories from the local clones in the repo folder (or another folder), or by name for
 *  one without a clone, and adds them to the managed list. */
function addReposDialog() {
  const picked = new Map();
  const s = { dir: '', data: null, error: '', query: '', manual: '', busy: false };
  const list = h('div', { class: 'repo-list', role: 'listbox', 'aria-multiselectable': 'true', 'aria-label': 'Lokale Repositories' });
  // Built once and updated with the list, so filtering never rebuilds the dialog.
  const bulk = h('div', { class: 'line wrap' });
  let render = () => {};
  const manual = () => s.manual.split(/[\s,]+/).filter(Boolean);
  const count = () => picked.size + manual().length;
  const updateButton = () => {
    const b = document.getElementById('add-confirm');
    if (b) { b.disabled = s.busy || !count(); b.textContent = count() ? `Aufnehmen (${count()})` : 'Aufnehmen'; }
  };
  const tag = (text, cls = '') => h('span', { class: `badge ${cls}` }, text);
  const matches = (r) => !s.query || r.repo.toLowerCase().includes(s.query) || r.path.toLowerCase().includes(s.query);
  const renderBulk = () => {
    if (!s.data) { bulk.replaceChildren(); return; }
    // Bulk choices act on the visible matches only; a choice hidden by the filter stays as it is.
    const free = s.data.repos.filter((r) => !r.managed && matches(r));
    const hidden = [...picked.keys()].filter((repo) => !free.some((r) => r.repo === repo)).length;
    const filtered = !!s.query;
    bulk.replaceChildren(
      h('span', { class: 'spacer muted small', role: 'status' },
        [`${picked.size} ausgewählt${hidden && filtered ? `, davon ${hidden} ausgeblendet` : ''}`,
          filtered && `${s.data.repos.filter(matches).length} von ${s.data.repos.length} angezeigt`].filter(Boolean).join(' · ')),
      button(filtered ? 'Alle Treffer auswählen' : 'Alle auswählen', () => { for (const r of free) picked.set(r.repo, r.path); renderList(); },
        { id: 'add-all', disabled: !free.length || free.every((r) => picked.has(r.repo)) }),
      button(filtered ? 'Treffer abwählen' : 'Alle abwählen', () => { for (const r of free) picked.delete(r.repo); renderList(); },
        { id: 'add-none', disabled: !free.some((r) => picked.has(r.repo)) }));
  };
  const renderList = () => {
    const focusId = list.contains(document.activeElement) || bulk.contains(document.activeElement) ? document.activeElement.id : null;
    renderBulk();
    updateButton();
    if (!s.data) { list.replaceChildren(h('div', { class: 'note' }, 'Lokale Repositories werden gesucht …')); return; }
    const repos = s.data.repos.filter(matches);
    list.replaceChildren(...(repos.length ? repos.map((r) => {
      const on = picked.has(r.repo);
      return h('button', {
        type: 'button', role: 'option', id: domId('add-row', r.repo), 'aria-selected': String(on || r.managed), class: `repo-row ${on ? 'picked' : ''}`, disabled: r.managed,
        title: r.managed ? 'wird schon verwaltet' : null,
        onclick: () => { on ? picked.delete(r.repo) : picked.set(r.repo, r.path); renderList(); },
      },
        h('div', { class: 'repo-head' },
          h('input', { type: 'checkbox', checked: on || r.managed, disabled: r.managed, tabindex: '-1', 'aria-hidden': 'true' }),
          h('strong', null, r.repo),
          r.managed && tag('verwaltet', 'green'),
          r.isWebsite && tag('Website', 'accent'),
          r.onSite && tag('Produkt der Website')),
        h('div', { class: 'muted small mono' }, r.path),
        r.others.length > 0 && h('div', { class: 'muted small' }, `Weitere Klone: ${r.others.join(', ')} (in der Verwaltung änderbar)`));
    }) : [h('div', { class: 'note' }, s.data.repos.length ? 'Keine passenden Repositories.' : 'In diesem Ordner liegt kein Git-Klon mit GitHub-Origin.')]));
    if (focusId) {
      // A bulk button that has just become pointless is disabled; the other one takes the focus.
      const again = document.getElementById(focusId);
      (again && !again.disabled ? again : bulk.querySelector('button:not(:disabled)') ?? search).focus({ preventScroll: true });
    }
  };
  const load = async () => {
    s.data = null;
    renderList();
    s.data = await invoke('list_local_repos', { dir: s.dir || null });
    renderList();
    render();
  };
  const search = h('input', { type: 'search', id: 'add-search', placeholder: 'Filtern …', spellcheck: 'false', autocomplete: 'off', 'aria-label': 'Repositories filtern',
    oninput: (e) => { s.query = e.target.value.trim().toLowerCase(); renderList(); } });
  openDialog('Repositories auswählen', (close, rerender) => {
    render = rerender;
    return {
      body: [
        h('h2', null, 'Repositories auswählen'),
        h('p', { class: 'note', style: 'margin:0' }, 'Die lokalen Git-Klone mit GitHub-Origin; die lokale Kopie wird gleich mit eingetragen. Nichts wird geklont oder geändert.'),
        h('div', { class: 'line' },
          h('span', { class: 'spacer muted small' }, s.data ? `Ordner: ${s.data.folder} · ${s.data.repos.length} Repositories` : ''),
          button('Anderer Ordner …', async () => { const p = await call('pick_folder'); if (p) { s.dir = p; picked.clear(); load(); } }, { id: 'add-folder' }),
          button('Neu einlesen', load, { id: 'add-reload', iconName: 'refresh', iconOnly: true })),
        h('div', { class: 'repo-picker' }, search, bulk, list),
        h('div', { class: 'field-col' }, h('label', { for: 'add-manual' }, 'Ohne lokale Kopie: owner/name eingeben (mehrere durch Leerzeichen getrennt)'),
          h('input', { type: 'text', id: 'add-manual', value: s.manual, spellcheck: 'false', autocomplete: 'off', placeholder: 'owner/name', 'aria-describedby': 'add-manual-note',
            oninput: (e) => { s.manual = e.target.value; updateButton(); } }),
          h('div', { class: 'note', id: 'add-manual-note' }, 'Status und PR-Pflege gehen auch ohne Klon; der Name muss auf GitHub existieren.')),
        s.error && h('div', { class: 'note error-text', role: 'alert' }, s.error),
      ],
      actions: [
        button('Abbrechen', close),
        button(s.busy ? 'Wird aufgenommen …' : count() ? `Aufnehmen (${count()})` : 'Aufnehmen', async () => {
          const entries = [...picked].map(([repo, localPath]) => ({ repo, localPath }))
            .concat(manual().map((repo) => ({ repo, localPath: '' })));
          s.busy = true; s.error = ''; render();
          try {
            await invoke('add_repositories', { repos: entries });
            close();
            if (area !== 'repos') showArea('repos');
          } catch (error) { s.error = String(error); s.busy = false; render(); }
        }, { id: 'add-confirm', cls: 'primary', disabled: s.busy || !count() }),
      ],
    };
  });
  load();
}

// MARK: Content target dialog

// Fields of the target form in the order they appear; the first with an error gets the focus.
const TARGET_FIELDS = ['name', 'sources', 'repo', 'paths'];

/** Which field a message of the app's own check (content::validate) belongs to; null = the form as a whole. */
function targetErrorField(message) {
  if (/^Name fehlt/.test(message)) return 'name';
  if (/Quell/.test(message)) return 'sources';
  if (/Format owner\/name/.test(message)) return 'repo';
  if (/Pfad|„.*“:|ganze Repository/.test(message)) return 'paths';
  return null;
}

/** Creates or edits a README/docs or wiki target. `source`: the repository it is created from. */
function targetDialog(target, source) {
  const managed = view.repositories;
  const t = target ? structuredClone(target) : {
    id: '', kind: 'docs', name: 'README', sources: source ? [source] : [], repo: source ? repoName(source) : '', branch: '', paths: ['README.md'],
    instructions: '', skill: '', model: '', basis: 'release', check: '', mergeWhenGreen: false,
  };
  // `shown`: fields whose own check shows (left once, or after a save); `server`: messages of the app's check.
  const s = { error: '', paths: t.paths.join('\n'), shown: new Set(), server: {}, sourceQuery: '', advanced: !!(t.skill || t.model || t.check) };
  const fieldRefs = {};
  const check = () => ({
    name: !t.name.trim() && 'Name fehlt.',
    sources: !t.sources.length && 'Mindestens ein Quell-Repository wählen.',
    repo: !/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(t.repo.trim())
      && (t.repo.trim() ? `„${t.repo}“ ist kein Repository im Format owner/name.` : t.kind === 'docs' ? 'Ziel-Repository wählen.' : 'Repository des Wikis wählen.'),
    paths: !s.paths.split('\n').some((p) => p.trim()) && 'Mindestens einen erlaubten Pfad angeben.',
  });
  // Updates the messages in place, so typing never rebuilds the form.
  const paint = () => {
    const errors = check();
    for (const [field, ref] of Object.entries(fieldRefs)) {
      const text = s.server[field] || (s.shown.has(field) && errors[field]) || '';
      ref.message.textContent = text;
      ref.message.hidden = !text;
      ref.control.setAttribute('aria-invalid', text ? 'true' : 'false');
    }
  };
  const leave = (field) => () => { s.shown.add(field); paint(); };
  const edited = (field) => { delete s.server[field]; paint(); };
  const field = (name, control) => {
    const message = h('div', { class: 'field-error', id: `t-${name}-error`, role: 'alert', hidden: true });
    control.setAttribute('aria-describedby', [control.getAttribute('aria-describedby'), message.id].filter(Boolean).join(' '));
    fieldRefs[name] = { control, message };
    return message;
  };
  const required = (label) => [label, h('span', { class: 'required', 'aria-hidden': 'true' }, ' *')];
  // The sources are a group of checkboxes: the focus goes to the first one.
  const focusField = (name) => {
    const control = fieldRefs[name]?.control;
    (control?.id === 't-sources' ? control.querySelector('input:not([disabled])') ?? control : control)?.focus();
  };
  const { dialog } = openDialog(target ? 'Inhaltsziel bearbeiten' : 'Inhaltsziel hinzufügen', (close, render) => {
    const docs = t.kind === 'docs';
    const setKind = (kind) => {
      if (kind === t.kind) return;
      t.kind = kind;
      if (!target) {
        t.name = kind === 'wiki' ? 'Wiki' : 'README';
        s.paths = kind === 'wiki' ? '*.md' : 'README.md';
        t.mergeWhenGreen = false;
      }
      s.server = {};
      render();
    };
    const nameInput = h('input', { type: 'text', id: 't-name', value: t.name, required: true, spellcheck: 'false', onblur: leave('name'), oninput: (e) => { t.name = e.target.value; edited('name'); } });
    const sourceBox = h('div', { class: 'source-list', id: 't-sources', role: 'group', tabindex: '-1', 'aria-labelledby': 't-sources-label' },
      managed.map((r) => h('label', { class: 'check-line', 'data-name': r.repo.toLowerCase(), hidden: !!s.sourceQuery && !r.repo.toLowerCase().includes(s.sourceQuery) },
        h('input', { type: 'checkbox', id: domId('t-source', r.key), checked: t.sources.includes(r.key), onchange: (e) => {
          t.sources = e.target.checked ? [...t.sources, r.key] : t.sources.filter((k) => k !== r.key);
          s.shown.add('sources');
          edited('sources');
          if (!t.repo && e.target.checked) { t.repo = r.repo; render(); }
        } }), h('span', { class: 'mono' }, r.repo))));
    const repoSelect = h('select', { id: 't-repo', required: true, onblur: leave('repo'), onchange: (e) => { t.repo = e.target.value; s.shown.add('repo'); edited('repo'); } },
      !managed.some((r) => r.repo === t.repo) && t.repo && h('option', { value: t.repo, selected: true }, t.repo),
      h('option', { value: '', selected: !t.repo }, '– wählen –'),
      managed.map((r) => h('option', { value: r.repo, selected: r.repo === t.repo }, r.repo)));
    const pathsInput = h('textarea', { id: 't-paths', rows: 3, value: s.paths, required: true, spellcheck: 'false', 'aria-describedby': 't-paths-note', onblur: leave('paths'), oninput: (e) => { s.paths = e.target.value; edited('paths'); } });
    const body = [
      h('h2', null, target ? `${t.name} bearbeiten` : 'Inhaltsziel hinzufügen'),
      h('p', { class: 'note', style: 'margin:0' }, 'Claude ändert nur in einem Arbeitsordner der App; du siehst jede Änderung vor der Freigabe. Pflichtfelder sind mit * markiert.'),
      h('fieldset', null, h('legend', null, 'Inhalt'),
        h('div', { class: 'choice-row', role: 'radiogroup', 'aria-label': 'Art' },
          h('label', { class: 'check-line top' }, h('input', { type: 'radio', name: 't-kind', id: 't-kind-docs', checked: docs, onchange: () => setKind('docs') }),
            h('span', null, h('strong', null, 'README oder Dokumentation'), h('div', { class: 'note' }, 'Als neuer Branch und Pull Request, nie direkt auf den Default-Branch.'))),
          h('label', { class: 'check-line top' }, h('input', { type: 'radio', name: 't-kind', id: 't-kind-wiki', checked: !docs, onchange: () => setKind('wiki') }),
            h('span', null, h('strong', null, 'GitHub-Wiki'), h('div', { class: 'note' }, 'Nach Freigabe ein Push ohne Force, nur wenn niemand das Wiki inzwischen geändert hat.')))),
        h('div', { class: 'field-row' }, h('label', { for: 't-name' }, required('Name')), h('div', { class: 'field-stack' }, nameInput, field('name', nameInput))),
        h('div', { class: 'field-col' }, h('label', { for: 't-instructions' }, 'Auftrag an Claude'),
          h('textarea', { id: 't-instructions', rows: 3, value: t.instructions, placeholder: 'z. B. „Halte Installation, Funktionen und Konfiguration in der README auf dem Stand der Quellen; Stil und Gliederung beibehalten.“',
            oninput: (e) => { t.instructions = e.target.value; } }))),
      h('fieldset', null, h('legend', null, 'Quellen und Ziel'),
        h('div', { class: 'field-col' },
          h('span', { class: 'field-label', id: 't-sources-label' }, required('Quellen: die Repositories, deren Stand beschrieben wird')),
          managed.length > 6 && h('input', { type: 'search', id: 't-source-search', value: s.sourceQuery, placeholder: 'Quellen filtern …', 'aria-label': 'Quellen filtern', spellcheck: 'false', autocomplete: 'off',
            oninput: (e) => {
              s.sourceQuery = e.target.value.trim().toLowerCase();
              for (const item of sourceBox.children) item.hidden = !!s.sourceQuery && !item.dataset.name.includes(s.sourceQuery);
            } }),
          sourceBox, field('sources', sourceBox)),
        h('div', { class: 'field-row' }, h('label', { for: 't-repo' }, required(docs ? 'Ziel-Repository' : 'Wiki von')), h('div', { class: 'field-stack' }, repoSelect, field('repo', repoSelect))),
        docs && row('Zielbranch', h('input', { type: 'text', value: t.branch, placeholder: 'leer = Default-Branch', spellcheck: 'false', oninput: (e) => { t.branch = e.target.value; } }), 't-branch'),
        h('div', { class: 'field-col' }, h('label', { for: 't-paths' }, required('Erlaubte Pfade (einer pro Zeile)')), pathsInput, field('paths', pathsInput),
          h('div', { class: 'note', id: 't-paths-note' }, '„README.md“ eine Datei, „docs/“ alles darunter, „docs/*.md“ Dateien in einem Ordner, „**/*.md“ über Ordner hinweg. .git, .github und Git-Steuerdateien sind nie erlaubt.'))),
      h('fieldset', null, h('legend', null, 'Aktualisierung'),
        row('Basis', h('select', { onchange: (e) => { t.basis = e.target.value; } },
          h('option', { value: 'release', selected: t.basis === 'release' }, 'Neuestes Release der Quellen'),
          h('option', { value: 'commit', selected: t.basis === 'commit' }, 'Aktueller Stand des Default-Branches')), 't-basis'),
        h('div', { class: 'note' }, 'Pro Auftrag änderbar. Mit „aktueller Stand“ geht es auch ohne neues Release.'),
        docs ? h('label', { class: 'check-line top' }, h('input', { type: 'checkbox', id: 't-merge', checked: t.mergeWhenGreen, onchange: (e) => { t.mergeWhenGreen = e.target.checked; } }),
          h('span', null, h('strong', null, 'Nach Freigabe mergen, sobald die CI grün ist'), h('div', { class: 'note' }, 'Sonst bleibt der Pull Request offen. Ohne CI-Checks wird nicht automatisch gemergt.')))
          : h('div', { class: 'note' }, 'Ein Wiki wird nach deiner Freigabe direkt veröffentlicht; es hat keine Pull Requests.'),
        h('div', { class: 'note' }, 'Jede Vorbereitung ist ein Claude-Lauf und kostet Claude-Nutzung; es gilt die Kostengrenze aus den Einstellungen.')),
      h('details', { class: 'fold', open: s.advanced, ontoggle: (e) => { s.advanced = e.target.open; } },
        h('summary', null, 'Erweiterte Optionen', (t.skill || t.model || t.check) ? h('span', { class: 'muted small' }, ' · angepasst') : null),
        h('div', { class: 'fold-body' },
          row('Skill (optional)', h('input', { type: 'text', value: t.skill, placeholder: 'Name eines Claude-Code-Skills', spellcheck: 'false', oninput: (e) => { t.skill = e.target.value; } }), 't-skill'),
          row('Modell', modelSelect(t.model, view.config.claudeModel || view.site?.claude?.model, (model) => { t.model = model; }, 't-model'), 't-model'),
          row('Prüfbefehl (optional)', h('input', { type: 'text', value: t.check, placeholder: 'z. B. npx --yes markdownlint-cli2 README.md', spellcheck: 'false', oninput: (e) => { t.check = e.target.value; } }), 't-check'),
          h('div', { class: 'note' }, 'Läuft im Arbeitsordner nach Claudes Änderung, ohne deinen GitHub-Token; Exit-Code 0 heißt bestanden. Eigene Prüfungen der App laufen immer: Pfade, Konfliktmarker, leere Dateien, neu defekte relative Links.'))),
      s.error && h('div', { class: 'note error-text', role: 'alert', id: 't-form-error', tabindex: '-1' }, s.error),
    ];
    queueMicrotask(paint);
    return {
      body,
      actions: [
        button('Abbrechen', close),
        button('Sichern', async () => {
          s.error = '';
          s.server = {};
          for (const f of TARGET_FIELDS) s.shown.add(f);
          const errors = check();
          const first = TARGET_FIELDS.find((f) => errors[f]);
          if (first) {
            render();
            focusField(first);
            return;
          }
          try {
            await invoke('save_target', { target: { ...t, paths: s.paths.split('\n').map((p) => p.trim()).filter(Boolean) } });
            close();
          } catch (error) {
            const message = String(error);
            const at = targetErrorField(message);
            if (at) s.server[at] = message; else s.error = message;
            render();
            if (at) focusField(at); else dialog.querySelector('#t-form-error')?.focus();
          }
        }, { id: 't-save', cls: 'primary' }),
      ],
    };
  });
}

// MARK: Job dialog

/** A care job for exactly these repositories: what to do, which targets, then start. */
function jobDialog(keys) {
  const repos = keys.map((k) => view.repositories.find((r) => r.key === k)).filter(Boolean);
  const allowed = repos.filter((r) => !r.capabilities.care && !r.job);
  const excluded = repos.filter((r) => r.capabilities.care || r.job);
  const withRule = allowed.filter((r) => r.release);
  const targets = view.targets.filter((t) => t.sources.some((k) => allowed.some((r) => r.key === k)));
  const feedsWebsite = !!view.websiteSources && allowed.some((r) => view.websiteSources.includes(r.key));
  const o = { prs: true, cleanup: false, release: false, targets: new Map(), website: false, model: '', instructions: '' };
  const s = { error: '', busy: false };
  openDialog('Pflegeauftrag', (close, render) => {
    const nothing = !o.prs && !o.cleanup && !o.release && !o.targets.size && !o.website;
    const option = (key, title, note, disabled) => h('label', { class: 'check-line top' },
      h('input', { type: 'checkbox', id: `job-opt-${key}`, checked: o[key], disabled, 'aria-describedby': `job-opt-${key}-note`, onchange: (e) => { o[key] = e.target.checked; render(); } }),
      h('span', null, h('strong', null, title), h('div', { class: 'note', id: `job-opt-${key}-note` }, note)));
    return {
      body: [
        h('h2', null, allowed.length === 1 ? `${allowed[0].repo} pflegen` : `${allowed.length} Repositories pflegen`),
        allowed.length > 1 && h('ul', { class: 'plain-list' }, allowed.map((r) => h('li', { class: 'mono' }, r.repo))),
        excluded.length > 0 && h('div', { class: 'note orange' }, `Ausgenommen: ${excluded.map((r) => `${r.repo} (${r.capabilities.care ?? 'gehört zu einem offenen Auftrag'})`).join(', ')}`),
        h('fieldset', null, h('legend', null, 'Repository-Pflege'),
          option('prs', 'Pull Requests prüfen und übernehmen', 'Claude bewertet jeden offenen PR auf den Default-Branch (Risikoklasse, Release-Notes, SHA-Pins, zusammengehörige Abhängigkeiten, Nutzung im Code); das kostet Claude-Nutzung. Gemergt wird nur, was GitHub mergebar mit grüner CI meldet und Claude grün oder gelb einstuft. Überflüssige PRs werden mit Begründung geschlossen, Dependabot-PRs mit Konflikt neu aufgebaut.'),
          option('cleanup', 'Gemergte Branches aufräumen', 'Branches, deren Commits auf GitHub vollständig im Default-Branch sind oder deren PR gemergt wurde, auf GitHub und in der lokalen Kopie. Geschützte, ausgecheckte und Branches offener PRs bleiben.'),
          option('release', 'Release: nächste Patch-Version taggen', withRule.length
            ? `Nur für Repositories mit Release-Regel (${withRule.map((r) => r.repo).join(', ')}), nur auf grüner CI genau des Ziel-Commits; danach wartet die App bei Bedarf auf das veröffentlichte GitHub-Release.`
            : 'Keins der Repositories hat eine Release-Regel; einrichten unter Repository → Verwaltung.', !withRule.length)),
        (targets.length > 0 || feedsWebsite) && h('fieldset', null, h('legend', null, 'Zugeordnete Inhalte'),
          targets.map((t) => h('div', { class: 'target-choice' },
            h('label', { class: 'check-line' },
              h('input', { type: 'checkbox', id: domId('job-target', t.id), checked: o.targets.has(t.id), disabled: !!t.problem || !!t.job, onchange: (e) => { e.target.checked ? o.targets.set(t.id, t.basis) : o.targets.delete(t.id); render(); } }),
              h('strong', null, t.name), h('span', { class: 'muted small' }, `${KIND_TEXT[t.kind]} · ${t.repo}`)),
            o.targets.has(t.id) && h('select', { class: 'basis-select', id: domId('job-basis', t.id), 'aria-label': `Basis für ${t.name}`, onchange: (e) => o.targets.set(t.id, e.target.value) },
              h('option', { value: 'release', selected: o.targets.get(t.id) === 'release' }, 'Basis: neuestes Release'),
              h('option', { value: 'commit', selected: o.targets.get(t.id) === 'commit' }, 'Basis: aktueller Stand')),
            (t.problem || t.job) && h('div', { class: 'note orange' }, t.problem ?? 'gehört zu einem offenen Auftrag'))),
          feedsWebsite && option('website', `Website ${view.site?.name ?? ''} aktualisieren und veröffentlichen`, 'Als letzter Schritt: die Website-Aufgaben für die Produkte dieser Repositories, Build, Commit; nach deiner Freigabe Push, SFTP-Upload und Prüfung der Live-Seite.', !view.site)),
        targets.length === 0 && h('div', { class: 'note' }, 'Für README oder Wiki zuerst ein Inhaltsziel anlegen (Repository → Zugeordnete Inhalte).'),
        (o.prs || o.targets.size > 0 || o.website) && row('Modell', modelSelect(o.model, view.config.claudeModel || view.site?.claude?.model, (model) => { o.model = model; }, 'job-model'), 'job-model'),
        (o.targets.size > 0 || o.website) && h('div', { class: 'field-col' }, h('label', { for: 'job-instructions' }, 'Zusätzliche Anweisung für Inhalte und Website (optional)'),
          h('textarea', { id: 'job-instructions', rows: 2, value: o.instructions, oninput: (e) => { o.instructions = e.target.value; } })),
        h('div', { class: 'note' }, 'Der Auftrag liest und bereitet vor, ohne nachzufragen. Vor jeder Änderung auf GitHub, im Wiki oder auf der Website legt er dir einen Vorschlag zur Freigabe vor; der Fortschritt steht unter Aufträge.'),
        s.error && h('div', { class: 'note error-text', role: 'alert' }, s.error),
      ],
      actions: [
        button('Abbrechen', close),
        button(s.busy ? 'Wird gestartet …' : 'Auftrag starten', async () => {
          s.busy = true; s.error = ''; render();
          try {
            const options = {
              prs: o.prs, cleanup: o.cleanup, release: o.release, website: o.website, model: o.model, instructions: o.instructions,
              targets: [...o.targets].map(([id, basis]) => ({ id, basis })),
            };
            const id = await invoke('start_job', { input: { repos: allowed.map((r) => r.key), options } });
            for (const r of allowed) selection.delete(r.key);
            close();
            jobTab = 'log';
            openJob(id);
          } catch (error) { s.error = String(error); s.busy = false; render(); }
        }, { cls: 'primary', iconName: 'play', disabled: s.busy || nothing || !allowed.length || !!view.running }),
      ],
    };
  });
}

// MARK: Settings

/** The cost limit as typed, as the app checks it: empty = none, else a positive amount with point or comma. */
function parseBudget(text) {
  const value = String(text ?? '').trim();
  if (!value) return { ok: true, value: null };
  const amount = /^\d+([.,]\d+)?$/.test(value) ? Number(value.replace(',', '.')) : NaN;
  return Number.isFinite(amount) && amount > 0 ? { ok: true, value: amount }
    : { ok: false, error: 'Bitte einen Betrag größer als 0 eingeben, z. B. 1,50, oder das Feld leeren.' };
}

function settingsDialog() {
  const c = view.config;
  const s = {
    projectDir: c.projectDir, workspaceDir: c.workspaceDir ?? '', siteRepo: view.siteRepo, cloneRepo: null, cloning: false,
    showPicker: false,
    host: c.sftp.host, port: c.sftp.port || 22, user: c.sftp.user, remoteDir: c.sftp.remoteDir,
    auth: c.sftp.auth, keyPath: c.sftp.keyPath, secret: '', message: null, busy: false,
    updateCheck: !c.noUpdateCheck, claudeModel: c.claudeModel ?? '', budget: c.maxBudgetUsd != null ? String(c.maxBudgetUsd).replace('.', ',') : '',
    // The connection test's answer, shown at the test; a server key it offered, and the one trusted here (saved with the rest).
    test: null, offeredKey: null, hostKey: '', hostKeyFor: '',
    open: { general: true, website: !!c.projectDir, app: false },
  };
  const target = () => `${s.host.trim()}:${Number(s.port) || 22}`;
  const input = () => ({
    projectDir: s.projectDir, workspaceDir: s.workspaceDir, host: s.host, port: Number(s.port) || 22, user: s.user, remoteDir: s.remoteDir,
    auth: s.auth, keyPath: s.keyPath, secret: s.secret, updateCheck: s.updateCheck,
    claudeModel: s.claudeModel, maxBudgetUsd: String(s.budget ?? ''),
    hostKey: s.hostKey && s.hostKeyFor === target() ? s.hostKey : '',
  });
  const text = (key, props = {}, after) => h('input', { type: 'text', value: s[key] ?? '', spellcheck: 'false', autocomplete: 'off', ...props,
    oninput: (e) => { s[key] = e.target.value; after?.(); } });
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
  // The budget's error is updated in place while typing; the field keeps focus and caret.
  const budgetError = h('div', { class: 'field-error', id: 's-budget-error', role: 'alert', hidden: true });
  const showBudget = (input) => {
    const parsed = parseBudget(s.budget);
    budgetError.textContent = parsed.ok ? '' : parsed.error;
    budgetError.hidden = parsed.ok;
    input?.setAttribute('aria-invalid', parsed.ok ? 'false' : 'true');
    return parsed.ok;
  };
  const section = (key, title, ...content) => h('details', { class: 'fold settings-group', open: s.open[key], ontoggle: (e) => { s.open[key] = e.target.open; } },
    h('summary', null, title), h('div', { class: 'fold-body' }, content));
  const testConnection = async () => {
    s.busy = true; s.test = { cls: 'note', text: 'Verbinde …' }; s.offeredKey = null; render();
    try {
      const result = await invoke('test_connection', { input: input() });
      if (result.kind === 'unknownHostKey') {
        s.offeredKey = result.fingerprint;
        s.test = { cls: 'orange', text: 'Der Server ist noch nicht bestätigt. Schlüssel prüfen, dann vertrauen und erneut testen.' };
      } else {
        s.test = { cls: 'ok-text', text: `${result.message}. Noch nicht gesichert: erst „Sichern“ übernimmt die Einstellungen.` };
      }
    } catch (error) {
      s.test = { cls: 'error-text', text: String(error) };
    }
    s.busy = false; render();
  };
  const dlg = openDialog('Einstellungen', (close, rerender) => {
    render = rerender;
    const password = s.auth === 'password';
    const sameProject = c.projectDir === s.projectDir;
    const budgetInput = text('budget', { id: 's-budget', placeholder: 'USD, leer = keine', inputmode: 'decimal', 'aria-describedby': 's-budget-note s-budget-error' }, () => showBudget(budgetInput));
    const savedKey = c.sftp.hostKey && `${c.sftp.host.trim()}:${c.sftp.port || 22}` === target() ? c.sftp.hostKey : '';
    const trusted = s.hostKey && s.hostKeyFor === target() ? s.hostKey : '';
    queueMicrotask(() => { if (s.budget) showBudget(budgetInput); });
    return {
      body: [
        h('h2', null, 'Einstellungen'),
        section('general', 'Allgemein',
          h('fieldset', null, h('legend', null, 'Zugänge'),
            row('GitHub', h('div', { class: view.signedIn === false ? 'orange' : 'muted' }, view.signedIn === false
              ? 'GitHub-CLI nicht angemeldet: im Terminal gh auth login'
              : view.signedIn ? 'über die GitHub-CLI (gh) angemeldet; die App nutzt ihren Token, Claude nie' : 'wird beim ersten Lesen geprüft (GitHub-CLI gh)')),
            row('Repo-Ordner', h('div', { class: 'line' }, text('workspaceDir', { id: 's-workspace', placeholder: `${view.workspaceDisplay} (automatisch)` }),
              button('Auswählen …', async () => { const p = await call('pick_folder'); if (p) { s.workspaceDir = p; repoCache = null; render(); } }, { id: 's-workspace-pick' }))),
            h('div', { class: 'note' }, 'Aus diesem Ordner bietet „Repositories auswählen“ die lokalen Klone an; neue Klone legt die App hier an.')),
          h('fieldset', null, h('legend', null, 'Ausführungsregeln für Aufträge'),
            row('Claude-Modell', modelSelect(s.claudeModel, view.site?.claude?.model, (model) => { s.claudeModel = model; }, 's-claude-model'), 's-claude-model'),
            h('div', { class: 'field-row' }, h('label', { for: 's-budget' }, 'Kostengrenze je Claude-Lauf'), h('div', { class: 'field-stack' }, budgetInput, budgetError)),
            h('div', { class: 'note', id: 's-budget-note' }, 'In USD, Komma oder Punkt. Gilt für PR-Prüfungen und Inhaltsarbeit, wenn Auftrag oder Ziel nichts anderes wählen. Schreibende Schritte laufen nie ohne Vorschau und deine Freigabe.'))),
        section('website', 'Website und Veröffentlichung (optional)',
          h('fieldset', null, h('legend', null, 'Website'),
            h('div', { class: 'note' }, 'Nur nötig, wenn die App auch eine Website aktualisieren und per SFTP veröffentlichen soll.'),
            row('Website-Repo', h('div', { class: 'line' },
              h('span', { class: `spacer ${s.siteRepo ? 'mono' : 'muted'}` }, s.siteRepo ?? 'nicht gewählt'),
              button(s.showPicker ? 'Liste schließen' : 'Aus Liste wählen …', () => { s.showPicker = !s.showPicker; render(); }, { id: 's-site-pick' }))),
            s.showPicker && picker,
            s.cloneRepo && h('div', { class: 'line' },
              h('span', { class: 'note spacer' }, `${s.cloneRepo} ist noch nicht lokal vorhanden.`),
              button(s.cloning ? 'Klone …' : `Nach ${s.workspaceDir || view.workspaceDisplay} klonen`, async () => {
                s.cloning = true; render();
                try {
                  s.projectDir = await invoke('clone_repo', { repo: s.cloneRepo, workspaceDir: s.workspaceDir });
                  s.cloneRepo = null; s.showPicker = false; repoCache = null;
                } catch (error) { s.message = String(error); }
                s.cloning = false; render();
              }, { id: 's-clone', cls: 'primary', disabled: s.cloning })),
            row('Projektordner', h('div', { class: 'line' }, text('projectDir', { id: 's-project', placeholder: 'leer = keine Website' }),
              button('Auswählen …', async () => { const p = await call('pick_folder'); if (p) { s.projectDir = p; render(); } }, { id: 's-project-pick' }),
              s.projectDir && button('Entfernen', () => { s.projectDir = ''; s.siteRepo = null; render(); }, { id: 's-project-remove', title: 'Website aus der App nehmen; im Projekt ändert sich nichts' }))),
            h('div', { class: `note ${view.siteError && sameProject ? 'error-text' : ''}` },
              !s.projectDir ? 'Keine Website eingerichtet.'
                : view.site && sameProject ? `${view.siteFile} gefunden: ${view.site.name}, Build „${view.site.build}“, Ausgabe ${view.site.output}`
                  : view.siteError && sameProject ? view.siteError
                    : 'Der Projektordner braucht eine Datei .openreleasepilot/site.json (siehe README); geprüft wird beim Sichern.')),
          s.projectDir && h('fieldset', null, h('legend', null, 'SFTP-Ziel der Website'),
            row('Server', text('host', { id: 's-host', placeholder: 'ssh.strato.de' })),
            row('Port', text('port', { id: 's-port', inputmode: 'numeric' })),
            row('Benutzer', text('user', { id: 's-user', placeholder: 'z. B. example.com' })),
            row('Zielordner', text('remoteDir', { id: 's-remote', placeholder: '/example.com' })),
            h('div', { class: 'note' }, 'Ordner auf dem Server, der dem Ausgabeordner entspricht. Gelöscht werden nur Dateien, die die App selbst hochgeladen hat.'),
            row('Anmeldung', h('select', { onchange: (e) => { s.auth = e.target.value; render(); } },
              h('option', { value: 'password', selected: password }, 'Passwort'),
              h('option', { value: 'key', selected: !password }, 'SSH-Schlüssel')), 's-auth'),
            !password && row('Schlüsseldatei', h('div', { class: 'line' }, text('keyPath', { id: 's-key', placeholder: '~/.ssh/id_ed25519' }),
              button('Auswählen …', async () => { const p = await call('pick_file'); if (p) { s.keyPath = p; render(); } }, { id: 's-key-pick' }))),
            row(password ? 'Passwort' : 'Passphrase', h('input', { type: 'password', value: s.secret, autocomplete: 'off',
              placeholder: c.sftp.secretSaved ? 'gespeichert – leer lassen zum Behalten' : (password ? '' : 'leer, wenn der Schlüssel keine hat'),
              oninput: (e) => { s.secret = e.target.value; } }), 's-secret'),
            h('div', { class: 'note' }, 'Das Passwort liegt nur im Schlüsselbund des Systems, nie in einer Datei oder im Repository.',
              c.sftp.secretSaved && [' ', button('Gespeichertes Passwort entfernen', async () => { await call('forget_secret'); close(); }, { id: 's-forget', cls: 'link' })]),
            row('Server-Schlüssel', h('div', { class: trusted || savedKey ? 'mono selectable' : 'muted' },
              trusted ? `${trusted} (hier bestätigt; wird mit „Sichern“ gespeichert)` : savedKey || 'wird beim ersten Verbindungstest bestätigt')),
            h('div', { class: 'line wrap' },
              button(s.busy ? 'Verbinde …' : 'Verbindung testen', testConnection, { id: 's-test', disabled: s.busy || s.cloning, title: 'Testet die Eingaben dieses Dialogs; gesichert wird erst mit „Sichern“' }),
              h('span', { class: 'note' }, 'Speichert nichts.')),
            h('div', { class: `note ${s.test?.cls ?? ''}`, role: 'status', 'aria-live': 'polite' }, s.test?.text ?? ''),
            s.offeredKey && h('div', { class: 'note-box warning' },
              h('strong', null, `Server-Schlüssel von ${target()}`),
              h('div', { class: 'fingerprint' }, s.offeredKey),
              h('div', { class: 'note' }, 'Vergleiche ihn mit der Angabe deines Hosters oder im Terminal mit:'),
              h('div', { class: 'fingerprint' }, `ssh-keyscan -p ${Number(s.port) || 22} ${s.host.trim()} | ssh-keygen -lf -`),
              h('div', { class: 'line' },
                button('Vertrauen und erneut testen', () => { s.hostKey = s.offeredKey; s.hostKeyFor = target(); s.offeredKey = null; testConnection(); }, { id: 's-trust', cls: 'primary' }),
                button('Nicht vertrauen', () => { s.offeredKey = null; s.test = null; render(); }, { id: 's-distrust' }))))),
        section('app', 'App',
          h('label', { class: 'check-line' },
            h('input', { type: 'checkbox', id: 's-update-check', checked: s.updateCheck, onchange: (e) => { s.updateCheck = e.target.checked; } }),
            'Beim Start nach neuen Versionen suchen'),
          h('div', { class: 'note' }, 'Fragt das neueste Release auf GitHub ab. Installiert wird nur auf Klick, nach Prüfung von SHA-256-Prüfsumme und Signatur.')),
        s.message && h('div', { class: 'note error-text', role: 'alert', id: 's-message', tabindex: '-1' }, s.message),
      ],
      actions: [
        button('Abbrechen', close),
        button('Sichern', async () => {
          if (!showBudget(document.getElementById('s-budget'))) {
            s.open.general = true; render();
            document.getElementById('s-budget')?.focus();
            return;
          }
          try {
            await invoke('save_settings', { input: input() });
            close();
          } catch (error) {
            s.message = String(error); render();
            document.getElementById('s-message')?.focus();
          }
        }, { id: 's-save', cls: 'primary', disabled: s.busy || s.cloning }),
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
  openDialog('Produkt zur Website hinzufügen', (close, rerender) => {
    render = rerender;
    const info = s.check?.info;
    return {
      body: [
        h('h2', null, 'Produkt zur Website hinzufügen'),
        h('p', { class: 'note', style: 'margin:0' }, 'Wähle das Repository des Produkts. Claude legt Produktseite (Deutsch und Englisch), Produktkarte, Navigation, Icon, Ablaufgrafik, llms.txt und den Eintrag in site.json an, nach dem Skill des Projekts. Danach baut die App und committet lokal; live geht es erst mit „Veröffentlichen“.'),
        picker,
        s.checking && h('div', { class: 'note' }, `${s.repo.fullName} wird geprüft …`),
        s.error && h('div', { class: 'note error-text', role: 'alert' }, s.error),
        info && h('fieldset', null, h('legend', null, info.fullName),
          h('div', { class: 'facts' },
            h('span', null, h('strong', null, 'Release: '), s.check.latestTag ?? 'noch keins'),
            h('span', null, h('strong', null, 'Lizenz: '), info.license ?? 'keine erkannt'),
            info.private && h('span', { class: 'orange' }, h('strong', null, 'privat: '), 'Besucher können GitHub-Links darauf nicht öffnen')),
          info.description && h('div', { class: 'muted' }, info.description),
          !s.check.latestTag && h('div', { class: 'note orange' }, 'Ohne Release gibt es kein Versions-Badge, und „Prüfen“ beobachtet das Produkt erst nach dem ersten Release.'),
          row('Name', h('input', { type: 'text', value: s.name, spellcheck: 'false', oninput: (e) => { s.name = e.target.value; } }), 'n-name'),
          row('Lokaler Klon', h('div', { class: 'line' },
            h('span', { class: `spacer ${s.localPath ? 'mono' : 'muted'}` }, s.localPath || 'nicht vorhanden – Claude liest dann nur über GitHub'),
            !s.localPath && button(s.cloning ? 'Klone …' : 'Klonen', async () => {
              s.cloning = true; render();
              try { s.localPath = await invoke('clone_repo', { repo: info.fullName, workspaceDir: '' }); repoCache = null; } catch (error) { s.error = String(error); }
              s.cloning = false; render();
            }, { id: 'n-clone', disabled: s.cloning }),
            button('Auswählen …', async () => { const p = await call('pick_folder'); if (p) { s.localPath = p; render(); } }, { id: 'n-local-pick' })), 'n-local'),
          row('Modell', modelSelect(s.model, view.site?.claude?.model, (model) => { s.model = model; }, 'n-model'), 'n-model'),
          h('div', { class: 'field-col' },
            h('label', { for: 'n-instructions' }, 'Anweisung an Claude (optional)'),
            h('textarea', { id: 'n-instructions', rows: 3, value: s.instructions,
              placeholder: 'z. B. Kategorie, Zielgruppe, Position in der Produktliste, welche Screenshots aus dem Repo',
              oninput: (e) => { s.instructions = e.target.value; } }))),
      ],
      actions: [
        button('Abbrechen', close),
        button('Produktseite erstellen', async () => {
          // A command without a return value resolves to null, so success is "no exception".
          try {
            await invoke('run_add_product', { input: { repo: info.fullName, name: s.name, localPath: s.localPath, instructions: s.instructions, model: s.model } });
            close();
          } catch (error) { s.error = String(error); render(); }
        }, { id: 'n-create', cls: 'primary', disabled: !info || s.checking || s.cloning || !s.name.trim() || !!view.running,
          title: 'Startet einen Claude-Lauf (kostet Claude-Nutzung), danach Build und lokaler Commit' }),
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
    const { step, job, line } = event.payload;
    if (job) {
      if (runsUi && job === jobId) appendTo(runsUi.log, [line]);
    } else if (step === selected) appendLog([line]);
  });
  await refresh();
  // Ages and "veraltet" move on without an event; a fresh view keeps them right. Reads nothing remote.
  setInterval(() => { if (area !== 'website' && document.visibilityState === 'visible') refresh(); }, 30000);
}

start();

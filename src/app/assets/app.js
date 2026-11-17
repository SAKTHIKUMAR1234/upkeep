'use strict';

/* ---------- Small helpers ---------- */

const IDLE_MS = 15 * 60 * 1000;
const OUTPUT_TAIL = 256 * 1024;
const MAX_TERMINAL_LINES = 2000;
const UTF8 = new TextEncoder();

const ICONS = {
  grid: '<rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/><rect x="14" y="14" width="7" height="7" rx="1"/>',
  clock: '<path d="M12 8v4l3 2"/><circle cx="12" cy="12" r="9"/>',
  list: '<path d="M4 6h16M4 12h16M4 18h10"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  play: '<path d="M7 5v14l11-7z" fill="currentColor" stroke="none"/>',
  stop: '<rect x="6" y="6" width="12" height="12" rx="1" fill="currentColor" stroke="none"/>',
  edit: '<path d="M4 20h4L19 9l-4-4L4 16v4z"/>',
  trash: '<path d="M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3"/>',
  monitor: '<rect x="3" y="4" width="18" height="12" rx="2"/><path d="M8 20h8M12 16v4"/>',
  layers: '<path d="M4 7h16M4 12h16M4 17h16"/>',
  plug: '<path d="M9 7V3M15 7V3M6 7h12v4a6 6 0 0 1-12 0V7zM12 17v4"/>',
  search: '<circle cx="11" cy="11" r="7"/><path d="M20 20l-4-4"/>',
  download: '<path d="M12 4v11M7 10l5 5 5-5M5 20h14"/>',
  copy: '<rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5 15V5a1 1 0 0 1 1-1h10"/>',
  down: '<path d="M12 5v14M6 13l6 6 6-6"/>',
  lock: '<rect x="5" y="11" width="14" height="10" rx="2"/><path d="M8 11V7a4 4 0 0 1 8 0v4"/>',
  refresh: '<path d="M20 11a8 8 0 1 0-2.3 5.7M20 4v7h-7"/>',
};

function icon(name) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('fill', 'none');
  svg.setAttribute('stroke', 'currentColor');
  svg.setAttribute('stroke-width', '2');
  svg.setAttribute('stroke-linecap', 'round');
  svg.setAttribute('stroke-linejoin', 'round');
  svg.setAttribute('aria-hidden', 'true');
  svg.classList.add('i');
  svg.innerHTML = ICONS[name]; // Static markup from ICONS only.
  return svg;
}

/** Builds an element. Strings become text nodes, so data is never parsed as HTML. */
function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs || {})) {
    if (value === undefined || value === null || value === false) continue;
    if (key === 'class') el.className = value;
    else if (key.startsWith('on')) el.addEventListener(key.slice(2), value);
    else if (key === 'text') el.textContent = value;
    else el.setAttribute(key, value === true ? '' : value);
  }
  append(el, children);
  return el;
}

function append(el, children) {
  for (const child of children.flat()) {
    if (child === undefined || child === null || child === false) continue;
    el.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return el;
}

function button(label, options = {}) {
  const classes = ['btn', options.kind, options.size].filter(Boolean).join(' ');
  return h('button', { type: options.type || 'button', class: classes, onclick: options.onClick, disabled: options.disabled, 'aria-pressed': options.pressed },
    options.icon && icon(options.icon), label);
}

const pad = (number) => String(number).padStart(2, '0');

/** "17-11 10:41"; older years add the year. */
function when(ms) {
  if (!ms) return '—';
  const date = new Date(ms);
  const year = date.getFullYear() === new Date().getFullYear() ? '' : `-${date.getFullYear()}`;
  return `${pad(date.getDate())}-${pad(date.getMonth() + 1)}${year} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

/** A clock for running time: "06:12" or "1:02:03". */
function clock(ms) {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  const hours = Math.floor(seconds / 3600);
  const rest = `${pad(Math.floor((seconds % 3600) / 60))}:${pad(seconds % 60)}`;
  return hours ? `${hours}:${rest}` : rest;
}

/** A finished duration: "0.4 s", "41 s", "27 min", "2 h 5 min". */
function took(ms) {
  if (ms < 10000) return `${(ms / 1000).toFixed(1)} s`;
  if (ms < 60000) return `${Math.round(ms / 1000)} s`;
  const minutes = Math.round(ms / 60000);
  return minutes < 60 ? `${minutes} min` : `${Math.floor(minutes / 60)} h ${minutes % 60} min`;
}

function runTook(run) {
  if (run.status === 'running') return clock(Date.now() - run.started_at);
  return run.finished_at ? took(run.finished_at - run.started_at) : '—';
}

/** "disk-cleanup" is "Disk cleanup", as the service names folders without a stored name. */
function titleOf(folder) {
  const words = folder.replace(/-/g, ' ').trim();
  return words.charAt(0).toUpperCase() + words.slice(1);
}

const shortUser = (email) => email.split('@')[0];

const RESULTS = {
  running: ['Running', 'b-run'],
  succeeded: ['Succeeded', 'b-ok'],
  failed: ['Failed', 'b-bad'],
  check_failed: ['Check failed', 'b-warn'],
  stopped: ['Stopped', 'b-gray'],
  interrupted: ['Interrupted', 'b-gray'],
};

function resultBadge(status, extra) {
  const [label, kind] = RESULTS[status] || [status, 'b-gray'];
  return h('span', { class: `badge ${kind}` }, status === 'running' ? '● ' : '', label, extra ? ` · ${extra}` : '');
}

const ACTIONS = {
  sign_in: 'Signed in',
  failed_sign_in: 'Failed sign-in',
  run: 'Ran operation',
  stop: 'Stopped run',
  server_add: 'Added server',
  server_edit: 'Edited server',
  server_delete: 'Deleted server',
  operation_create: 'Created operation',
  operation_edit: 'Edited operation',
  operation_delete: 'Deleted operation',
  script_edit: 'Edited script',
  user_add: 'Added user',
  user_password: 'Changed password',
  user_disable: 'Disabled user',
  user_enable: 'Enabled user',
};

function toast(message, bad) {
  const el = h('div', { class: bad ? 'toast bad' : 'toast', role: bad ? 'alert' : 'status' }, message);
  document.getElementById('toasts').append(el);
  setTimeout(() => el.remove(), bad ? 7000 : 4000);
}

/* ---------- API ---------- */

class ApiError extends Error {
  constructor(message, status, code) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

const session = { email: null, csrf: null };

function storedEmail() {
  try { return sessionStorage.getItem('upkeep-email'); } catch { return null; }
}

function storeEmail(email) {
  try {
    if (email) sessionStorage.setItem('upkeep-email', email);
    else sessionStorage.removeItem('upkeep-email');
  } catch { /* Storage is a convenience only. */ }
}

async function api(method, path, body) {
  const headers = { Accept: 'application/json' };
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  if (method !== 'GET' && session.csrf) headers['X-CSRF-Token'] = session.csrf;
  let response;
  try {
    response = await fetch(`/api${path}`, { method, headers, credentials: 'same-origin', body: body === undefined ? undefined : JSON.stringify(body) });
  } catch {
    throw new ApiError('The Upkeep service does not answer.', 0, 'offline');
  }
  if (response.status === 204) return null;
  const data = await response.json().catch(() => ({}));
  if (!response.ok) {
    const error = new ApiError(data.error || `Request failed (${response.status})`, response.status, data.code);
    if (response.status === 401 && path !== '/session') sessionEnded(error.code);
    throw error;
  }
  return data;
}

/* ---------- Sign-in, lock and idle ---------- */

let idleTimer = null;
let lastInput = Date.now();

function noteInput() {
  lastInput = Date.now();
}

function scheduleIdleLock() {
  clearTimeout(idleTimer);
  if (!session.csrf) return;
  idleTimer = setTimeout(() => {
    if (Date.now() - lastInput >= IDLE_MS) lock();
    else scheduleIdleLock();
  }, Math.max(1000, IDLE_MS - (Date.now() - lastInput)));
}

function sessionEnded(code) {
  const email = session.email || storedEmail();
  endSession();
  if (code === 'locked' && email) showLock(email);
  else showSignIn();
}

function endSession() {
  session.csrf = null;
  clearTimeout(idleTimer);
  closeDialogs();
  shell.stop();
}

async function lock() {
  const email = session.email;
  try { await api('DELETE', '/session'); } catch { /* The screen locks anyway. */ }
  endSession();
  showLock(email);
}

async function signOut() {
  try { await api('DELETE', '/session'); } catch { /* Signed out locally anyway. */ }
  storeEmail(null);
  session.email = null;
  endSession();
  showSignIn();
}

function showAuth(card) {
  document.getElementById('boot').hidden = true;
  document.getElementById('app').hidden = true;
  const auth = document.getElementById('auth');
  auth.replaceChildren(card);
  auth.hidden = false;
  const first = card.querySelector('input:not([readonly])');
  if (first) first.focus();
}

function authForm({ heading, hint, email, submitLabel, extra }) {
  const error = h('p', { class: 'form-error', role: 'alert' });
  const emailInput = h('input', { class: 'in', type: 'email', name: 'email', autocomplete: 'username', required: true, value: email || '', readonly: email ? true : undefined });
  const passwordInput = h('input', { class: 'in', type: 'password', name: 'password', autocomplete: 'current-password', required: true });
  const submit = button(submitLabel, { kind: 'pri', type: 'submit' });
  const form = h('form', { class: 'auth-card', novalidate: true },
    h('div', { class: 'auth-brand' }, h('i', { class: 'logo', 'aria-hidden': 'true' }), 'Upkeep'),
    h('div', {}, h('h1', {}, heading), h('p', { class: 'hint' }, hint)),
    email ? h('div', { class: 'who' }, h('span', { class: 'av' }, initials(email)), email) : h('label', { class: 'fl' }, 'Email', emailInput),
    email && h('input', { type: 'hidden', name: 'email', value: email }),
    h('label', { class: 'fl' }, 'Password', passwordInput),
    error, submit, extra);
  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    error.textContent = '';
    submit.disabled = true;
    try {
      const me = await api('POST', '/session', { email: email || emailInput.value, password: passwordInput.value });
      startSession(me);
    } catch (failure) {
      error.textContent = failure.message;
      passwordInput.value = '';
      passwordInput.focus();
    } finally {
      submit.disabled = false;
    }
  });
  return form;
}

function showSignIn() {
  showAuth(authForm({
    heading: 'Sign in',
    hint: 'Use the email and password from your Upkeep admin.',
    submitLabel: 'Sign in',
    extra: h('p', { class: 'hint auth-foot' }, 'Forgot your password? An admin resets it with ', h('code', {}, 'upkeep user passwd'), '.'),
  }));
}

function showLock(email) {
  showAuth(authForm({
    heading: 'Locked',
    hint: 'Upkeep is locked. It also locks after 15 minutes without use. Enter your password to go on.',
    email,
    submitLabel: 'Unlock',
    extra: h('p', { class: 'hint auth-foot' }, h('button', { type: 'button', class: 'link-plain', onclick: () => { storeEmail(null); showSignIn(); } }, 'Sign in as someone else')),
  }));
}

function initials(email) {
  const name = shortUser(email).replace(/[^a-zA-Z]/g, '');
  return (name.slice(0, 2) || '?').toUpperCase();
}

function startSession(me) {
  session.email = me.email;
  session.csrf = me.csrf;
  storeEmail(me.email);
  lastInput = Date.now();
  scheduleIdleLock();
  document.getElementById('boot').hidden = true;
  document.getElementById('auth').hidden = true;
  document.getElementById('app').hidden = false;
  document.getElementById('me-email').textContent = me.email;
  document.getElementById('me-initials').textContent = initials(me.email);
  shell.start();
}

/* ---------- Dialogs ---------- */

const openDialogs = [];

function closeDialogs() {
  while (openDialogs.length) openDialogs.pop().close();
}

/**
 * A modal with a form. Enter submits, Esc closes. `onSubmit` returns false to keep it open.
 */
function dialog({ title, hint, body, submitLabel, submitKind = 'pri', extraButtons = [], leftButtons = [], onSubmit, wide }) {
  const previous = document.activeElement;
  const titleId = `dialog-${Math.random().toString(36).slice(2)}`;
  const error = h('p', { class: 'form-error', role: 'alert' });
  const submit = submitLabel ? button(submitLabel, { kind: submitKind === 'danger' ? 'dng solid' : submitKind, type: 'submit' }) : null;
  const form = h('form', { class: wide ? 'modal wide' : 'modal', role: 'dialog', 'aria-modal': 'true', 'aria-labelledby': titleId, novalidate: true },
    h('div', { class: 'mh' }, h('h3', { id: titleId }, title), hint && h('p', { class: 'hint' }, hint)),
    h('div', { class: 'mb' }, body, error),
    h('div', { class: 'mf' }, leftButtons.map((left) => { left.classList.add('left'); return left; }), button('Cancel', { onClick: () => close() }), extraButtons, submit));
  const backdrop = h('div', { class: 'modal-bg' }, form);
  const handle = {
    close,
    error(message) { error.textContent = message || ''; },
    busy(state) { form.querySelectorAll('button').forEach((el) => { el.disabled = state; }); },
  };
  function close() {
    backdrop.remove();
    const index = openDialogs.indexOf(handle);
    if (index >= 0) openDialogs.splice(index, 1);
    if (previous && previous.focus) previous.focus();
  }
  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    if (!onSubmit) return close();
    handle.error('');
    handle.busy(true);
    try {
      if ((await onSubmit(handle)) !== false) close();
    } catch (failure) {
      handle.error(failure.message);
    } finally {
      handle.busy(false);
    }
  });
  form.addEventListener('keydown', (event) => {
    if (event.key === 'Escape') { event.preventDefault(); close(); }
    if (event.key === 'Enter' && (event.ctrlKey || event.metaKey)) { event.preventDefault(); form.requestSubmit(); }
  });
  backdrop.addEventListener('mousedown', (event) => { if (event.target === backdrop) close(); });
  document.getElementById('dialogs').append(backdrop);
  openDialogs.push(handle);
  const first = form.querySelector('input:not([type=hidden]):not([readonly]), textarea, select') || submit;
  if (first) first.focus();
  return handle;
}

function confirmDialog({ title, message, confirmLabel, onConfirm }) {
  return dialog({ title, body: h('p', {}, message), submitLabel: confirmLabel, submitKind: 'danger', onSubmit: onConfirm });
}

function field(label, input, hint) {
  return h('label', { class: 'fl' }, label, input, hint && h('span', { class: 'hint' }, hint));
}

function input(attrs) {
  return h('input', { class: 'in', ...attrs });
}

/* ---------- Server dialogs ---------- */

async function serverDialog(server) {
  const name = input({ name: 'name', required: true, value: server ? server.name : '', placeholder: 'YRP server (.22)', maxlength: 80 });
  const address = input({ name: 'address', class: 'in mono', required: true, value: server ? server.address : '', placeholder: '100.64.0.22', autocomplete: 'off', spellcheck: 'false' });
  const user = input({ name: 'ssh_user', class: 'in mono', required: true, value: server ? server.ssh_user : 'ubuntu', autocomplete: 'off', spellcheck: 'false' });
  const port = input({ name: 'port', class: 'in mono', type: 'number', min: 1, max: 65535, required: true, value: server ? server.port : 22 });
  const keys = h('datalist', { id: 'ssh-keys' });
  const key = input({ name: 'key_path', class: 'in mono', required: true, value: server ? server.key_path : '', placeholder: '~/.ssh/id_ed25519', list: 'ssh-keys', autocomplete: 'off', spellcheck: 'false' });
  const result = h('p', { class: 'check', role: 'status' });
  api('GET', '/ssh-keys').then((found) => keys.replaceChildren(...found.keys.map((path) => h('option', { value: path })))).catch(() => {});

  const values = () => ({ name: name.value, address: address.value, port: Number(port.value), ssh_user: user.value, key_path: key.value });
  const test = button('Test connection', { icon: 'plug' });
  test.addEventListener('click', async () => {
    result.className = 'check wait';
    result.replaceChildren(h('span', { class: 'spinner', 'aria-hidden': 'true' }), 'Connecting over SSH…');
    test.disabled = true;
    try {
      const answer = await api('POST', '/servers/test', values());
      result.className = answer.ok ? 'check ok' : 'check bad';
      result.textContent = `${answer.ok ? '✓' : '✗'} Test connection: ${answer.message}`;
    } catch (failure) {
      result.className = 'check bad';
      result.textContent = `✗ ${failure.message}`;
    } finally {
      test.disabled = false;
    }
  });

  const remove = server && button('Delete server', { kind: 'dng', icon: 'trash', onClick: () => {
    confirmDialog({
      title: `Delete ${server.name}?`,
      message: `Upkeep forgets this server and stops reaching it. Its history stays. Nothing changes on the server itself.`,
      confirmLabel: 'Delete server',
      onConfirm: async () => {
        await api('DELETE', `/servers/${server.id}`);
        closeDialogs();
        toast(`Deleted ${server.name}`);
        await shell.refreshServers();
        location.hash = '#/';
      },
    });
  } });

  dialog({
    title: server ? 'Edit server' : 'Add server',
    hint: 'Upkeep reaches the server only over SSH.',
    body: [
      field('Name', name),
      field('Address', address),
      h('div', { class: 'row' }, h('div', { class: 'sp' }, field('SSH user', user)), h('div', { class: 'port' }, field('Port', port))),
      h('div', { class: 'fl' }, 'Sign-in method', h('span', { class: 'seg' }, h('span', { class: 'on' }, 'SSH key'))),
      field('Private key', key, "Only the file's location is saved. The key stays in its file. It must be readable only by you (chmod 600)."),
      keys,
      result,
    ],
    extraButtons: [test],
    leftButtons: remove ? [remove] : [],
    submitLabel: server ? 'Save server' : 'Add server',
    onSubmit: async () => {
      const saved = server ? await api('PUT', `/servers/${server.id}`, values()) : await api('POST', '/servers', values());
      toast(server ? `Saved ${saved.name}` : `Added ${saved.name}`);
      await shell.refreshServers();
      if (server) shell.render();
      else location.hash = `#/servers/${saved.id}`;
    },
  });
}

/* ---------- Operation dialogs ---------- */

const RUN_TEMPLATE = '#!/bin/bash\nset -euo pipefail\necho "::step First step"\n';
const CHECK_PLACEHOLDER = '#!/bin/bash\n# Optional. Exit 0 when the server is healthy.';

function slug(text) {
  return text.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, 64);
}

function scriptArea(name, value, placeholder, rows) {
  return h('textarea', { class: 'in', name, rows, spellcheck: 'false', placeholder, text: value });
}

function newOperationDialog(server, onDone) {
  const name = input({ name: 'title', required: true, placeholder: 'Disk cleanup', maxlength: 80 });
  const folder = input({ name: 'folder', class: 'in mono', required: true, placeholder: 'disk-cleanup', pattern: '[a-z0-9-]+', maxlength: 64, spellcheck: 'false' });
  let folderEdited = false;
  name.addEventListener('input', () => { if (!folderEdited) folder.value = slug(name.value); });
  folder.addEventListener('input', () => { folderEdited = true; });
  const runSh = scriptArea('run_sh', RUN_TEMPLATE, '', 9);
  const checkSh = scriptArea('check_sh', '', CHECK_PLACEHOLDER, 4);
  dialog({
    title: `New operation on ${server.name}`,
    hint: 'Creates /.upkeep/<folder> on the server.',
    wide: true,
    body: [
      field('Name', name),
      field('Folder', h('span', { class: 'prefixed' }, h('span', { class: 'pre' }, '/.upkeep/'), folder), 'Lowercase letters, digits and dashes.'),
      field('run.sh', runSh, 'Print "::step <title>" to start a step. Exit 0 for OK, 10 to restart the server, anything else for failed.'),
      h('label', { class: 'fl' }, h('span', {}, 'check.sh ', h('span', { class: 'hint' }, '(optional)')), checkSh),
    ],
    submitLabel: 'Create operation',
    onSubmit: async () => {
      await api('POST', `/servers/${server.id}/operations`, { title: name.value, folder: folder.value, run_sh: runSh.value, check_sh: checkSh.value });
      toast(`Created /.upkeep/${folder.value}`);
      onDone();
    },
  });
}

async function editOperationDialog(server, operation, onDone) {
  const body = h('div', { class: 'mb-body' }, h('p', { class: 'check wait' }, h('span', { class: 'spinner', 'aria-hidden': 'true' }), 'Reading the scripts from the server…'));
  const remove = button('Delete operation', { kind: 'dng', icon: 'trash', onClick: () => {
    confirmDialog({
      title: `Delete /.upkeep/${operation.folder}?`,
      message: `This removes the folder /.upkeep/${operation.folder} and its scripts from ${server.name}. The run history stays.`,
      confirmLabel: 'Delete folder',
      onConfirm: async () => {
        await api('DELETE', `/servers/${server.id}/operations/${operation.folder}`);
        closeDialogs();
        toast(`Deleted /.upkeep/${operation.folder}`);
        onDone();
      },
    });
  } });
  let scripts = null;
  const handle = dialog({
    title: `Edit ${operation.title}`,
    hint: `/.upkeep/${operation.folder} on ${server.name}`,
    wide: true,
    body,
    leftButtons: [remove],
    submitLabel: 'Save scripts',
    onSubmit: async () => {
      if (!scripts) return false;
      const result = await api('PUT', `/servers/${server.id}/operations/${operation.folder}`, { title: scripts.title.value, run_sh: scripts.run.value, check_sh: scripts.check.value });
      toast(result.changes.length ? `Saved ${result.changes.map((change) => `${change.file} (${change.summary})`).join(', ')}` : 'No changes');
      onDone();
    },
  });
  try {
    const current = await api('GET', `/servers/${server.id}/operations/${operation.folder}`);
    scripts = {
      title: input({ name: 'title', required: true, value: current.title, maxlength: 80 }),
      run: scriptArea('run_sh', current.run_sh, '', 12),
      check: scriptArea('check_sh', current.check_sh, CHECK_PLACEHOLDER, 5),
    };
    body.replaceChildren(
      field('Name', scripts.title),
      field('run.sh', scripts.run),
      h('label', { class: 'fl' }, h('span', {}, 'check.sh ', h('span', { class: 'hint' }, '(optional; empty removes the file)')), scripts.check),
    );
    scripts.run.focus();
  } catch (failure) {
    handle.error(failure.message);
    body.replaceChildren();
  }
}

/* ---------- Virtual list ---------- */

/**
 * A table that draws only the rows in view and loads the next page near the end.
 * `load(cursor)` returns `{items, next}`.
 */
function virtualTable({ columns, template, cells, load, onOpen, emptyText, rowKey }) {
  const rowHeight = parseInt(getComputedStyle(document.documentElement).getPropertyValue('--row'), 10) || 41;
  const head = h('div', { class: 'vt-head', role: 'row' }, columns.map((name) => h('div', { role: 'columnheader' }, name)));
  const spacer = h('div', { class: 'vt-spacer' });
  const rows = h('div', { role: 'rowgroup' });
  const state = h('div', { class: 'vt-state', role: 'status' });
  const body = h('div', { class: 'vt-body' }, spacer, rows, state);
  const foot = h('span', {});
  const el = h('div', { class: 'vt', role: 'table', 'aria-rowcount': '-1' }, head, body);
  head.style.gridTemplateColumns = template;

  let items = [];
  let next = null;
  let loading = false;
  let generation = 0;
  let frame = 0;

  function draw() {
    frame = 0;
    spacer.style.height = `${items.length * rowHeight}px`;
    const first = Math.max(0, Math.floor(body.scrollTop / rowHeight) - 6);
    const last = Math.min(items.length, Math.ceil((body.scrollTop + body.clientHeight) / rowHeight) + 6);
    const drawn = [];
    for (let index = first; index < last; index += 1) {
      const item = items[index];
      const row = h('div', { class: onOpen ? 'vt-row link' : 'vt-row', role: 'row', tabindex: onOpen ? '0' : undefined, 'aria-rowindex': index + 1 },
        cells(item).map((cell) => h('div', { role: 'cell' }, cell)));
      row.style.gridTemplateColumns = template;
      row.style.transform = `translateY(${index * rowHeight}px)`;
      if (onOpen) {
        row.addEventListener('click', () => onOpen(item));
        row.addEventListener('keydown', (event) => { if (event.key === 'Enter') onOpen(item); });
      }
      drawn.push(row);
    }
    rows.replaceChildren(...drawn);
    state.style.top = `${items.length * rowHeight}px`;
    foot.textContent = items.length ? `Showing 1–${items.length}${next ? ' · loads the next 50 as you scroll' : ' · all rows'}` : '';
    if (next && !loading && last >= items.length - 10) more();
  }

  function schedule() {
    if (!frame) frame = requestAnimationFrame(draw);
  }

  async function more() {
    loading = true;
    const mine = generation;
    state.replaceChildren(h('span', { class: 'spinner', 'aria-hidden': 'true' }), ' Loading…');
    state.className = 'vt-state row-center';
    try {
      const page = await load(next === null ? '' : next);
      if (mine !== generation) return;
      items = items.concat(page.items);
      next = page.next;
      state.replaceChildren(items.length ? '' : emptyText);
    } catch (failure) {
      if (mine !== generation) return;
      next = next === null ? '' : next;
      state.replaceChildren(failure.message, ' ', button('Try again', { size: 'sm', onClick: () => { state.replaceChildren(); more(); } }));
      loading = false;
      return;
    }
    loading = false;
    schedule();
  }

  body.addEventListener('scroll', schedule, { passive: true });
  window.addEventListener('resize', schedule);

  return {
    el,
    foot,
    reset() {
      generation += 1;
      items = [];
      next = null;
      loading = false;
      body.scrollTop = 0;
      rows.replaceChildren();
      more();
    },
    /** Replaces a row with a newer copy, or adds it on top when `addNew` allows. */
    upsert(item, addNew) {
      const index = items.findIndex((existing) => rowKey(existing) === rowKey(item));
      if (index >= 0) items[index] = item;
      else if (addNew) items.unshift(item);
      else return;
      if (items.length) state.replaceChildren();
      schedule();
    },
    destroy() { window.removeEventListener('resize', schedule); },
  };
}

function select(label, options, value, onChange) {
  const el = h('select', { class: 'sel', 'aria-label': label },
    options.map(([optionValue, text]) => h('option', { value: optionValue, selected: optionValue === value ? true : undefined }, text)));
  el.addEventListener('change', () => onChange(el.value));
  return el;
}

function searchBox(placeholder, onSearch) {
  const field = h('input', { type: 'search', placeholder, 'aria-label': placeholder });
  let timer = null;
  field.addEventListener('input', () => { clearTimeout(timer); timer = setTimeout(() => onSearch(field.value.trim()), 300); });
  return h('label', { class: 'search' }, icon('search'), field);
}

function query(params) {
  const text = new URLSearchParams(Object.entries(params).filter(([, value]) => value !== '' && value !== null && value !== undefined)).toString();
  return text ? `?${text}` : '';
}

/* ---------- Views ---------- */

function topBar(crumb, title, actions) {
  return h('div', { class: 'top' }, h('div', {}, crumb && h('div', { class: 'crumb' }, crumb), h('h1', { class: 'ttl' }, title)), h('div', { class: 'row' }, actions));
}

function loadingPanel(text) {
  return h('div', { class: 'panel' }, h('div', { class: 'empty', role: 'status' }, h('span', { class: 'spinner', 'aria-hidden': 'true' }), text));
}

function errorPanel(message, retry) {
  return h('div', { class: 'banner bad', role: 'alert' }, message, h('span', { class: 'sp' }), retry && button('Try again', { size: 'sm', icon: 'refresh', onClick: retry }));
}

function statCard(label, value, small) {
  return h('div', { class: 'stat' }, h('span', { class: 'muted' }, label), h('b', { class: small ? 'small' : undefined }, value));
}

function pip(server) {
  if (server.running_run) return h('i', { class: 'pip r', title: 'Running', 'aria-label': 'running' });
  if (server.reachable === true) return h('i', { class: 'pip g', title: 'Reachable over SSH', 'aria-label': 'reachable' });
  if (server.reachable === false) return h('i', { class: 'pip w', title: 'Last SSH contact failed', 'aria-label': 'not reachable' });
  return h('i', { class: 'pip', title: 'Not contacted yet', 'aria-label': 'not contacted yet' });
}

function overviewView() {
  const content = h('div', { class: 'content' }, loadingPanel('Loading the overview…'));
  const el = [topBar(null, 'Overview', button('Add server', { kind: 'pri', icon: 'plus', onClick: () => serverDialog() })), content];

  async function render() {
    try {
      const [figures, recent] = await Promise.all([api('GET', '/overview'), api('GET', '/runs')]);
      const servers = shell.servers;
      if (!servers.length) {
        content.replaceChildren(h('div', { class: 'panel' }, h('div', { class: 'empty' },
          h('b', {}, 'No servers'), 'Add a server to run its operations over SSH.',
          button('Add server', { kind: 'pri', icon: 'plus', onClick: () => serverDialog() }))));
        return;
      }
      const lastRuns = new Map();
      for (const run of recent.items) if (!lastRuns.has(run.server_id)) lastRuns.set(run.server_id, run);
      content.replaceChildren(
        h('div', { class: 'stats' },
          statCard('Servers', figures.servers),
          statCard('Running now', figures.running),
          statCard('Runs (30 days)', figures.runs_30d),
          statCard('Failed (30 days)', figures.failed_30d)),
        h('div', { class: 'cols2' },
          h('div', { class: 'panel' }, h('div', { class: 'ph' }, h('h3', {}, 'Servers'), h('span', { class: 'ph-note' }, 'Reached over SSH')),
            servers.map((server) => {
              const last = lastRuns.get(server.id);
              return h('a', { class: 'srv-row', href: `#/servers/${server.id}` }, pip(server),
                h('div', {}, h('b', {}, server.name), h('div', { class: 'hint mono' }, `${server.ssh_user}@${server.address}`)),
                last ? resultBadge(last.status) : h('span', { class: 'hint' }, 'No runs'),
                h('span', { class: 'hint' }, last ? when(last.started_at) : ''));
            })),
          h('div', { class: 'panel' }, h('div', { class: 'ph' }, h('h3', {}, 'Recent runs'), h('a', { class: 'btn sm', href: '#/history' }, 'Open history')),
            recent.items.length ? h('table', { class: 'simple' },
              h('thead', {}, h('tr', {}, ['Run', 'Server', 'Operation', 'Result', 'Started'].map((name) => h('th', {}, name)))),
              h('tbody', {}, recent.items.slice(0, 8).map((run) => h('tr', { tabindex: '0', onclick: () => { location.hash = `#/runs/${run.id}`; }, onkeydown: (event) => { if (event.key === 'Enter') location.hash = `#/runs/${run.id}`; } },
                h('td', {}, `#${run.id}`), h('td', {}, run.server_name), h('td', {}, run.title), h('td', {}, resultBadge(run.status)), h('td', {}, when(run.started_at))))))
              : h('div', { class: 'empty' }, 'No runs. Open a server and run an operation.'))));
    } catch (failure) {
      content.replaceChildren(errorPanel(failure.message, render));
    }
  }
  render();
  return { el, onRunChange: render, onServersChange: render };
}

function serverView(id) {
  const server = shell.servers.find((candidate) => candidate.id === id);
  if (!server) {
    return { el: [topBar('Servers', 'Server not found'), h('div', { class: 'content' }, errorPanel('This server does not exist. It may have been deleted.'))] };
  }
  const badge = h('span', { class: 'badge b-gray', role: 'status' }, 'Checking SSH…');
  const actions = [badge,
    button('Edit server', { icon: 'edit', onClick: () => serverDialog(server) }),
    button('New operation', { kind: 'pri', icon: 'plus', onClick: () => newOperationDialog(server, loadOperations) })];
  const stats = h('div', { class: 'stats' });
  const banner = h('div', {});
  const list = h('div', { class: 'ops' }, [0, 1].map(() => h('div', { class: 'op' }, h('div', { class: 'ic' }), h('div', {}, h('div', { class: 'skeleton' })), h('span', {}), h('span', {}))));
  const el = [topBar('Servers', server.name, actions), h('div', { class: 'content' }, stats, banner,
    h('div', { class: 'panel' }, h('div', { class: 'ph' }, h('h3', {}, 'Operations'), h('span', { class: 'ph-note' }, 'Scripts in /.upkeep on this server')), list))];

  async function loadStats() {
    try {
      const page = await api('GET', `/servers/${id}`);
      const last = page.last_run;
      const lastText = last ? `${last.title} ${last.status === 'succeeded' ? '✓' : last.status === 'running' ? '…' : '✗'}` : 'None';
      const address = page.server.port === 22 ? page.server.address : `${page.server.address}:${page.server.port}`;
      stats.replaceChildren(statCard('Address', address, true), statCard('Operations', page.operations), statCard('Last run', lastText, true), statCard('Runs (30 days)', page.runs_30d));
    } catch (failure) {
      stats.replaceChildren(errorPanel(failure.message, loadStats));
    }
  }

  async function loadOperations() {
    try {
      const answer = await api('GET', `/servers/${id}/operations`);
      badge.className = answer.connected ? 'badge b-ok' : 'badge b-bad';
      badge.textContent = answer.connected ? '● Connected over SSH' : '● Cannot connect';
      banner.replaceChildren(answer.connected ? '' : errorPanel(`Cannot reach the server: ${answer.error}. The list shows the operations Upkeep saw last.`, loadOperations));
      shell.refreshServers();
      drawOperations(answer.operations);
    } catch (failure) {
      badge.className = 'badge b-bad';
      badge.textContent = '● Error';
      banner.replaceChildren(errorPanel(failure.message, loadOperations));
      list.replaceChildren();
    }
    loadStats(); // After the list, so the count matches what the server has.
  }

  function drawOperations(operations) {
    if (!operations.length) {
      list.replaceChildren(h('div', { class: 'empty' }, h('b', {}, 'No operations on this server'),
        'An operation is a folder in /.upkeep with a run.sh and an optional check.sh.',
        button('New operation', { kind: 'pri', icon: 'plus', onClick: () => newOperationDialog(server, loadOperations) })));
      return;
    }
    const running = shell.servers.find((candidate) => candidate.id === id)?.running_run;
    list.replaceChildren(...operations.map((operation) => {
      const last = operation.last_run;
      const isRunning = last && last.status === 'running';
      const run = button(isRunning ? 'Open run' : 'Run', { kind: 'pri', size: 'sm', icon: isRunning ? 'down' : 'play', onClick: () => (isRunning ? openRun(last.id) : startRun(server, operation)) });
      if (running && !isRunning) {
        run.disabled = true;
        run.title = 'Another run is active on this server';
      }
      return h('div', { class: 'op' },
        h('div', { class: 'ic' }, icon(operation.folder.includes('update') ? 'monitor' : 'layers')),
        h('div', { class: 'op-main' }, h('b', {}, operation.title), h('div', { class: 'path' }, `/.upkeep/${operation.folder} · ${operation.has_check ? 'run.sh + check.sh' : 'run.sh'}`)),
        h('div', { class: 'last' }, last ? [resultBadge(last.status), h('br'), `${when(last.started_at)} · ${runTook(last)}`] : 'Never run'),
        h('div', { class: 'acts' }, button('Edit', { size: 'sm', icon: 'edit', onClick: () => editOperationDialog(server, operation, loadOperations) }), run));
    }));
  }

  loadOperations();
  return {
    el,
    onRunChange(run) { if (run.server_id === id) loadOperations(); },
  };
}

function openRun(id) {
  location.hash = `#/runs/${id}`;
}

async function startRun(server, operation) {
  try {
    const run = await api('POST', `/servers/${server.id}/operations/${operation.folder}/runs`);
    openRun(run.id);
  } catch (failure) {
    toast(failure.message, true);
  }
}

function terminalLine(text) {
  const line = h('div', { class: 'ln' });
  if (text.startsWith('::step ')) append(line, [h('span', { class: 'c' }, '::step'), text.slice(6)]);
  else if (text.startsWith('$ ')) append(line, [h('span', { class: 'd' }, text)]);
  else if (text.startsWith('upkeep: ')) append(line, [h('span', { class: 'y' }, text)]);
  else line.textContent = text;
  return line;
}

const STEP_MARKS = { ok: '✓', failed: '✗', stopped: '■' };

/** The size of `lines` in the output file, with their newlines. */
function byteLength(lines) {
  return lines.reduce((total, line) => total + UTF8.encode(line).length + 1, 0);
}

/** The last `limit` lines of an output chunk and the byte where they start. With `cut`, the
 * chunk may start inside a line, so its first line is dropped. */
function chunkLines(chunk, limit, cut) {
  const lines = chunk.text.split('\n');
  if (lines[lines.length - 1] === '') lines.pop();
  const skip = Math.max(cut && lines.length > 1 ? 1 : 0, lines.length - limit);
  return { start: chunk.start + byteLength(lines.slice(0, skip)), lines: lines.slice(skip) };
}

function runView(id) {
  const status = h('span', { class: 'badge b-gray', role: 'status' }, 'Loading…');
  const stop = button('Stop', { kind: 'dng', icon: 'stop' });
  stop.hidden = true;
  const crumb = h('span', {}, 'Run');
  const by = h('span', { class: 'ph-note' });
  const steps = h('ol', { class: 'steps', 'aria-label': 'Steps' });
  const detail = h('p', { class: 'hint run-detail' });
  const shownCount = h('span');
  const earlier = h('div', { class: 'earlier', hidden: true }, shownCount, ' · ',
    h('button', { type: 'button', onclick: () => loadEarlier() }, '↑ Load earlier'), ' · ',
    h('button', { type: 'button', onclick: () => openFullLog() }, 'Open full log'));
  const lines = h('div');
  const live = h('div');
  const cursor = h('div', { class: 'cursor-row', 'aria-hidden': 'true' }, h('span', { class: 'cursor' }, '▌'));
  const terminal = h('div', { class: 'term', role: 'log', 'aria-label': 'Output', tabindex: '0' }, earlier, lines, live);
  const follow = button('Follow ↓', { size: 'sm', pressed: 'true' });
  const copy = button('Copy', { size: 'sm', icon: 'copy' });
  const el = [topBar(crumb, `Run #${id}`, [status, stop]), h('div', { class: 'content' }, h('div', { class: 'split' },
    h('div', { class: 'panel' }, h('div', { class: 'ph' }, h('h3', {}, 'Progress'), by), steps, detail,
      h('p', { class: 'hint steps-hint' }, 'Steps come from lines the script prints, such as ', h('code', {}, '::step Taking snapshot'), '. A script without them shows as one step.')),
    h('div', { class: 'panel' }, h('div', { class: 'ph' }, h('h3', {}, 'Output'), h('div', { class: 'row' }, copy, follow)), terminal)))];

  let run = null;
  let have = 0; // The output byte the view has, without the live rows.
  let start = 0; // The output byte of the first shown line.
  let shown = 0;
  let source = null;
  let ticker = null;
  let following = true;
  let queuedText = '';
  let queuedLive = null;
  let frame = 0;

  follow.addEventListener('click', () => {
    following = !following;
    follow.setAttribute('aria-pressed', String(following));
    if (following) terminal.scrollTop = terminal.scrollHeight;
  });
  terminal.addEventListener('scroll', () => {
    const atEnd = terminal.scrollHeight - terminal.scrollTop - terminal.clientHeight < 30;
    if (following !== atEnd) { following = atEnd; follow.setAttribute('aria-pressed', String(atEnd)); }
  }, { passive: true });
  copy.addEventListener('click', async () => {
    const text = [...terminal.querySelectorAll('.ln')].map((line) => line.textContent).join('\n');
    try { await navigator.clipboard.writeText(text); toast('Output copied'); } catch { toast('Cannot copy here. Select the text instead.', true); }
  });
  stop.addEventListener('click', () => confirmDialog({
    title: `Stop run #${id}?`,
    message: 'Upkeep closes the SSH session, which ends the script on the server at once. The run is marked stopped.',
    confirmLabel: 'Stop run',
    onConfirm: async () => { await api('POST', `/runs/${id}/stop`); },
  }));

  /** Queues output for the next frame, so a busy run draws at most once a frame. */
  function queue(text, liveRows) {
    queuedText += text;
    if (liveRows) queuedLive = liveRows;
    if (!frame) frame = requestAnimationFrame(draw);
  }

  function draw() {
    frame = 0;
    const added = queuedText.split('\n');
    if (added[added.length - 1] === '') added.pop();
    queuedText = '';
    if (added.length) {
      const fragment = document.createDocumentFragment();
      for (const line of added) fragment.append(terminalLine(line));
      lines.append(fragment);
      shown += added.length;
      if (following) trim();
      drawEarlier();
    }
    if (queuedLive) {
      live.replaceChildren(...queuedLive.map(terminalLine));
      queuedLive = null;
    }
    if (following) terminal.scrollTop = terminal.scrollHeight;
  }

  /** Keeps the newest lines only; the older ones stay in the log. */
  function trim() {
    if (shown <= MAX_TERMINAL_LINES) return;
    const removed = [];
    while (shown > MAX_TERMINAL_LINES) {
      removed.push(lines.firstChild.textContent);
      lines.firstChild.remove();
      shown -= 1;
    }
    start += byteLength(removed);
  }

  function drawEarlier() {
    earlier.hidden = start === 0;
    shownCount.textContent = `Showing the last ${shown} lines`;
  }

  function drawRun() {
    crumb.replaceChildren(h('a', { href: `#/servers/${run.server_id}` }, run.server_name), ' › ', run.title);
    by.textContent = `by ${shortUser(run.user)}`;
    by.title = run.user;
    const label = run.status === 'running' ? clock(Date.now() - run.started_at) : runTook(run);
    const badge = resultBadge(run.status, label);
    status.className = badge.className;
    status.replaceChildren(...badge.childNodes);
    stop.hidden = run.status !== 'running';
    detail.textContent = run.detail ? `${run.detail[0].toUpperCase()}${run.detail.slice(1)}.` : '';
    let number = 0;
    steps.replaceChildren(...run.steps.map((step) => {
      number += 1;
      const time = step.status === 'running' ? clock(Date.now() - step.started_at)
        : step.started_at && step.finished_at ? took(step.finished_at - step.started_at) : '';
      return h('li', { class: `st ${step.status}` }, h('span', { class: `n ${step.status}`, 'aria-label': step.status }, STEP_MARKS[step.status] || number),
        h('span', { class: 'title' }, step.title), h('span', { class: 't' }, time));
    }));
    if (run.status !== 'running') {
      cursor.remove();
      clearInterval(ticker);
      ticker = null;
    } else {
      if (!cursor.isConnected) terminal.append(cursor);
      if (!ticker) ticker = setInterval(drawRun, 1000); // Redraws the elapsed time only; no request.
    }
  }

  async function loadEarlier() {
    try {
      const from = Math.max(0, start - OUTPUT_TAIL);
      const chunk = await api('GET', `/runs/${id}/output${query({ offset: from, limit: start - from })}`);
      const part = chunkLines(chunk, Infinity, from > 0);
      const fragment = document.createDocumentFragment();
      for (const line of part.lines) fragment.append(terminalLine(line));
      const height = terminal.scrollHeight;
      lines.prepend(fragment);
      start = part.start;
      shown += part.lines.length;
      drawEarlier();
      following = false;
      follow.setAttribute('aria-pressed', 'false');
      terminal.scrollTop += terminal.scrollHeight - height;
    } catch (failure) {
      toast(failure.message, true);
    }
  }

  /** Reads the whole log in chunks and opens it as plain text. */
  async function openFullLog() {
    try {
      const parts = [];
      let offset = 0;
      for (;;) {
        const chunk = await api('GET', `/runs/${id}/output${query({ offset })}`);
        parts.push(chunk.text);
        if (chunk.end >= chunk.size || chunk.end === chunk.start) break;
        offset = chunk.end;
      }
      const url = URL.createObjectURL(new Blob(parts, { type: 'text/plain;charset=utf-8' }));
      if (!window.open(url, '_blank')) h('a', { href: url, download: `run-${id}.log` }).click();
      setTimeout(() => URL.revokeObjectURL(url), 60000);
    } catch (failure) {
      toast(failure.message, true);
    }
  }

  function listen() {
    source = new EventSource(`/api/runs/${id}/events?from=${have}`);
    source.addEventListener('run', (event) => {
      run = JSON.parse(event.data);
      drawRun();
      if (run.status !== 'running') { source.close(); source = null; }
    });
    source.addEventListener('output', (event) => {
      const chunk = JSON.parse(event.data);
      if (chunk.start < have) return;
      have = chunk.end;
      queue(chunk.text, chunk.live);
    });
  }

  async function load() {
    try {
      run = await api('GET', `/runs/${id}`);
      const chunk = await api('GET', `/runs/${id}/output${query({ tail: OUTPUT_TAIL })}`);
      const part = chunkLines(chunk, MAX_TERMINAL_LINES, false);
      start = part.start;
      have = chunk.end;
      drawRun();
      queue(part.lines.map((line) => `${line}\n`).join(''));
      if (run.status === 'running') listen();
    } catch (failure) {
      status.className = 'badge b-bad';
      status.textContent = 'Error';
      lines.replaceChildren(h('div', { class: 'ln r' }, failure.message));
    }
  }

  load();
  return {
    el,
    destroy() {
      if (source) source.close();
      cancelAnimationFrame(frame);
      clearInterval(ticker);
    },
  };
}

function historyView() {
  const filters = { server: '', operation: '', result: '', user: '', since: String(30 * 86400000), q: '' };
  const filterRow = h('div', { class: 'filters' });
  const exportLink = h('a', { class: 'btn', download: 'upkeep-history.csv' }, icon('download'), 'Export CSV');
  const table = virtualTable({
    columns: ['Run', 'Server', 'Operation', 'Result', 'By', 'Started', 'Took'],
    template: '80px minmax(120px, 1.2fr) minmax(130px, 1.3fr) 150px minmax(110px, 1fr) 120px 90px',
    cells: (run) => [`#${run.id}`, run.server_name, run.title, resultBadge(run.status), shortUser(run.user), when(run.started_at), runTook(run)],
    load: (before) => api('GET', `/runs${query({ ...params(), before })}`),
    onOpen: (run) => openRun(run.id),
    emptyText: 'No runs match these filters.',
    rowKey: (run) => run.id,
  });

  function params() {
    const since = filters.since ? Date.now() - Number(filters.since) : '';
    return { server: filters.server, operation: filters.operation, result: filters.result, user: filters.user, since, q: filters.q };
  }

  function refresh() {
    exportLink.href = `/api/runs/export.csv${query(params())}`;
    table.reset();
  }

  const set = (key) => (value) => { filters[key] = value; refresh(); };

  async function drawFilters() {
    let choices = { servers: [], operations: [], users: [] };
    try { choices = await api('GET', '/filters'); } catch (failure) { toast(failure.message, true); }
    filterRow.replaceChildren(
      select('Server', [['', 'Server: All'], ...choices.servers.map((server) => [String(server.id), server.name])], filters.server, set('server')),
      select('Operation', [['', 'Operation: All'], ...choices.operations.map((folder) => [folder, titleOf(folder)])], filters.operation, set('operation')),
      select('Result', [['', 'Result: All'], ...Object.entries(RESULTS).map(([value, [label]]) => [value, label])], filters.result, set('result')),
      select('User', [['', 'User: All'], ...choices.users.map((email) => [email, email])], filters.user, set('user')),
      select('Period', [[String(86400000), 'Last 24 hours'], [String(7 * 86400000), 'Last 7 days'], [String(30 * 86400000), 'Last 30 days'], [String(90 * 86400000), 'Last 90 days'], ['', 'All time']], filters.since, set('since')));
  }

  drawFilters();
  refresh();
  const el = [topBar('All runs on every server', 'History', exportLink), h('div', { class: 'content' },
    h('div', { class: 'panel list-panel' },
      h('div', { class: 'ph' }, filterRow, searchBox('Search runs', set('q'))),
      table.el,
      h('div', { class: 'foot' }, table.foot, h('span', {}, 'Sorted by newest'))))];
  const unfiltered = () => !filters.server && !filters.operation && !filters.result && !filters.user && !filters.q;
  return {
    el,
    onRunChange: (run) => table.upsert(run, unfiltered()),
    destroy: table.destroy,
  };
}

function activityView() {
  const filters = { user: '', server: '', action: '' };
  const filterRow = h('div', { class: 'filters' });
  const table = virtualTable({
    columns: ['When', 'User', 'Action', 'Server', 'Detail'],
    template: '120px minmax(120px, 1fr) 160px minmax(120px, 1fr) minmax(200px, 2fr)',
    cells: (entry) => [when(entry.at), entry.user === 'cli' ? 'CLI' : shortUser(entry.user), ACTIONS[entry.action] || entry.action, entry.server || '—', entry.detail],
    load: (before) => api('GET', `/activity${query({ ...filters, before })}`),
    emptyText: 'No activity matches these filters.',
    rowKey: (entry) => entry.id,
  });
  const set = (key) => (value) => { filters[key] = value; table.reset(); };

  async function drawFilters() {
    let choices = { servers: [], users: [] };
    try { choices = await api('GET', '/filters'); } catch (failure) { toast(failure.message, true); }
    filterRow.replaceChildren(
      select('User', [['', 'User: All'], ...choices.users.map((email) => [email, email]), ['cli', 'CLI']], filters.user, set('user')),
      select('Server', [['', 'Server: All'], ...choices.servers.map((server) => [server.name, server.name])], filters.server, set('server')),
      select('Action', [['', 'Action: All'], ...Object.entries(ACTIONS)], filters.action, set('action')));
  }

  drawFilters();
  table.reset();
  const el = [topBar('Who did what, and when', 'Activity'), h('div', { class: 'content' },
    h('div', { class: 'panel list-panel' }, h('div', { class: 'ph' }, filterRow), table.el,
      h('div', { class: 'foot' }, table.foot, h('span', {}, 'Sorted by newest'))))];
  return { el, destroy: table.destroy };
}

/* ---------- Shell: sidebar, routes and live changes ---------- */

const shell = {
  servers: [],
  view: null,
  events: null,
  started: false,

  start() {
    if (!this.started) {
      window.addEventListener('hashchange', () => this.render());
      this.started = true;
    }
    this.listen();
    this.refreshServers().then(() => this.render());
  },

  stop() {
    if (this.events) this.events.close();
    this.events = null;
    this.destroyView();
  },

  /** Runs that start or end anywhere, pushed by the service. */
  listen() {
    if (this.events) this.events.close();
    this.events = new EventSource('/api/events');
    this.events.addEventListener('run', (event) => {
      const run = JSON.parse(event.data);
      const server = this.servers.find((candidate) => candidate.id === run.server_id);
      if (server) server.running_run = run.status === 'running' ? run.id : null;
      this.drawSidebar();
      if (this.view && this.view.onRunChange) this.view.onRunChange(run);
    });
    // On reconnect the page reads the current state once.
    this.events.addEventListener('open', () => { if (this.view) this.refreshServers(); });
  },

  async refreshServers() {
    try {
      const answer = await api('GET', '/servers');
      this.servers = answer.servers;
    } catch (failure) {
      if (failure.status !== 401) toast(failure.message, true);
    }
    this.drawSidebar();
  },

  drawSidebar() {
    const route = location.hash;
    const list = document.getElementById('server-list');
    list.replaceChildren(...(this.servers.length ? this.servers.map((server) => h('a', {
      class: route === `#/servers/${server.id}` ? 'srv on' : 'srv',
      href: `#/servers/${server.id}`,
      'aria-current': route === `#/servers/${server.id}` ? 'page' : undefined,
    }, pip(server), h('span', {}, server.name))) : [h('div', { class: 'empty-note' }, 'No servers')]));
    const section = route.startsWith('#/history') ? 'history' : route.startsWith('#/activity') ? 'activity' : route === '' || route === '#/' ? 'overview' : '';
    document.querySelectorAll('[data-nav]').forEach((link) => {
      const on = link.dataset.nav === section;
      link.classList.toggle('on', on);
      if (on) link.setAttribute('aria-current', 'page'); else link.removeAttribute('aria-current');
    });
  },

  destroyView() {
    if (this.view && this.view.destroy) this.view.destroy();
    this.view = null;
  },

  render() {
    if (!session.csrf) return;
    this.destroyView();
    closeDialogs();
    const route = location.hash.replace(/^#/, '') || '/';
    const match = (pattern) => route.match(pattern);
    let found;
    if ((found = match(/^\/servers\/(\d+)$/))) this.view = serverView(Number(found[1]));
    else if ((found = match(/^\/runs\/(\d+)$/))) this.view = runView(Number(found[1]));
    else if (route === '/history') this.view = historyView();
    else if (route === '/activity') this.view = activityView();
    else this.view = overviewView();
    const main = document.getElementById('view');
    main.replaceChildren(...[this.view.el].flat());
    main.scrollTop = 0;
    this.drawSidebar();
  },
};

/** Called by the desktop window when it is closed while runs are active. */
window.upkeepCloseWarning = (active, seconds) => {
  toast(`${active === 1 ? 'A run is' : `${active} runs are`} active. Closing Upkeep ends ${active === 1 ? 'it' : 'them'} and marks ${active === 1 ? 'it' : 'them'} interrupted. Close again within ${seconds} s to quit anyway.`, true);
};

/* ---------- Start ---------- */

document.addEventListener('DOMContentLoaded', async () => {
  document.querySelectorAll('[data-icon]').forEach((slot) => slot.replaceWith(icon(slot.dataset.icon)));
  document.getElementById('add-server').addEventListener('click', () => serverDialog());
  document.getElementById('lock').addEventListener('click', lock);
  document.getElementById('sign-out').addEventListener('click', signOut);
  for (const type of ['mousedown', 'keydown', 'wheel', 'touchstart']) document.addEventListener(type, noteInput, { passive: true, capture: true });
  try {
    startSession(await api('GET', '/session'));
  } catch (failure) {
    const email = storedEmail();
    if (failure.code === 'locked' && email) showLock(email);
    else if (failure.status === 0) showAuth(h('div', { class: 'auth-card' }, h('h1', {}, 'Upkeep is not reachable'), h('p', { class: 'hint' }, failure.message)));
    else showSignIn();
  }
});

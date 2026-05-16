// Nanoguard Console — vanilla JS SPA

const $ = (sel, el = document) => el.querySelector(sel);
const $$ = (sel, el = document) => Array.from(el.querySelectorAll(sel));

let currentUser = null;

async function api(path, opts = {}) {
  const res = await fetch(path, {
    credentials: 'same-origin',
    headers: { 'Content-Type': 'application/json', ...opts.headers },
    ...opts,
  });
  if (res.status === 401 && path !== '/api/login') {
    showLogin();
    throw new Error('unauthorized');
  }
  const data = res.headers.get('content-type')?.includes('application/json')
    ? await res.json()
    : await res.text();
  if (!res.ok) {
    const msg = data?.error || `${res.status} ${res.statusText}`;
    throw new Error(msg);
  }
  return data;
}

// ── Navigation ──────────────────────────────────────────────

const TABS = ['overview', 'tokens', 'budget', 'audit', 'config'];
const ADMIN_TABS = ['users'];

function renderNav() {
  const nav = $('#nav');
  const all = [...TABS, ...(currentUser?.role === 'admin' ? ADMIN_TABS : [])];
  nav.innerHTML = all.map(t =>
    `<button data-tab="${t}" class="${t === 'overview' ? 'active' : ''}">${t}</button>`
  ).join('');
  $$('nav button').forEach(btn => {
    btn.addEventListener('click', () => switchTab(btn.dataset.tab));
  });
}

function switchTab(name) {
  $$('.tab').forEach(el => el.classList.add('hidden'));
  $$(`#tab-${name}`).forEach(el => el.classList.remove('hidden'));
  $$('nav button').forEach(b => b.classList.toggle('active', b.dataset.tab === name));
  if (name === 'tokens') loadTokens();
  if (name === 'budget') loadBudget();
  if (name === 'audit') loadAudit();
  if (name === 'config') loadConfig();
  if (name === 'users') loadUsers();
}

// ── Auth ────────────────────────────────────────────────────

async function init() {
  try {
    currentUser = await api('/api/me');
    showMain();
  } catch {
    showLogin();
  }
}

function showLogin() {
  currentUser = null;
  $('#login-page').classList.remove('hidden');
  $('#main-page').classList.add('hidden');
}

function showMain() {
  $('#login-page').classList.add('hidden');
  $('#main-page').classList.remove('hidden');
  $('#user-name').textContent = currentUser.display_name || currentUser.username;
  renderNav();
  switchTab('overview');
  loadOverview();
}

$('#login-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  $('#login-error').textContent = '';
  try {
    const res = await api('/api/login', {
      method: 'POST',
      body: JSON.stringify({
        username: $('#username').value,
        password: $('#password').value,
      }),
    });
    currentUser = res.user;
    showMain();
  } catch (err) {
    $('#login-error').textContent = err.message;
  }
});

$('#logout-btn').addEventListener('click', async () => {
  await api('/api/logout', { method: 'POST' });
  showLogin();
});

// ── Overview ────────────────────────────────────────────────

async function loadOverview() {
  try {
    const audit = await api('/api/audit?limit=1');
    $('#audit-count').textContent = (audit.data?.length ?? 0) ? 'available' : 'empty';
  } catch { $('#audit-count').textContent = 'unavailable'; }

  try {
    const budget = await api('/api/budget');
    $('#budget-count').textContent = budget.data?.length ?? 0;
  } catch { $('#budget-count').textContent = 'unavailable'; }

  try {
    const cfg = await api('/api/config');
    $('#cfg-count').textContent = Object.keys(cfg.files ?? {}).length;
  } catch { $('#cfg-count').textContent = 'unavailable'; }
}

$$('[data-tab]').forEach(btn => {
  if (!btn.closest('nav')) {
    btn.addEventListener('click', () => switchTab(btn.dataset.tab));
  }
});

// ── Tokens ──────────────────────────────────────────────────

async function loadTokens() {
  try {
    const res = await api('/api/tokens');
    const tbody = $('#tokens-table tbody');
    tbody.innerHTML = (res.data || []).map(t => `
      <tr>
        <td><code>${esc(t.prefix)}</code></td>
        <td>${esc(t.label || '')}</td>
        <td>${fmtDate(t.created_at)}</td>
        <td>${t.expires_at ? fmtDate(t.expires_at) : '—'}</td>
        <td>${t.revoked_at ? fmtDate(t.revoked_at) : '—'}</td>
        <td>${t.last_used_at ? fmtDate(t.last_used_at) : '—'}</td>
        <td>${t.revoked_at ? '' : `<button class="btn small danger" data-revoke="${t.id}">Revoke</button>`}</td>
      </tr>
    `).join('');
    $$('#tokens-table [data-revoke]').forEach(btn => {
      btn.addEventListener('click', async () => {
        if (!confirm('Revoke this token?')) return;
        await api(`/api/tokens/${btn.dataset.revoke}`, { method: 'DELETE' });
        loadTokens();
      });
    });
  } catch (err) {
    $('#tokens-table tbody').innerHTML = `<tr><td colspan="6" class="error">${esc(err.message)}</td></tr>`;
  }
}

$('#create-token-btn').addEventListener('click', () => {
  $('#token-modal').showModal();
  $('#token-label').value = '';
  $('#token-expires').value = '';
  $('#token-result').classList.add('hidden');
  $('#token-submit').classList.remove('hidden');
});

$('#token-close').addEventListener('click', () => $('#token-modal').close());

$('#token-submit').addEventListener('click', async (e) => {
  e.preventDefault();
  try {
    const res = await api('/api/tokens', {
      method: 'POST',
      body: JSON.stringify({
        label: $('#token-label').value,
        expires_at: $('#token-expires').value || undefined,
      }),
    });
    $('#token-secret').value = res.token;
    $('#token-result').classList.remove('hidden');
    $('#token-submit').classList.add('hidden');
    loadTokens();
  } catch (err) {
    alert(err.message);
  }
});

$('#copy-token-btn').addEventListener('click', () => {
  const el = $('#token-secret');
  el.select();
  document.execCommand('copy');
  $('#copy-token-btn').textContent = 'Copied!';
  setTimeout(() => $('#copy-token-btn').textContent = 'Copy', 1500);
});

// ── Budget ──────────────────────────────────────────────────

async function loadBudget() {
  try {
    const res = await api('/api/budget');
    const tbody = $('#budget-table tbody');
    tbody.innerHTML = (res.data || []).map(b => {
      const pct = b.limit ? Math.round((b.usage / b.limit) * 100) : 0;
      return `
        <tr>
          <td><code>${esc(b.api_key)}</code></td>
          <td>${b.usage.toLocaleString()}</td>
          <td>${b.limit?.toLocaleString() ?? '—'}</td>
          <td>${b.limit ? `<div class="bar"><div style="width:${pct}%">${pct}%</div></div>` : '—'}</td>
        </tr>
      `;
    }).join('');
  } catch (err) {
    $('#budget-table tbody').innerHTML = `<tr><td colspan="4" class="error">${esc(err.message)}</td></tr>`;
  }
}

// ── Audit ───────────────────────────────────────────────────

async function loadAudit() {
  const verdict = $('#audit-verdict').value;
  const qs = verdict ? `?verdict=${encodeURIComponent(verdict)}` : '';
  try {
    const res = await api(`/api/audit${qs}`);
    const tbody = $('#audit-table tbody');
    tbody.innerHTML = (res.data || []).map(a => `
      <tr>
        <td>${fmtDate(a.timestamp)}</td>
        <td><span class="badge ${a.verdict}">${esc(a.verdict)}</span></td>
        <td>${esc(a.rule_id || a.rule || '—')}</td>
        <td>${esc(a.model || '—')}</td>
      </tr>
    `).join('');
  } catch (err) {
    $('#audit-table tbody').innerHTML = `<tr><td colspan="4" class="error">${esc(err.message)}</td></tr>`;
  }
}

$('#audit-refresh').addEventListener('click', loadAudit);
$('#audit-verdict').addEventListener('change', loadAudit);

// ── Config ──────────────────────────────────────────────────

async function loadConfig() {
  try {
    const res = await api('/api/config');
    const container = $('#config-list');
    container.innerHTML = Object.entries(res.files || {}).map(([name, content]) => `
      <div class="config-file">
        <h4>${esc(name)}</h4>
        <pre><code>${esc(content)}</code></pre>
      </div>
    `).join('');
  } catch (err) {
    $('#config-list').innerHTML = `<p class="error">${esc(err.message)}</p>`;
  }
}

// ── Users (admin) ───────────────────────────────────────────

async function loadUsers() {
  try {
    const res = await api('/api/users');
    const tbody = $('#users-table tbody');
    tbody.innerHTML = (res.data || []).map(u => `
      <tr>
        <td>${u.id}</td>
        <td>${esc(u.username)}</td>
        <td>${esc(u.role)}</td>
        <td>${u.disabled ? 'Yes' : 'No'}</td>
        <td>${fmtDate(u.created_at)}</td>
        <td>
          <button class="btn small" data-edit="${u.id}">Edit</button>
          ${u.disabled
            ? `<button class="btn small" data-enable="${u.id}">Enable</button>`
            : `<button class="btn small danger" data-disable="${u.id}">Disable</button>`}
        </td>
      </tr>
    `).join('');
    $$('#users-table [data-disable]').forEach(btn => {
      btn.addEventListener('click', async () => {
        if (!confirm('Disable this user?')) return;
        await api(`/api/users/${btn.dataset.disable}`, {
          method: 'PUT',
          body: JSON.stringify({ disabled: true }),
        });
        loadUsers();
      });
    });
    $$('#users-table [data-enable]').forEach(btn => {
      btn.addEventListener('click', async () => {
        await api(`/api/users/${btn.dataset.enable}`, {
          method: 'PUT',
          body: JSON.stringify({ disabled: false }),
        });
        loadUsers();
      });
    });
  } catch (err) {
    $('#users-table tbody').innerHTML = `<tr><td colspan="6" class="error">${esc(err.message)}</td></tr>`;
  }
}

$('#create-user-btn').addEventListener('click', () => {
  $('#user-modal').showModal();
  $('#new-username').value = '';
  $('#new-password').value = '';
  $('#new-role').value = 'user';
  $('#user-error').textContent = '';
});

$('#user-close').addEventListener('click', () => $('#user-modal').close());

$('#user-submit').addEventListener('click', async (e) => {
  e.preventDefault();
  $('#user-error').textContent = '';
  try {
    await api('/api/users', {
      method: 'POST',
      body: JSON.stringify({
        username: $('#new-username').value,
        password: $('#new-password').value,
        role: $('#new-role').value,
      }),
    });
    $('#user-modal').close();
    loadUsers();
  } catch (err) {
    $('#user-error').textContent = err.message;
  }
});

// ── Utilities ───────────────────────────────────────────────

function esc(str) {
  const div = document.createElement('div');
  div.textContent = str ?? '';
  return div.innerHTML;
}

function fmtDate(iso) {
  if (!iso) return '—';
  try {
    return new Date(iso).toLocaleString();
  } catch {
    return iso;
  }
}

// Start
init();

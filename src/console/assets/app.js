// Nanoguard Console — vanilla JS SPA

const $ = (sel, el = document) => el.querySelector(sel);
const $$ = (sel, el = document) => Array.from(el.querySelectorAll(sel));

let currentUser = null;
// Per-session CSRF token. Issued on login, refreshed from /api/me on init,
// and rotated by the server on every successful mutating request via the
// X-CSRF-Token-Next response header.
let csrfToken = null;

const MUTATING_METHODS = new Set(['POST', 'PUT', 'PATCH', 'DELETE']);

async function api(path, opts = {}) {
  // Pull headers out of opts so the final fetch spread can't accidentally
  // overwrite our merged headers (and silently drop the CSRF token) if a
  // future caller passes its own headers object.
  const { headers: optHeaders, ...restOpts } = opts;
  const method = (opts.method || 'GET').toUpperCase();
  const headers = { 'Content-Type': 'application/json', ...(optHeaders || {}) };
  if (MUTATING_METHODS.has(method) && path !== '/api/login' && csrfToken) {
    headers['X-CSRF-Token'] = csrfToken;
  }
  const res = await fetch(path, {
    credentials: 'same-origin',
    ...restOpts,
    headers,
  });
  // Pick up a rotated CSRF token before throwing on non-2xx so the next
  // request after a 403-on-rotate uses the fresh value.
  const nextCsrf = res.headers.get('x-csrf-token-next');
  if (nextCsrf) {
    csrfToken = nextCsrf;
  }
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
    const me = await api('/api/me');
    currentUser = me;
    // /api/me echoes the current CSRF token so the SPA can recover it on a
    // page refresh — otherwise the first mutation would fail with 403 until
    // the user re-logged in.
    if (me?.csrf_token) {
      csrfToken = me.csrf_token;
    }
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
    csrfToken = res.csrf_token || null;
    showMain();
  } catch (err) {
    $('#login-error').textContent = err.message;
  }
});

$('#logout-btn').addEventListener('click', async () => {
  try {
    await api('/api/logout', { method: 'POST' });
  } catch {}
  csrfToken = null;
  showLogin();
});

// ── Overview ────────────────────────────────────────────────

async function loadOverview() {
  try {
    const cfg = await api('/api/config');
    $('#cfg-count').textContent = Object.keys(cfg.files || {}).length;
    const audit = await api('/api/audit?limit=1');
    $('#audit-count').textContent = (audit.data || []).length > 0 ? 'yes' : 'none';
    const budget = await api('/api/budget');
    $('#budget-count').textContent = (budget.data || []).length;
  } catch {}

  // The "Getting Started" + "Guards Active" panels are populated from
  // /api/overview. Failing this fetch should not blank the existing
  // three cards above, so we wrap it independently.
  try {
    const ov = await api('/api/overview');
    renderGettingStarted(ov);
    renderGuards(ov);
  } catch (err) {
    $('#getting-started-body').textContent = `error: ${err.message}`;
  }
}

function renderGettingStarted(ov) {
  const proxyUrl = ov.proxy_url;
  const authOn = ov.auth?.enabled === true;
  const hasToken = (ov.user_token_count || 0) > 0;

  // Show the user three things they actually need to send the first
  // request: the proxy URL, a curl example with the right Bearer
  // wiring, and the OpenAI SDK environment-variable form.
  const authBlurb = authOn
    ? (hasToken
        ? `<p class="hint ok">Client authentication is <strong>enabled</strong>. Use one of your <a href="#" data-tab-link="tokens">tokens</a> as the Bearer.</p>`
        : `<p class="hint warn">Client authentication is <strong>enabled</strong>, but you have no tokens yet. <a href="#" data-tab-link="tokens">Create one</a> before sending a request.</p>`)
    : `<p class="hint">Client authentication is <strong>disabled</strong>. Any caller can reach the proxy on this host — fine for local dev, not safe to expose on a network. Enable <code>[auth].enabled = true</code> in <code>nanoguard.toml</code> and start issuing tokens before opening the listener up.</p>`;

  const bearerCurl = authOn ? `\\\n  -H "Authorization: Bearer ng_${ov.auth.env_marker || 'p'}_..." ` : '';
  const curlExample = `curl ${proxyUrl}/v1/chat/completions \\\n  -H "Content-Type: application/json" ${bearerCurl}\\\n  -d '{"model":"${ov.backend.model || 'gpt-4o-mini'}","messages":[{"role":"user","content":"hello"}]}'`;

  const sdkExample = authOn
    ? `# OpenAI Python SDK\nexport OPENAI_BASE_URL=${proxyUrl}/v1\nexport OPENAI_API_KEY=ng_${ov.auth.env_marker || 'p'}_...   # from the Tokens tab\n\n# OpenAI Node SDK\nprocess.env.OPENAI_BASE_URL = "${proxyUrl}/v1";\nprocess.env.OPENAI_API_KEY = "ng_${ov.auth.env_marker || 'p'}_...";`
    : `# OpenAI Python SDK\nexport OPENAI_BASE_URL=${proxyUrl}/v1\nexport OPENAI_API_KEY=any-string-works-when-auth-disabled\n\n# OpenAI Node SDK\nprocess.env.OPENAI_BASE_URL = "${proxyUrl}/v1";`;

  const endpointsHtml = (ov.endpoints || []).map(p =>
    `<li><code>${esc(proxyUrl + p)}</code></li>`
  ).join('');

  $('#getting-started-body').innerHTML = `
    <p class="hint">Point your LLM client at the proxy URL below. Nanoguard exposes an OpenAI-compatible <code>/v1/chat/completions</code> and Anthropic-compatible <code>/v1/messages</code>; the rest of your stack stays the same.</p>
    <dl class="kv">
      <dt>Proxy URL</dt><dd><code>${esc(proxyUrl)}</code></dd>
      <dt>Backend</dt><dd><code>${esc(ov.backend.provider)}</code> → <code>${esc(ov.backend.endpoint)}</code>${ov.backend.model ? ` (model: <code>${esc(ov.backend.model)}</code>)` : ''}</dd>
      <dt>Endpoints</dt><dd><ul class="endpoints">${endpointsHtml}</ul></dd>
      <dt>Your tokens</dt><dd>${ov.user_token_count} active <a href="#" data-tab-link="tokens">(manage)</a></dd>
    </dl>
    ${authBlurb}
    <h4>Send a request with curl</h4>
    <pre class="example"><code>${esc(curlExample)}</code></pre>
    <h4>Use it from the OpenAI SDK</h4>
    <pre class="example"><code>${esc(sdkExample)}</code></pre>
  `;

  // Wire the "manage tokens" inline links to the tab nav.
  $$('#getting-started-body [data-tab-link]').forEach(a => {
    a.addEventListener('click', e => {
      e.preventDefault();
      switchTab(a.dataset.tabLink);
    });
  });
}

function renderGuards(ov) {
  const items = (ov.guards || []).map(g => `
    <li class="guard ${g.enabled ? 'on' : 'off'}">
      <span class="dot" aria-hidden="true"></span>
      <span class="name">${esc(g.name)}</span>
      <span class="summary">${esc(g.summary)}</span>
    </li>
  `).join('');
  $('#guards-body').innerHTML = `
    <ul class="guard-list">${items}</ul>
    <p class="hint subtle">Edit <code>nanoguard.toml</code> via the Config tab to change these. Most changes take effect on the next reload (SIGHUP / <code>RELOAD</code> over the configured socket); a small set of keys are restart-only — see <code>docs/design/hot-reload.md</code>.</p>
  `;
}

$$('[data-tab]').forEach(btn => {
  btn.addEventListener('click', () => switchTab(btn.dataset.tab));
});

// ── Tokens ──────────────────────────────────────────────────

async function loadTokens() {
  try {
    const res = await api('/api/tokens');
    const tbody = $('#tokens-table tbody');
    tbody.innerHTML = (res.data || []).map(t => `
      <tr>
        <td><code>${esc(t.prefix)}</code></td>
        <td>${esc(t.label)}</td>
        <td>${fmtDate(t.created_at)}</td>
        <td>${fmtDate(t.last_used_at)}</td>
        <td>${fmtDate(t.expires_at)}</td>
        <td>${t.revoked_at ? 'Yes' : 'No'}</td>
        <td>${t.revoked_at ? '' : `<button class="btn small danger" data-revoke="${t.id}">Revoke</button>`}</td>
      </tr>
    `).join('');
    $$('#tokens-table [data-revoke]').forEach(btn => {
      btn.addEventListener('click', async () => {
        await api(`/api/tokens/${btn.dataset.revoke}`, { method: 'DELETE' });
        loadTokens();
      });
    });
  } catch (err) {
    $('#tokens-table tbody').innerHTML = `<tr><td colspan="7" class="error">${esc(err.message)}</td></tr>`;
  }
}

$('#create-token-btn').addEventListener('click', () => {
  $('#token-modal').showModal();
  $('#token-label').value = '';
  $('#token-expires').value = '';
  $('#token-result').classList.add('hidden');
  $('#token-secret').value = '';
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
    loadTokens();
  } catch (err) {
    alert(err.message);
  }
});

$('#copy-token-btn').addEventListener('click', () => {
  $('#token-secret').select();
  document.execCommand('copy');
});

// ── Budget ──────────────────────────────────────────────────

async function loadBudget() {
  try {
    const res = await api('/api/budget');
    const tbody = $('#budget-table tbody');
    const isAdmin = currentUser?.role === 'admin';
    tbody.innerHTML = (res.data || []).map(b => {
      const pct = b.limit ? Math.round((b.usage / b.limit) * 100) : 0;
      const adminCol = isAdmin
        ? `<td>
             <button class="btn small" data-budget-edit="${esc(b.api_key)}" data-budget-limit="${b.limit ?? ''}">Edit limit</button>
             <button class="btn small danger" data-budget-reset="${esc(b.api_key)}">Reset usage</button>
           </td>`
        : '';
      return `
        <tr>
          <td><code>${esc(b.api_key)}</code></td>
          <td>${b.usage.toLocaleString()}</td>
          <td>${b.limit?.toLocaleString() ?? '—'}</td>
          <td>${b.limit ? `<div class="bar"><div style="width:${pct}%">${pct}%</div></div>` : '—'}</td>
          ${adminCol}
        </tr>
      `;
    }).join('');

    // Wire the per-row admin actions. Edit prompts for a new limit
    // (empty input clears the cap); Reset zeroes the usage counter.
    $$('#budget-table [data-budget-edit]').forEach(btn => {
      btn.addEventListener('click', async () => {
        const apiKey = btn.dataset.budgetEdit;
        const current = btn.dataset.budgetLimit;
        const input = prompt(`New token limit for ${apiKey}\n(leave empty to clear; current: ${current || 'unlimited'})`, current);
        if (input === null) return;
        const trimmed = input.trim();
        const payload = { api_key: apiKey };
        if (trimmed !== '') {
          const n = Number(trimmed);
          if (!Number.isFinite(n) || n < 0 || Math.floor(n) !== n) {
            alert('Limit must be a non-negative integer or empty.');
            return;
          }
          payload.limit = n;
        }
        try {
          await api('/api/budget/limit', { method: 'POST', body: JSON.stringify(payload) });
          loadBudget();
        } catch (err) {
          alert(err.message);
        }
      });
    });
    $$('#budget-table [data-budget-reset]').forEach(btn => {
      btn.addEventListener('click', async () => {
        const apiKey = btn.dataset.budgetReset;
        if (!confirm(`Reset usage counter for ${apiKey}?`)) return;
        try {
          await api('/api/budget/reset', { method: 'POST', body: JSON.stringify({ api_key: apiKey }) });
          loadBudget();
        } catch (err) {
          alert(err.message);
        }
      });
    });
  } catch (err) {
    $('#budget-table tbody').innerHTML = `<tr><td colspan="5" class="error">${esc(err.message)}</td></tr>`;
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

let editingPath = null;
let editOriginal = '';

// Short, plain-English description of what each editable file
// controls. Shown above the file content in the Config tab so an
// operator who has never read docs/design/*.md still has a chance of
// editing the right file. Matched against the file path on display.
function fileCaption(path) {
  if (path === 'nanoguard.toml') {
    return 'Top-level proxy config: listen address, backend, [auth], PII / spotlight / tool gate / schema toggles, [budget], [audit], [reload], [console]. Most keys are reload-safe; a small set (listen, log_level, [backend], db paths, audit path) are restart-only.';
  }
  if (path.startsWith('dicts/') && path.includes('pii-regex')) {
    return 'PII regex dictionary. Each line is `/<regex>/<TAB><ENTITY>` (e.g. `/[a-z]+@[a-z]+/<TAB>EMAIL`). Matched entities are masked, rejected, or logged depending on [input.pii].action.';
  }
  if (path.startsWith('dicts/') && path.endsWith('.txt')) {
    return 'Keyword dictionary used by the input matcher. Each line is `<pattern><TAB><action-key>` where action-key is 0 (block) / 1 (alert) / 2 (flag). Reload-safe.';
  }
  if (path.startsWith('policies/') && (path.endsWith('.yaml') || path.endsWith('.yml'))) {
    return 'YAML policy bundle. Bundles keyword + redaction rules with policy metadata (rule_id, severity, compliance tags) so audit entries carry rule lineage. Reload-safe.';
  }
  return '';
}

async function loadConfig() {
  editingPath = null;
  $('#config-editor').classList.add('hidden');
  $('#config-list').classList.remove('hidden');
  try {
    const res = await api('/api/config');
    const container = $('#config-list');
    container.innerHTML = Object.entries(res.files || {}).map(([name, content]) => {
      const cap = fileCaption(name);
      return `
      <div class="config-file">
        <h4>${esc(name)} ${currentUser?.role === 'admin' ? `<button class="btn small" data-edit="${esc(name)}">Edit</button>` : ''}</h4>
        ${cap ? `<p class="hint subtle file-caption">${esc(cap)}</p>` : ''}
        <pre><code>${esc(content)}</code></pre>
      </div>
    `;
    }).join('');
    $$('.config-file [data-edit]').forEach(btn => {
      btn.addEventListener('click', () => startEdit(btn.dataset.edit, res.files[btn.dataset.edit]));
    });
  } catch (err) {
    $('#config-list').innerHTML = `<p class="error">${esc(err.message)}</p>`;
  }
}

function startEdit(path, content) {
  editingPath = path;
  editOriginal = content;
  $('#edit-path').textContent = path;
  $('#edit-content').value = content;
  $('#edit-error').textContent = '';
  $('#edit-status').textContent = '';
  $('#edit-status').className = 'status';
  $('#config-list').classList.add('hidden');
  $('#config-editor').classList.remove('hidden');
  loadBackups(path);
}

function cancelEdit() {
  editingPath = null;
  $('#config-editor').classList.add('hidden');
  $('#config-list').classList.remove('hidden');
}

$('#edit-cancel').addEventListener('click', cancelEdit);

$('#edit-validate').addEventListener('click', async () => {
  $('#edit-error').textContent = '';
  $('#edit-status').textContent = '';
  $('#edit-status').className = 'status';
  try {
    const res = await api('/api/validate', {
      method: 'POST',
      body: JSON.stringify({ path: editingPath, content: $('#edit-content').value }),
    });
    if (res.valid) {
      $('#edit-status').textContent = 'Valid';
      $('#edit-status').classList.add('success');
    } else {
      $('#edit-status').textContent = res.error || 'Invalid';
      $('#edit-status').classList.add('error');
    }
  } catch (err) {
    $('#edit-status').textContent = err.message;
    $('#edit-status').classList.add('error');
  }
});

$('#edit-save').addEventListener('click', async () => {
  $('#edit-error').textContent = '';
  $('#edit-status').textContent = '';
  $('#edit-status').className = 'status';
  try {
    const res = await api('/api/edit', {
      method: 'POST',
      body: JSON.stringify({ path: editingPath, content: $('#edit-content').value, summary: 'edited via console' }),
    });
    $('#edit-status').textContent = `Saved. Reload: ${res.reload?.triggered ? 'triggered via ' + res.reload.method : 'not triggered'}`;
    $('#edit-status').classList.add('success');
    if (res.reload?.error) {
      $('#edit-status').textContent += ` — ${res.reload.error}`;
    }
    editOriginal = $('#edit-content').value;
    // Poll reload status if triggered.
    if (res.reload?.triggered) {
      pollReloadStatus();
    }
    loadBackups(editingPath);
  } catch (err) {
    $('#edit-status').textContent = err.message;
    $('#edit-status').classList.add('error');
  }
});

async function pollReloadStatus() {
  const since = Date.now() / 1000 - 5;
  for (let i = 0; i < 10; i++) {
    await new Promise(r => setTimeout(r, 1000));
    try {
      const status = await api(`/api/reload/status?since=${since}`);
      if (status.ready) {
        if (status.ok) {
          $('#edit-status').textContent = 'Reload successful.';
          $('#edit-status').classList.add('success');
        } else {
          $('#edit-status').textContent = 'Reload failed. See proxy audit log for details.';
          $('#edit-status').classList.add('error');
        }
        return;
      }
    } catch {}
  }
  $('#edit-status').textContent = 'Reload status: timeout waiting for proxy.';
  $('#edit-status').classList.add('error');
}

async function loadBackups(path) {
  try {
    const res = await api(`/api/backups?path=${encodeURIComponent(path)}`);
    const tbody = $('#backups-table tbody');
    const rows = res.data || [];
    tbody.innerHTML = rows.map(b => `
      <tr>
        <td>${esc(b.name)}</td>
        <td>${b.size}</td>
        <td>${fmtDate(new Date(b.created_at * 1000).toISOString())}</td>
        <td><button class="btn small" data-revert="${esc(b.name)}">Revert</button></td>
      </tr>
    `).join('');
    $$('#backups-table [data-revert]').forEach(btn => {
      btn.addEventListener('click', async () => {
        if (!confirm(`Revert ${path} to ${btn.dataset.revert}?`)) return;
        try {
          const result = await api('/api/revert', {
            method: 'POST',
            body: JSON.stringify({ path, backup: btn.dataset.revert }),
          });
          const config = await api('/api/config');
          const revertedContent = config.files?.[path] ?? '';
          $('#edit-content').value = revertedContent;
          editOriginal = revertedContent;
          $('#edit-status').className = 'status';
          $('#edit-status').textContent = `Reverted. Reload: ${result.reload?.triggered ? 'triggered' : 'not triggered'}`;
          $('#edit-status').classList.add('success');
          if (result.reload?.triggered) {
            pollReloadStatus();
          }
          loadBackups(path);
        } catch (err) {
          $('#edit-status').textContent = err.message;
          $('#edit-status').classList.add('error');
        }
      });
    });
    $('#backups-section').classList.toggle('hidden', rows.length === 0);
  } catch {
    $('#backups-section').classList.add('hidden');
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

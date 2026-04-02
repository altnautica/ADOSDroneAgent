/* ADOS Agent — Shared Utilities */

const API_BASE = '/api';
let _pollTimer = null;

/** Fetch JSON from /api/{path}. Returns parsed JSON or null on error. */
async function fetchApi(path) {
  try {
    const res = await fetch(`${API_BASE}/${path}`);
    if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
    return await res.json();
  } catch (e) {
    console.error(`API ${path}:`, e);
    return null;
  }
}

/** PUT JSON to /api/config. Returns true on success. */
async function putConfig(data) {
  try {
    const res = await fetch(`${API_BASE}/config`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(data),
    });
    if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
    showToast('Settings saved');
    return true;
  } catch (e) {
    console.error('putConfig:', e);
    showToast('Save failed', true);
    return false;
  }
}

/** POST a command to /api/command. */
async function postCommand(command) {
  try {
    const res = await fetch(`${API_BASE}/command`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ command }),
    });
    if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
    return true;
  } catch (e) {
    console.error('postCommand:', e);
    showToast('Command failed', true);
    return false;
  }
}

/** Highlight active nav link based on current filename. */
function navInit() {
  const page = location.pathname.split('/').pop() || 'index.html';
  document.querySelectorAll('nav a').forEach(a => {
    const href = a.getAttribute('href');
    if (href === page || (page === '' && href === 'index.html')) {
      a.classList.add('active');
    }
  });
}

/** Format seconds into "Xh Ym" or "Xm Xs". */
function formatUptime(seconds) {
  if (seconds == null) return '--';
  const s = Math.floor(seconds);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ${m % 60}m`;
  const d = Math.floor(h / 24);
  return `${d}d ${h % 24}h`;
}

/** Return 0-4 based on signal dBm. */
function signalBars(dbm) {
  if (dbm == null) return 0;
  if (dbm >= -65) return 4;
  if (dbm >= -75) return 3;
  if (dbm >= -85) return 2;
  if (dbm >= -95) return 1;
  return 0;
}

/** Render signal bars HTML. */
function signalBarsHtml(dbm) {
  const level = signalBars(dbm);
  const heights = [4, 7, 10, 14];
  return `<span class="signal-bars">${heights.map((h, i) =>
    `<span class="bar${i < level ? ' on' : ''}" style="height:${h}px"></span>`
  ).join('')}</span>`;
}

/** Set inner text safely. */
function setText(id, val) {
  const el = document.getElementById(id);
  if (el) el.textContent = val != null ? val : '--';
}

/** Set innerHTML safely. */
function setHtml(id, val) {
  const el = document.getElementById(id);
  if (el) el.innerHTML = val;
}

/** Set badge class: ok/warn/err. */
function setBadge(id, status, label) {
  const el = document.getElementById(id);
  if (!el) return;
  el.className = `badge ${status}`;
  // preserve the dot pseudo-element, only set text
  el.textContent = label || '';
}

/** Set progress bar. Returns color class used. */
function setProgress(id, percent) {
  const fill = document.getElementById(id);
  if (!fill) return;
  const p = Math.max(0, Math.min(100, percent || 0));
  fill.style.width = p + '%';
  fill.className = 'progress-fill' + (p > 85 ? ' err' : p > 65 ? ' warn' : ' ok');
}

/** Simple toast notification. */
function showToast(msg, isError) {
  let t = document.getElementById('_toast');
  if (!t) {
    t = document.createElement('div');
    t.id = '_toast';
    t.style.cssText = 'position:fixed;bottom:20px;left:50%;transform:translateX(-50%);padding:10px 20px;border-radius:8px;font-size:14px;font-weight:500;z-index:100;transition:opacity 0.3s;pointer-events:none;';
    document.body.appendChild(t);
  }
  t.style.background = isError ? '#EF4444' : '#22C55E';
  t.style.color = '#fff';
  t.textContent = msg;
  t.style.opacity = '1';
  clearTimeout(t._timer);
  t._timer = setTimeout(() => { t.style.opacity = '0'; }, 2000);
}

/** Battery percentage color. */
function battColor(pct) {
  if (pct == null) return '';
  if (pct <= 20) return 'color:var(--error)';
  if (pct <= 40) return 'color:var(--warning)';
  return 'color:var(--success)';
}

/** Poll /api/status every 3s. Calls optional page-specific callback. */
function pollStatus(callback) {
  async function tick() {
    const data = await fetchApi('status');
    if (data && callback) callback(data);
  }
  tick();
  _pollTimer = setInterval(tick, 3000);
}

/** Stop polling. */
function stopPolling() {
  if (_pollTimer) clearInterval(_pollTimer);
}

/** Toggle password visibility. */
function togglePw(inputId, btn) {
  const inp = document.getElementById(inputId);
  if (!inp) return;
  const show = inp.type === 'password';
  inp.type = show ? 'text' : 'password';
  btn.textContent = show ? 'Hide' : 'Show';
}

/** DOMContentLoaded init. */
document.addEventListener('DOMContentLoaded', () => {
  navInit();
  // Setup page handles its own logic; don't auto-poll
  if (location.pathname.includes('setup')) return;
  // Each page sets window._onStatus for page-specific updates
  if (typeof window._onStatus === 'function') {
    pollStatus(window._onStatus);
  }
});

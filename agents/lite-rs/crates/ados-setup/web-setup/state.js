// Singleton store + polling helper for the dashboard.
//
// The store is a tiny pub/sub on a single state object. Subscribers fire
// after each set(). Polling wraps fetch with a configurable interval so
// we can run at 5 Hz when the dashboard is foregrounded and 1 Hz in the
// background.

const SETUP_TOKEN_KEY = "ados.setup.token";

export class Store {
  constructor(initial) {
    this.state = initial;
    this.subs = new Set();
  }

  get() {
    return this.state;
  }

  set(partial) {
    this.state = { ...this.state, ...partial };
    for (const fn of this.subs) {
      try {
        fn(this.state);
      } catch (err) {
        console.warn("store subscriber failed", err);
      }
    }
  }

  subscribe(fn) {
    this.subs.add(fn);
    return () => this.subs.delete(fn);
  }

  unsubscribe(fn) {
    this.subs.delete(fn);
  }
}

export class Polling {
  constructor({ url, intervalMs, store, key, hiddenIntervalMs }) {
    this.url = url;
    this.intervalMs = intervalMs;
    this.hiddenIntervalMs = hiddenIntervalMs ?? Math.max(intervalMs * 5, 30000);
    this.store = store;
    this.key = key;
    this.timer = null;
    this.inflight = false;
    this.disposed = false;
    this._tick = this._tick.bind(this);
    this._onVisibility = this._onVisibility.bind(this);
    document.addEventListener("visibilitychange", this._onVisibility);
  }

  start() {
    if (this.timer != null || this.disposed) return;
    this._tick();
  }

  stop() {
    if (this.timer != null) {
      clearTimeout(this.timer);
      this.timer = null;
    }
  }

  setRate(intervalMs) {
    this.intervalMs = intervalMs;
    if (this.timer != null) {
      this.stop();
      this.start();
    }
  }

  dispose() {
    this.disposed = true;
    this.stop();
    document.removeEventListener("visibilitychange", this._onVisibility);
  }

  _currentInterval() {
    return document.hidden ? this.hiddenIntervalMs : this.intervalMs;
  }

  _onVisibility() {
    if (this.disposed) return;
    if (this.timer != null) {
      this.stop();
      this.start();
    }
  }

  async _tick() {
    if (this.disposed) return;
    if (this.store.get().paused) {
      this.timer = setTimeout(this._tick, this._currentInterval());
      return;
    }
    if (this.inflight) {
      this.timer = setTimeout(this._tick, this._currentInterval());
      return;
    }
    this.inflight = true;
    try {
      const data = await apiFetch(this.url);
      this.store.set({ [this.key]: data, lastPollAt: Date.now(), lastPollError: null });
    } catch (err) {
      this.store.set({ lastPollError: err.message || String(err), lastPollAt: Date.now() });
    } finally {
      this.inflight = false;
      if (!this.disposed) {
        this.timer = setTimeout(this._tick, this._currentInterval());
      }
    }
  }
}

export async function apiFetch(path, init = {}) {
  const headers = new Headers(init.headers || {});
  const token = sessionStorage.getItem(SETUP_TOKEN_KEY);
  if (token) headers.set("X-ADOS-Setup-Token", token);
  if (init.body && !headers.has("Content-Type")) headers.set("Content-Type", "application/json");
  const res = await fetch(path, { ...init, headers });
  const ct = res.headers.get("content-type") || "";
  const body = ct.includes("application/json") ? await res.json() : await res.text();
  if (!res.ok) {
    const msg = (body && typeof body === "object" && body.error) || res.statusText || `HTTP ${res.status}`;
    const err = new Error(msg);
    err.status = res.status;
    err.body = body;
    throw err;
  }
  return body;
}

const initial = {
  status: null,
  theme: localStorage.getItem("ados.theme") || "auto",
  density: localStorage.getItem("ados.density") || "regular",
  paused: false,
  focusedPanel: null,
  lastPollAt: null,
  lastPollError: null,
};

export const store = new Store(initial);

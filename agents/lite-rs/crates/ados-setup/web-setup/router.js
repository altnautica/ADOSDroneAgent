// Tiny History-API client router. No dependencies.
//
// Patterns support :param segments. Match against location.pathname.
// Handlers are called with (params, query) and should mount themselves
// into a target element decided by the caller.

function compile(pattern) {
  const keys = [];
  const regex = new RegExp(
    "^" +
      pattern.replace(/:[A-Za-z_][A-Za-z0-9_]*/g, (m) => {
        keys.push(m.slice(1));
        return "([^/]+)";
      }).replace(/\/$/, "") +
      "/?$"
  );
  return { regex, keys };
}

export class Router {
  constructor() {
    this.routes = [];
    this.fallback = null;
    this.last = null;
    this._onPop = this._onPop.bind(this);
  }

  route(pattern, handler) {
    if (pattern === "*") {
      this.fallback = handler;
      return this;
    }
    const { regex, keys } = compile(pattern);
    this.routes.push({ pattern, regex, keys, handler });
    return this;
  }

  start() {
    window.addEventListener("popstate", this._onPop);
    this._resolve();
  }

  stop() {
    window.removeEventListener("popstate", this._onPop);
  }

  navigate(path, options = {}) {
    if (options.replace) history.replaceState({}, "", path);
    else history.pushState({}, "", path);
    this._resolve();
  }

  currentPath() {
    return this.last;
  }

  _onPop() {
    this._resolve();
  }

  _resolve() {
    const path = location.pathname || "/";
    const query = Object.fromEntries(new URLSearchParams(location.search));
    const safeCall = (handler, params) => {
      try {
        handler(params, query);
      } catch (err) {
        console.error("router handler failed", path, err);
      }
    };
    for (const r of this.routes) {
      const m = r.regex.exec(path);
      if (!m) continue;
      const params = {};
      r.keys.forEach((k, i) => {
        params[k] = decodeURIComponent(m[i + 1]);
      });
      this.last = { pattern: r.pattern, path, params, query };
      safeCall(r.handler, params);
      return;
    }
    if (this.fallback) {
      this.last = { pattern: "*", path, params: {}, query };
      safeCall(this.fallback, {});
    }
  }
}

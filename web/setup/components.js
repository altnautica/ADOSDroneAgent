// Reusable wizard primitives. Framework-free DOM helpers shared by every
// step renderer. The chip vocabulary mirrors Mission Control's badge tokens
// so the two surfaces feel like one product.

export function el(tag, props, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props || {})) {
    if (value == null || value === false) continue;
    if (key === "className") node.className = value;
    else if (key === "text") node.textContent = String(value);
    else if (key === "dataset") Object.assign(node.dataset, value);
    else if (key === "style") Object.assign(node.style, value);
    else if (key.startsWith("on") && typeof value === "function") {
      node.addEventListener(key.slice(2).toLowerCase(), value);
    } else if (key.startsWith("aria-") || key === "role" || key === "for" || key === "title") {
      node.setAttribute(key, String(value));
    } else {
      node[key] = value;
    }
  }
  for (const child of children.flat()) {
    if (child == null || child === false) continue;
    if (typeof child === "string" || typeof child === "number") {
      node.appendChild(document.createTextNode(String(child)));
    } else {
      node.appendChild(child);
    }
  }
  return node;
}

// ---------------------------------------------------------------------------
// Chip / Pill / Dot
// ---------------------------------------------------------------------------

const CHIP_VARIANTS = new Set(["ok", "warn", "err", "info", "muted", "accent"]);

export function chip(opts) {
  const variant = CHIP_VARIANTS.has(opts.variant) ? opts.variant : "muted";
  const classes = ["chip", `chip--${variant}`];
  if (opts.dot) classes.push("chip--with-dot");
  if (opts.size === "sm") classes.push("chip--sm");
  const node = el("span", { className: classes.join(" "), title: opts.title || null });
  if (opts.dot) {
    node.appendChild(el("span", { className: `chip-dot ${opts.pulse ? "chip-dot--pulse" : ""}`.trim(), "aria-hidden": "true" }));
  }
  if (opts.icon) {
    node.appendChild(el("span", { className: "chip-icon", "aria-hidden": "true", text: opts.icon }));
  }
  node.appendChild(el("span", { className: "chip-label", text: opts.label || "" }));
  return node;
}

export function statusDot(status, pulse = false) {
  const variant = CHIP_VARIANTS.has(status) ? status : "muted";
  return el("span", {
    className: `status-dot status-dot--${variant} ${pulse ? "status-dot--pulse" : ""}`.trim(),
    "aria-hidden": "true",
  });
}

// Inline label + chip-row pair. Used for live signal rows on welcome / profile.
export function liveRow(opts) {
  const right = el("div", { className: "live-row-chips" });
  for (const c of opts.chips || []) {
    if (c instanceof Node) right.appendChild(c);
    else if (c) right.appendChild(chip(c));
  }
  return el("div", { className: "live-row" },
    el("span", { className: "live-row-label", text: opts.label || "" }),
    right,
    opts.hint ? el("p", { className: "live-row-hint", text: opts.hint }) : null,
  );
}

// ---------------------------------------------------------------------------
// Verify button
// ---------------------------------------------------------------------------

// One-shot verification button. Becomes a chip after the request resolves.
export function verifyButton(opts) {
  const wrap = el("div", { className: "verify-button-wrap" });
  let inFlight = false;
  const renderIdle = () => {
    wrap.replaceChildren(
      el("button", {
        type: "button",
        className: "btn",
        disabled: inFlight,
        onclick: async () => {
          if (inFlight) return;
          inFlight = true;
          wrap.replaceChildren(chip({ variant: "info", dot: true, pulse: true, label: opts.busyLabel || "Checking…" }));
          const start = performance.now();
          try {
            const res = await fetch(opts.endpoint, { method: opts.method || "GET" });
            const ms = Math.round(performance.now() - start);
            const body = res.headers.get("content-type")?.includes("application/json") ? await res.json() : null;
            opts.onResult?.(body, res.ok);
            if (!res.ok) {
              wrap.replaceChildren(
                chip({ variant: "err", dot: true, label: body?.error || `Failed (${res.status})` }),
                el("button", { type: "button", className: "btn btn--ghost", text: "Retry", onclick: () => { inFlight = false; renderIdle(); } }),
              );
            } else {
              const ok = body?.reachable !== false;
              wrap.replaceChildren(
                chip({
                  variant: ok ? "ok" : "warn",
                  dot: true,
                  label: ok ? `${opts.successLabel || "OK"} · ${body?.latency_ms ?? ms}ms` : (body?.error || "Unreachable"),
                }),
                el("button", { type: "button", className: "btn btn--ghost", text: "Re-check", onclick: () => { inFlight = false; renderIdle(); } }),
              );
            }
          } catch (err) {
            wrap.replaceChildren(
              chip({ variant: "err", dot: true, label: err.message || "Network error" }),
              el("button", { type: "button", className: "btn btn--ghost", text: "Retry", onclick: () => { inFlight = false; renderIdle(); } }),
            );
          } finally {
            inFlight = false;
          }
        },
        text: opts.label || "Verify",
      }),
    );
  };
  renderIdle();
  return wrap;
}

// ---------------------------------------------------------------------------
// Streaming console
// ---------------------------------------------------------------------------

const ANSI_RE = /\x1b\[[0-9;]*m/g;
const MAX_LINES = 1000;

// Auto-reconnecting WebSocket-backed log tail. Strips ANSI, virtualizes
// after MAX_LINES so the DOM never grows unbounded. Returns a node with
// a `.dispose()` method the wizard calls when leaving the step.
export function streamConsole(opts) {
  const url = opts.wsUrl;
  const height = opts.height || 240;
  const lines = el("div", { className: "stream-console-lines" });
  const status = el("div", { className: "stream-console-status" });
  const root = el("div", { className: "stream-console", style: { maxHeight: `${height}px` } }, lines);
  const wrap = el("div", { className: "stream-console-wrap" }, status, root);

  let socket = null;
  let attempts = 0;
  let stickToBottom = true;
  let disposed = false;
  let reconnectTimer = null;

  const setStatus = (label, variant) => {
    status.replaceChildren(chip({ variant: variant || "muted", dot: true, pulse: variant === "info", label, size: "sm" }));
  };

  const append = (line) => {
    if (!line) return;
    const text = String(line).replace(ANSI_RE, "");
    const node = el("div", { className: "stream-console-line", text });
    lines.appendChild(node);
    while (lines.childElementCount > MAX_LINES) lines.removeChild(lines.firstChild);
    if (stickToBottom) root.scrollTop = root.scrollHeight;
  };

  root.addEventListener("scroll", () => {
    const atBottom = root.scrollHeight - root.scrollTop - root.clientHeight < 40;
    stickToBottom = atBottom;
  });

  const connect = () => {
    if (disposed) return;
    setStatus("Connecting…", "info");
    try {
      const wsUrl = url.startsWith("ws") ? url : `${location.protocol === "https:" ? "wss:" : "ws:"}//${location.host}${url}`;
      socket = new WebSocket(wsUrl);
      // Default browser binaryType is "blob"; the MAVLink proxy on port
      // 8765 sends raw frames as binary, and our parser only handles
      // ArrayBuffer / Uint8Array. Without this, every binary frame is
      // dropped silently. Text frames (e.g. cloudflared journal lines)
      // ignore binaryType, so this is safe for both consumers.
      socket.binaryType = "arraybuffer";
    } catch (err) {
      setStatus(`Connect failed: ${err.message || err}`, "err");
      scheduleReconnect();
      return;
    }
    socket.onopen = () => {
      attempts = 0;
      setStatus("Live", "ok");
    };
    socket.onmessage = (ev) => {
      if (opts.parser) {
        try {
          const out = opts.parser(ev.data);
          if (out != null) append(out);
        } catch { /* ignore parse errors */ }
      } else if (typeof ev.data === "string") {
        append(ev.data);
      } else {
        // ignore binary unless a parser was provided
      }
    };
    socket.onerror = () => {
      setStatus("Connection error", "warn");
    };
    socket.onclose = () => {
      socket = null;
      if (disposed) return;
      setStatus("Disconnected", "muted");
      scheduleReconnect();
    };
  };

  const scheduleReconnect = () => {
    if (disposed) return;
    attempts += 1;
    const delay = Math.min(15000, 500 * 2 ** Math.min(attempts, 5)) + Math.random() * 250;
    reconnectTimer = setTimeout(connect, delay);
  };

  wrap.dispose = () => {
    disposed = true;
    if (reconnectTimer) clearTimeout(reconnectTimer);
    if (socket) {
      try { socket.close(); } catch { /* ignore */ }
      socket = null;
    }
  };

  connect();
  return wrap;
}

// ---------------------------------------------------------------------------
// MAVLink mini-parser (HEARTBEAT, SYS_STATUS, GPS_RAW_INT, ATTITUDE,
// AUTOPILOT_VERSION). Used by the wizard's flight-controller step so the
// agent does not need to ship a Python MAVLink parser. The four message
// payloads are read directly off the wire frame from the existing
// `ws://host:8765` proxy.
// ---------------------------------------------------------------------------

const MAVLINK_V2_STX = 0xFD;
const MAVLINK_V1_STX = 0xFE;

const MAV_MODE_FLAG_SAFETY_ARMED = 0x80;

const FIX_TYPE = ["No GPS", "No fix", "2D fix", "3D fix", "DGPS", "RTK Float", "RTK Fixed", "Static", "PPP"];

const AUTOPILOT_NAMES = {
  3: "ArduPilot",
  12: "PX4",
};

const VEHICLE_NAMES = {
  1: "Fixed wing",
  2: "Quadrotor",
  10: "Ground rover",
  13: "Hexacopter",
  14: "Octocopter",
  20: "Helicopter",
  21: "Submarine",
  22: "Coaxial",
};

// Map of capability flag bits we care about. AUTOPILOT_VERSION.capabilities
// is a uint64; we surface a few that matter for the wizard's "what does this
// FC speak" chip row.
const CAPABILITY_FLAGS = [
  { bit: 1, label: "MISSION_FLOAT" },
  { bit: 2, label: "PARAM_FLOAT" },
  { bit: 4, label: "MISSION_INT" },
  { bit: 8, label: "COMMAND_INT" },
  { bit: 16, label: "PARAM_UNION" },
  { bit: 32, label: "FTP" },
  { bit: 64, label: "SET_ATTITUDE_TARGET" },
  { bit: 128, label: "SET_POSITION_TARGET_LOCAL_NED" },
  { bit: 256, label: "SET_POSITION_TARGET_GLOBAL_INT" },
  { bit: 512, label: "TERRAIN" },
  { bit: 1024, label: "SET_ACTUATOR_TARGET" },
  { bit: 2048, label: "FLIGHT_TERMINATION" },
  { bit: 4096, label: "COMPASS_CALIBRATION" },
  { bit: 8192, label: "MAVLINK2" },
  { bit: 16384, label: "MISSION_FENCE" },
  { bit: 32768, label: "MISSION_RALLY" },
];

export function parseMavlinkFrame(buf) {
  // Accept ArrayBuffer or Uint8Array. Blob input is the caller's job.
  let bytes = buf instanceof Uint8Array ? buf : (buf instanceof ArrayBuffer ? new Uint8Array(buf) : null);
  if (!bytes || bytes.length < 8) return null;

  // Re-sync to a STX byte. The proxy may send partial frames; we just look
  // for v2 first, then v1, and decode the first valid frame in the buffer.
  let i = 0;
  while (i < bytes.length && bytes[i] !== MAVLINK_V2_STX && bytes[i] !== MAVLINK_V1_STX) i += 1;
  if (i >= bytes.length) return null;

  const v2 = bytes[i] === MAVLINK_V2_STX;
  if (v2) {
    if (bytes.length - i < 12) return null;
    const len = bytes[i + 1];
    const incompat = bytes[i + 2];
    const headerLen = 10;
    const sigLen = (incompat & 0x01) ? 13 : 0;
    if (bytes.length - i < headerLen + len + 2 + sigLen) return null;
    const msgId = bytes[i + 7] | (bytes[i + 8] << 8) | (bytes[i + 9] << 16);
    const payloadStart = i + headerLen;
    const payload = bytes.slice(payloadStart, payloadStart + len);
    return { msgId, payload, version: 2 };
  }
  if (bytes.length - i < 8) return null;
  const len = bytes[i + 1];
  if (bytes.length - i < 8 + len) return null;
  const msgId = bytes[i + 5];
  const payload = bytes.slice(i + 6, i + 6 + len);
  return { msgId, payload, version: 1 };
}

export function decodeMavlinkPayload(frame) {
  if (!frame) return null;
  const { msgId, payload } = frame;
  const dv = new DataView(payload.buffer, payload.byteOffset, payload.byteLength);
  const u32 = (off) => off + 4 <= payload.length ? dv.getUint32(off, true) : 0;
  const u16 = (off) => off + 2 <= payload.length ? dv.getUint16(off, true) : 0;
  const u8 = (off) => off < payload.length ? payload[off] : 0;
  const i32 = (off) => off + 4 <= payload.length ? dv.getInt32(off, true) : 0;
  const u64 = (off) => {
    if (off + 8 > payload.length) return 0n;
    return dv.getBigUint64(off, true);
  };

  switch (msgId) {
    case 0: { // HEARTBEAT
      const customMode = u32(0);
      const type = u8(4);
      const autopilot = u8(5);
      const baseMode = u8(6);
      const systemStatus = u8(7);
      return {
        type: "heartbeat",
        vehicle: VEHICLE_NAMES[type] || `type ${type}`,
        autopilot: AUTOPILOT_NAMES[autopilot] || `ap ${autopilot}`,
        armed: !!(baseMode & MAV_MODE_FLAG_SAFETY_ARMED),
        mode: customMode,
        systemStatus,
      };
    }
    case 1: { // SYS_STATUS
      return {
        type: "sys_status",
        voltage_v: (u16(14) || 0) / 1000,
        current_a: (dv.getInt16(16, true) || 0) / 100,
        battery_remaining: payload[30] !== undefined ? (payload[30] === 0xFF ? null : payload[30]) : null,
      };
    }
    case 24: { // GPS_RAW_INT
      const fix = u8(28);
      return {
        type: "gps",
        fix,
        fix_label: FIX_TYPE[fix] || `fix ${fix}`,
        lat: i32(8) / 1e7,
        lon: i32(12) / 1e7,
        sats: u8(29),
      };
    }
    case 30: { // ATTITUDE
      const f32 = (off) => off + 4 <= payload.length ? dv.getFloat32(off, true) : 0;
      return {
        type: "attitude",
        roll: f32(4),
        pitch: f32(8),
        yaw: f32(12),
      };
    }
    case 148: { // AUTOPILOT_VERSION
      const caps = u64(0);
      const flightSwVersion = u32(8);
      const supported = [];
      for (const c of CAPABILITY_FLAGS) {
        if (caps & BigInt(c.bit)) supported.push(c.label);
      }
      return {
        type: "autopilot_version",
        capabilities: caps.toString(),
        supported,
        flight_sw_version: flightSwVersion,
      };
    }
    default:
      return { type: "other", msgId };
  }
}

// ---------------------------------------------------------------------------
// Dashboard primitives (panel, statTile, sparkline, sheet, contextMenu, ...)
// ---------------------------------------------------------------------------

export function cn(...args) {
  return args.filter(Boolean).join(" ");
}

export function clamp(v, min, max) {
  return Math.max(min, Math.min(max, v));
}

export function debounce(fn, ms) {
  let t = null;
  return (...args) => {
    if (t != null) clearTimeout(t);
    t = setTimeout(() => fn(...args), ms);
  };
}

export function formatRelative(iso) {
  if (!iso) return "never";
  const t = typeof iso === "number" ? iso : Date.parse(iso);
  if (Number.isNaN(t)) return "never";
  const s = Math.max(0, Math.floor((Date.now() - t) / 1000));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

export function formatRate(hz) {
  if (hz == null || Number.isNaN(hz)) return "-";
  if (hz >= 100) return `${hz.toFixed(0)} Hz`;
  if (hz >= 10) return `${hz.toFixed(1)} Hz`;
  return `${hz.toFixed(2)} Hz`;
}

export async function copyText(value) {
  try {
    await navigator.clipboard.writeText(String(value));
    if (navigator.vibrate) {
      try { navigator.vibrate(10); } catch { /* noop */ }
    }
    toast({ message: "copied", severity: "ok", ttlMs: 1200 });
    return true;
  } catch (err) {
    toast({ message: `copy failed: ${err.message || err}`, severity: "err", ttlMs: 2000 });
    return false;
  }
}

// Panel chrome. body and footer can be a Node, an array of Nodes, or null.
export function panel({ title, span, expandable, body, footer, actions, severity, id }) {
  const head = el("header", { className: "panel-head" });
  if (severity) head.appendChild(statusDot(severity));
  head.appendChild(el("h2", { className: "panel-title", text: title || "" }));
  if (actions) {
    const act = el("div", { className: "panel-actions" });
    for (const a of [].concat(actions)) {
      if (a instanceof Node) act.appendChild(a);
    }
    head.appendChild(act);
  }
  if (expandable) {
    head.appendChild(el("button", {
      type: "button",
      className: "panel-expand",
      "aria-label": "toggle panel",
      text: "[+]",
      onclick: (ev) => {
        const root = ev.currentTarget.closest(".panel");
        if (root) root.classList.toggle("panel--collapsed");
      },
    }));
  }

  const bodyEl = el("div", { className: "panel-body" });
  appendChildren(bodyEl, body);

  const node = el("section", {
    className: cn("panel", span ? `panel--span-${span}` : null),
    id: id || null,
  }, head, bodyEl);

  if (footer) {
    const foot = el("footer", { className: "panel-foot" });
    appendChildren(foot, footer);
    node.appendChild(foot);
  }
  return node;
}

function appendChildren(host, children) {
  if (children == null) return;
  for (const c of [].concat(children)) {
    if (c == null || c === false) continue;
    if (typeof c === "string" || typeof c === "number") {
      host.appendChild(document.createTextNode(String(c)));
    } else if (c instanceof Node) {
      host.appendChild(c);
    }
  }
}

// Stat tile with mono value, sparkline, severity dot, optional hotkey label.
export function statTile({ label, value, sparkPoints, severity, hotkey, sub }) {
  const head = el("div", { className: "stat-tile-head" });
  head.appendChild(el("span", { className: "stat-tile-label", text: label || "" }));
  if (hotkey) head.appendChild(el("kbd", { className: "stat-tile-key", text: hotkey }));
  if (severity) head.appendChild(statusDot(severity));

  const valueEl = el("div", { className: "stat-tile-value mono", text: value != null ? String(value) : "-" });
  const subEl = sub ? el("div", { className: "stat-tile-sub mono", text: String(sub) }) : null;

  const sparkEl = sparkline(sparkPoints || [], { width: 96, height: 22 });

  return el("button", {
    type: "button",
    className: cn("stat-tile", severity ? `stat-tile--${severity}` : null),
  }, head, valueEl, subEl, sparkEl);
}

// Inline SVG sparkline. Accepts up to 60 points.
export function sparkline(points, opts = {}) {
  const width = opts.width || 96;
  const height = opts.height || 22;
  const stroke = opts.stroke || "currentColor";
  const fill = opts.fill || "none";

  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("class", "sparkline");
  svg.setAttribute("width", width);
  svg.setAttribute("height", height);
  svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
  svg.setAttribute("aria-hidden", "true");

  const arr = Array.isArray(points) ? points.slice(-60) : [];
  if (arr.length < 2) return svg;

  const min = Math.min(...arr);
  const max = Math.max(...arr);
  const span = max - min || 1;
  const step = width / (arr.length - 1);

  let d = "";
  arr.forEach((v, i) => {
    const x = i * step;
    const y = height - ((v - min) / span) * (height - 2) - 1;
    d += (i === 0 ? "M" : "L") + x.toFixed(1) + "," + y.toFixed(1);
  });

  const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
  path.setAttribute("d", d);
  path.setAttribute("stroke", stroke);
  path.setAttribute("fill", fill);
  path.setAttribute("stroke-width", "1");
  path.setAttribute("stroke-linecap", "round");
  path.setAttribute("stroke-linejoin", "round");
  svg.appendChild(path);
  return svg;
}

// Sheet primitive. Full-screen on mobile, modal on desktop. Esc to close.
// Returns { node, close, setBody }.
export function sheet({ title, body, footer, onDismiss, dismissable }) {
  const close = () => {
    if (onDismiss) onDismiss();
    document.removeEventListener("keydown", onKey, true);
    if (node.parentNode) node.parentNode.removeChild(node);
  };
  const onKey = (ev) => {
    if (ev.key === "Escape" && dismissable !== false) {
      ev.preventDefault();
      close();
      return;
    }
    if (ev.key === "Tab" && node) {
      const focusables = node.querySelectorAll(
        'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])'
      );
      if (!focusables.length) {
        ev.preventDefault();
        return;
      }
      const first = focusables[0];
      const last = focusables[focusables.length - 1];
      const activeEl = document.activeElement;
      if (ev.shiftKey && activeEl === first) {
        ev.preventDefault();
        last.focus();
      } else if (!ev.shiftKey && activeEl === last) {
        ev.preventDefault();
        first.focus();
      }
    }
  };

  const head = el("header", { className: "sheet-head" },
    el("h2", { className: "sheet-title", text: title || "" }),
    dismissable === false ? null : el("button", {
      type: "button",
      className: "sheet-close",
      "aria-label": "close",
      text: "esc",
      onclick: close,
    }),
  );

  const bodyEl = el("div", { className: "sheet-body" });
  appendChildren(bodyEl, body);

  const inner = el("div", { className: "sheet-inner" }, head, bodyEl);
  if (footer) {
    const foot = el("footer", { className: "sheet-foot" });
    appendChildren(foot, footer);
    inner.appendChild(foot);
  }

  const node = el("div", {
    className: "sheet-host",
    role: "dialog",
    "aria-modal": "true",
    onclick: (ev) => {
      if (ev.target === node && dismissable !== false) close();
    },
  }, inner);

  document.addEventListener("keydown", onKey, true);
  document.body.appendChild(node);

  // Move focus into the sheet for focus-trap-lite.
  queueMicrotask(() => {
    const focusable = inner.querySelector("button,[href],input,select,textarea,[tabindex]");
    if (focusable) focusable.focus();
  });

  const setBody = (next) => {
    bodyEl.replaceChildren();
    appendChildren(bodyEl, next);
  };

  return { node, close, setBody };
}

// Toast strip lives at the top. Singleton host is created lazily.
let toastHost = null;

export function setToastHost(host) {
  toastHost = host;
}

export function toast({ message, severity, ttlMs }) {
  if (!toastHost) {
    toastHost = el("div", { className: "toast-host", role: "status", "aria-live": "polite" });
    document.body.appendChild(toastHost);
  }
  const node = el("div", {
    className: cn("toast", severity ? `toast--${severity}` : null),
  }, statusDot(severity || "info"), el("span", { className: "toast-msg", text: String(message || "") }));
  toastHost.appendChild(node);
  const ttl = ttlMs || 4000;
  setTimeout(() => node.classList.add("toast--leave"), ttl - 200);
  setTimeout(() => {
    if (node.parentNode) node.parentNode.removeChild(node);
  }, ttl);
}

// Anchored popover with arrow-key + Esc support.
// items: [{ label, hotkey, onSelect, severity }]
export function contextMenu(target, items, opts = {}) {
  const list = el("ul", { className: "context-menu", role: "menu" });
  let active = 0;

  const close = () => {
    document.removeEventListener("keydown", onKey, true);
    document.removeEventListener("pointerdown", onPointer, true);
    if (list.parentNode) list.parentNode.removeChild(list);
    if (opts.onClose) opts.onClose();
  };

  const select = (i) => {
    const item = items[i];
    if (!item) return;
    close();
    try { item.onSelect && item.onSelect(); } catch (err) { console.warn(err); }
  };

  const onKey = (ev) => {
    if (ev.key === "Escape") { ev.preventDefault(); close(); return; }
    if (ev.key === "ArrowDown") { ev.preventDefault(); active = (active + 1) % items.length; render(); return; }
    if (ev.key === "ArrowUp") { ev.preventDefault(); active = (active - 1 + items.length) % items.length; render(); return; }
    if (ev.key === "Enter") { ev.preventDefault(); select(active); return; }
  };

  const onPointer = (ev) => {
    if (!list.contains(ev.target)) close();
  };

  const render = () => {
    list.replaceChildren();
    items.forEach((item, i) => {
      const li = el("li", {
        className: cn("context-menu-item", i === active ? "is-active" : null, item.severity ? `is-${item.severity}` : null),
        role: "menuitem",
        onclick: () => select(i),
        onmouseenter: () => { active = i; render(); },
      },
        el("span", { className: "context-menu-label", text: item.label || "" }),
        item.hotkey ? el("kbd", { className: "context-menu-key", text: item.hotkey }) : null,
      );
      list.appendChild(li);
    });
  };
  render();

  // Anchor to target.
  const rect = target.getBoundingClientRect();
  list.style.position = "fixed";
  list.style.top = `${Math.round(rect.bottom + 4)}px`;
  list.style.left = `${Math.round(rect.left)}px`;
  document.body.appendChild(list);

  document.addEventListener("keydown", onKey, true);
  // Defer pointer listener so the click that opened the menu doesn't close it.
  setTimeout(() => document.addEventListener("pointerdown", onPointer, true), 0);

  return { close };
}


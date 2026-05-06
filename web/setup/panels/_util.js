// Internal helpers shared by every dashboard panel.
//
// Ring buffer caps a numeric series at 60 samples for the at-a-glance
// 60-second sparkline. Severity helpers normalize the five status colors
// so each panel's body lookups stay terse. Render helpers wrap defensive
// pulls from a possibly-null snapshot so panels never throw on first
// render (state.dashboard arrives a tick later than state.status).

export function createRingBuffer(cap = 60) {
  const buf = [];
  return {
    push(v) {
      if (v == null || Number.isNaN(v)) return;
      buf.push(Number(v));
      while (buf.length > cap) buf.shift();
    },
    points() {
      return buf.slice();
    },
    last() {
      return buf.length ? buf[buf.length - 1] : null;
    },
    clear() {
      buf.length = 0;
    },
  };
}

const SEVERITIES = new Set(["ok", "warn", "err", "idle", "info"]);

export function severityFromState(state) {
  if (!state) return "idle";
  const s = String(state).toLowerCase();
  if (SEVERITIES.has(s)) return s;
  if (s === "active" || s === "running" || s === "healthy" || s === "good" || s === "connected" || s === "up") return "ok";
  if (s === "degraded" || s === "warning" || s === "stale" || s === "slow") return "warn";
  if (s === "failed" || s === "fault" || s === "down" || s === "error" || s === "disconnected") return "err";
  if (s === "off" || s === "absent" || s === "unconfigured" || s === "stopped" || s === "inactive") return "idle";
  return "idle";
}

export function pick(obj, path, fallback) {
  if (!obj) return fallback;
  const parts = String(path).split(".");
  let cur = obj;
  for (const p of parts) {
    if (cur == null) return fallback;
    cur = cur[p];
  }
  return cur == null ? fallback : cur;
}

export function fmtNum(v, digits = 1, suffix = "") {
  if (v == null || Number.isNaN(v)) return "-";
  const n = Number(v);
  if (!Number.isFinite(n)) return "-";
  if (n >= 1000 && digits <= 2) return `${(n / 1000).toFixed(1)}k${suffix}`;
  return `${n.toFixed(digits)}${suffix}`;
}

export function fmtBytes(bytes) {
  if (bytes == null || Number.isNaN(bytes)) return "-";
  const b = Number(bytes);
  if (b < 1024) return `${b} B`;
  if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)} KB`;
  if (b < 1024 * 1024 * 1024) return `${(b / 1024 / 1024).toFixed(1)} MB`;
  return `${(b / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

export function fmtBitrate(kbps) {
  if (kbps == null || Number.isNaN(kbps)) return "-";
  const k = Number(kbps);
  if (k >= 1000) return `${(k / 1000).toFixed(1)} Mbps`;
  return `${k.toFixed(0)} kbps`;
}

export function fmtDur(seconds) {
  if (seconds == null || Number.isNaN(seconds)) return "-";
  const s = Math.max(0, Math.floor(Number(seconds)));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h${Math.floor((s % 3600) / 60)}m`;
  return `${Math.floor(s / 86400)}d${Math.floor((s % 86400) / 3600)}h`;
}

export function safeArr(v) {
  return Array.isArray(v) ? v : [];
}

export function safeObj(v) {
  return v && typeof v === "object" ? v : {};
}

// Mask a pairing code or token. ABCD-1234 -> ABCD-····
export function maskCode(code) {
  if (!code) return "-";
  const s = String(code);
  if (s.length <= 4) return "····";
  return `${s.slice(0, 4)}-····`;
}

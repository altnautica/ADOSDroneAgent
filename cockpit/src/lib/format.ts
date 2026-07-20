// Small display formatters for the status strip + HUD. Every one is
// null-safe: an absent reading renders an em-dash placeholder rather than a
// misleading zero (a surface never fabricates a value it lacks).

const DASH = "—";

export function fmtDbm(v: number | null | undefined): string {
  return v == null ? DASH : `${Math.round(v)} dBm`;
}

export function fmtTemp(v: number | null | undefined): string {
  return v == null ? DASH : `${Math.round(v)}°C`;
}

export function fmtPct(v: number | null | undefined): string {
  return v == null ? DASH : `${Math.round(v)}%`;
}

export function fmtChannel(v: number | null | undefined): string {
  return v == null || v === 0 ? DASH : `ch${v}`;
}

export function fmtMbps(v: number | null | undefined): string {
  return v == null ? DASH : `${v.toFixed(1)} Mbps`;
}

/** Seconds → a compact "1d 2h", "3h 4m", "5m 6s" uptime string. */
export function fmtUptime(seconds: number | null | undefined): string {
  if (seconds == null || seconds < 0) return DASH;
  const s = Math.floor(seconds);
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${sec}s`;
  return `${sec}s`;
}

export { DASH };

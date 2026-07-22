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

/** A compass heading in degrees → "045°". Guards the MAVLink "unknown"
 *  sentinel (hdg 65535 → 655.35) and any out-of-range value. */
export function fmtHeading(v: number | null | undefined): string {
  if (v == null || !Number.isFinite(v) || v < 0 || v > 360) return DASH;
  return `${String(Math.round(v) % 360).padStart(3, "0")}°`;
}

/** Metres, rounded, with a "m" suffix. */
export function fmtMeters(v: number | null | undefined): string {
  return v == null || !Number.isFinite(v) ? DASH : `${Math.round(v)} m`;
}

/** A plain rounded integer, or a dash. Used for the tape readouts + sat count. */
export function fmtInt(v: number | null | undefined): string {
  return v == null || !Number.isFinite(v) ? DASH : `${Math.round(v)}`;
}

/** A GPS satellite count, guarding the "unknown" (255) sentinel. */
export function fmtSats(v: number | null | undefined): string {
  return v == null || !Number.isFinite(v) || v > 200 ? DASH : `${Math.round(v)}`;
}

/** A GPS fix-type code → a short label (MAVLink GPS_FIX_TYPE). An unknown code
 *  reads a dash rather than a fabricated fix quality. */
export function fmtGpsFix(v: number | null | undefined): string {
  switch (v) {
    case 0:
    case 1:
      return "No fix";
    case 2:
      return "2D";
    case 3:
      return "3D";
    case 4:
      return "DGPS";
    case 5:
      return "RTK flt";
    case 6:
      return "RTK fix";
    default:
      return DASH;
  }
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

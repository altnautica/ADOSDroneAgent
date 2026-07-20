// Extra display formatters the status screens need, in the same null-safe house
// style as `lib/format.ts` (an absent reading renders an em-dash, never a
// fabricated zero). Kept separate so the status screens grow their own
// formatting vocabulary without churning the shared HUD formatter module.

import { DASH } from "@/lib/format";

/** A plain decibel reading (SNR) → "12 dB". */
export function fmtDb(v: number | null | undefined): string {
  return v == null || !Number.isFinite(v) ? DASH : `${Math.round(v)} dB`;
}

/** A 5 GHz centre frequency in MHz → "5745 MHz". */
export function fmtMhz(v: number | null | undefined): string {
  return v == null || !Number.isFinite(v) ? DASH : `${Math.round(v)} MHz`;
}

/** A megabit-per-second rate carried as kbps → "12.0 Mbps". */
export function fmtKbpsAsMbps(v: number | null | undefined): string {
  return v == null || !Number.isFinite(v) ? DASH : `${(v / 1000).toFixed(1)} Mbps`;
}

/** A loss/percentage reading → "3.2%". */
export function fmtLossPct(v: number | null | undefined): string {
  return v == null || !Number.isFinite(v) ? DASH : `${v.toFixed(1)}%`;
}

/** Gigabytes, one decimal → "12.4 GB". */
export function fmtGb(v: number | null | undefined): string {
  return v == null || !Number.isFinite(v) ? DASH : `${v.toFixed(1)} GB`;
}

/** Megabytes, rounded → "512 MB". */
export function fmtMb(v: number | null | undefined): string {
  return v == null || !Number.isFinite(v) ? DASH : `${Math.round(v)} MB`;
}

/** A raw byte count → a human MB/GB string ("1.4 GB", "512 MB", "48 KB"). */
export function fmtBytes(v: number | null | undefined): string {
  if (v == null || !Number.isFinite(v) || v < 0) return DASH;
  if (v >= 1e9) return `${(v / 1e9).toFixed(1)} GB`;
  if (v >= 1e6) return `${(v / 1e6).toFixed(1)} MB`;
  if (v >= 1e3) return `${Math.round(v / 1e3)} KB`;
  return `${Math.round(v)} B`;
}

/** A Unix-seconds timestamp → a short local time "14:32". */
export function fmtClock(unixSeconds: number | null | undefined): string {
  if (unixSeconds == null || !Number.isFinite(unixSeconds)) return DASH;
  const d = new Date(unixSeconds * 1000);
  if (Number.isNaN(d.getTime())) return DASH;
  return d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
}

/** Milliseconds-until (from a `closes_at_ms` epoch) → a compact "42s" countdown,
 *  or a dash when the window is not open / already elapsed. */
export function fmtCountdown(closesAtMs: number | null | undefined): string {
  if (closesAtMs == null || !Number.isFinite(closesAtMs)) return DASH;
  const remaining = Math.round((closesAtMs - Date.now()) / 1000);
  if (remaining <= 0) return "0s";
  if (remaining >= 60) return `${Math.floor(remaining / 60)}m ${remaining % 60}s`;
  return `${remaining}s`;
}

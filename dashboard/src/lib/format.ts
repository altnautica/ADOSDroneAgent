// Pure formatters and severity helpers shared by panels and tiles.

import type { Severity } from "./types";

export function fmtUptime(seconds: number | null | undefined): string {
  if (!seconds || seconds < 0) return "—";
  const d = Math.floor(seconds / 86400);
  const h = Math.floor((seconds % 86400) / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  if (d > 0) return `${d}d${String(h).padStart(2, "0")}h`;
  if (h > 0) return `${h}h${String(m).padStart(2, "0")}m`;
  return `${m}m`;
}

export function fmtBitrate(kbps: number | null | undefined): string {
  if (kbps == null || kbps <= 0) return "—";
  if (kbps >= 1000) return `${(kbps / 1000).toFixed(1)} Mbps`;
  return `${kbps.toFixed(0)} kbps`;
}

export function fmtNum(
  v: number | null | undefined,
  digits = 1,
): string {
  if (v == null || Number.isNaN(v)) return "—";
  return v.toFixed(digits);
}

export function fmtVoltage(v: number | null | undefined): string {
  if (v == null) return "—";
  return `${v.toFixed(2)} V`;
}

export function fmtPercent(v: number | null | undefined): string {
  if (v == null) return "—";
  return `${Math.round(v)}%`;
}

export function fmtRssi(dbm: number | null | undefined): string {
  if (dbm == null) return "—";
  return `${dbm} dBm`;
}

export function severityFromState(state: string | null | undefined): Severity {
  if (!state) return "idle";
  const s = state.toLowerCase();
  if (s === "running" || s === "active" || s === "online" || s === "ok") return "ok";
  if (s === "degraded" || s === "warn" || s === "warning") return "warn";
  if (s === "failed" || s === "error" || s === "err" || s === "offline") return "err";
  if (s === "unknown" || s === "idle") return "idle";
  return "info";
}

export function severityClasses(sev: Severity): {
  text: string;
  dot: string;
  bg: string;
} {
  switch (sev) {
    case "ok":
      return {
        text: "text-ok",
        dot: "bg-ok",
        bg: "bg-ok/10",
      };
    case "warn":
      return {
        text: "text-warn",
        dot: "bg-warn",
        bg: "bg-warn/10",
      };
    case "err":
      return {
        text: "text-destructive",
        dot: "bg-destructive",
        bg: "bg-destructive/10",
      };
    case "info":
      return {
        text: "text-info",
        dot: "bg-info",
        bg: "bg-info/10",
      };
    case "idle":
    default:
      return {
        text: "text-muted-foreground",
        dot: "bg-muted-foreground",
        bg: "bg-muted/30",
      };
  }
}

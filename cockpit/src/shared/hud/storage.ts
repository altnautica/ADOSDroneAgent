// Storage use for the status pill, from `GET /api/status/full`. A drone
// reports it as `health.disk_percent`; a ground station as `system.disk_pct`.
// The cockpit and the dashboard both read it through here, so the pill's
// storage item comes from one source on every agent profile.

export interface StorageStatusFull {
  health?: { disk_percent?: number | null } | null;
  system?: { disk_pct?: number | null } | null;
}

const finite = (v: unknown): number | null => (typeof v === "number" && Number.isFinite(v) ? v : null);

/** Percent of storage used, or null when the agent reports none. */
export function storagePct(full: StorageStatusFull | null | undefined): number | null {
  return finite(full?.health?.disk_percent) ?? finite(full?.system?.disk_pct);
}

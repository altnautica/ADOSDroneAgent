// View model for the Extensions screen's catalog list (`GET
// /api/v1/plugins/catalog`) and the installed list (`GET /api/plugins`). Pure,
// so the error, update and permission-count rules are unit-testable; the
// screen turns the result kinds into words.

import type { RiskLevel } from "@/shared/api/plugin-install";

export interface CatalogEntry {
  id: string;
  name: string;
  version: string;
  description: string;
  author?: string;
  homepage?: string;
  license?: string;
  risk?: RiskLevel;
  category?: string;
  halves?: ("agent" | "gcs")[];
  download_url: string;
  archive_sha256?: string | null;
}

export interface CatalogResponse {
  schema_version: number;
  source: string;
  plugins: CatalogEntry[];
  /** Set when the bundled catalog failed to parse. */
  error?: string;
}

export type CatalogView =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "parseError"; message: string }
  | { kind: "empty" }
  | { kind: "list"; entries: CatalogEntry[]; parseError: string | null };

/** What the catalog list shows. A fetch failure or a parse error is shown,
 *  never silently dropped. */
export function catalogView(data: CatalogResponse | null, error: string | null, ready: boolean): CatalogView {
  if (error && !data) return { kind: "error", message: error };
  if (!data) return ready ? { kind: "empty" } : { kind: "loading" };
  const entries = Array.isArray(data.plugins) ? data.plugins : [];
  if (entries.length === 0) return data.error ? { kind: "parseError", message: data.error } : { kind: "empty" };
  return { kind: "list", entries, parseError: data.error ?? null };
}

export type InstallState = "absent" | "installed" | "update";

/** -1/0/1 for dotted numeric versions ("1.4.1" vs "1.10.0"); a pre-release or
 *  build suffix after `-`/`+` is ignored. */
export function compareVersions(a: string, b: string): number {
  const parts = (v: string) => v.split(/[-+]/)[0].split(".").map((n) => Number.parseInt(n, 10) || 0);
  const pa = parts(a);
  const pb = parts(b);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const d = (pa[i] ?? 0) - (pb[i] ?? 0);
    if (d !== 0) return d < 0 ? -1 : 1;
  }
  return 0;
}

/** Whether a catalog entry is absent, installed at (or above) the catalog
 *  version, or installed at an older version the catalog can update. */
export function installState(
  entry: Pick<CatalogEntry, "id" | "version">,
  installed: ReadonlyMap<string, string | undefined>,
): InstallState {
  if (!installed.has(entry.id)) return "absent";
  const have = installed.get(entry.id);
  return have && compareVersions(have, entry.version) < 0 ? "update" : "installed";
}

/** One row of `GET /api/plugins` `installs[]`. */
export interface InstallRow {
  pluginId: string;
  version: string | null;
  status: string | null;
  enabled: boolean;
  signed: boolean;
  granted: number;
  declared: number;
  /** Why the host failed to start it, when it did. */
  failureReason: string | null;
}

function isObj(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/** Parse the install list; rows without a plugin id are dropped. */
export function parseInstalls(body: unknown): InstallRow[] {
  const rows = isObj(body) && Array.isArray(body.installs) ? body.installs : [];
  return rows.flatMap((r: unknown): InstallRow[] => {
    if (!isObj(r) || typeof r.plugin_id !== "string" || !r.plugin_id) return [];
    const perms = isObj(r.permissions) ? Object.values(r.permissions) : [];
    const status = typeof r.status === "string" ? r.status : null;
    return [
      {
        pluginId: r.plugin_id,
        version: typeof r.version === "string" ? r.version : null,
        status,
        enabled: status === "enabled" || status === "running",
        signed: typeof r.signer_id === "string" && r.signer_id.length > 0,
        granted: perms.filter((p) => isObj(p) && p.granted === true).length,
        declared: perms.length,
        failureReason: typeof r.failure_reason === "string" && r.failure_reason ? r.failure_reason : null,
      },
    ];
  });
}

const STATUS_KEYS = {
  installed: "extensions.status.installed",
  enabled: "extensions.status.enabled",
  running: "extensions.status.running",
  disabled: "extensions.status.disabled",
  failed: "extensions.status.failed",
  incompatible: "extensions.status.incompatible",
} as const;

/** The label key for an install's status (unknown text reads as unknown). */
export function installStatusKey(status: string | null): (typeof STATUS_KEYS)[keyof typeof STATUS_KEYS] | "extensions.status.unknown" {
  return status && status in STATUS_KEYS ? STATUS_KEYS[status as keyof typeof STATUS_KEYS] : "extensions.status.unknown";
}

/** The status tone: a failed start is an error, an incompatible one a warning. */
export function installStatusTone(status: string | null): "ok" | "err" | "warn" | "muted" {
  if (status === "running") return "ok";
  if (status === "failed") return "err";
  if (status === "incompatible") return "warn";
  return "muted";
}

/** A URL the operator typed is usable when it is http(s); the agent applies
 *  its own allowlist on top. */
export function isInstallUrl(raw: string): boolean {
  try {
    const u = new URL(raw.trim());
    return u.protocol === "https:" || u.protocol === "http:";
  } catch {
    return false;
  }
}

/** A sha256 pin is optional; when given it must be 64 hex characters. */
export function normalizeSha256(raw: string): string | null | "invalid" {
  const s = raw.trim().toLowerCase();
  if (!s) return null;
  return /^[0-9a-f]{64}$/.test(s) ? s : "invalid";
}

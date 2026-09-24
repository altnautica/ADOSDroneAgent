// Plugin install orchestration. Two-stage flow, the same for an uploaded
// `.adosplug` and a first-party catalog entry:
//   1. parsePlugin(source)                - non-committing manifest preview
//   2. installPlugin(source, permissions) - install + grant the approved set
//
// Every call goes through `apiFetch`, so a paired node reached off-box gets the
// same credential the rest of the dashboard sends. If a required permission
// did not land, the caller disables the plugin so it never runs half-granted.

import { ApiError, apiFetch } from "@/lib/api";

export type RiskLevel = "low" | "medium" | "high" | "critical";

/** One declared permission, as the agent enriches it from its capability
 *  catalog (`label`, `description`, `risk`, `risk_reason`). */
export interface PluginPermission {
  id: string;
  required: boolean;
  label?: string;
  description?: string;
  risk?: RiskLevel;
  risk_reason?: string;
}

export interface PluginManifestSummary {
  ok: true;
  plugin_id: string;
  version: string;
  name: string;
  description?: string;
  author?: string;
  license?: string;
  risk: RiskLevel;
  signer_id: string | null;
  signed: boolean;
  halves: ("agent" | "gcs")[];
  permissions: PluginPermission[];
}

export interface PluginErrorEnvelope {
  ok: false;
  code: number;
  kind: string;
  detail: string;
}

/** Where an install comes from: an uploaded archive, or a catalog entry the
 *  agent downloads itself (always SHA-pinned). */
export type PluginSource =
  | { kind: "file"; file: File }
  | { kind: "catalog"; url: string; sha256: string | null };

export interface PluginInstallResult {
  ok: true;
  plugin_id: string;
  granted: string[];
}

function isEnvelope(body: unknown): body is PluginErrorEnvelope {
  return (
    !!body &&
    typeof body === "object" &&
    (body as { ok?: unknown }).ok === false &&
    typeof (body as { kind?: unknown }).kind === "string"
  );
}

/** The agent answers a refused install with an error envelope on a non-2xx
 *  status; hand that envelope back as a value so the dialog can show it. */
async function withEnvelope<T>(request: Promise<T>): Promise<T | PluginErrorEnvelope> {
  try {
    return await request;
  } catch (err) {
    if (err instanceof ApiError && isEnvelope(err.body)) return err.body;
    throw err;
  }
}

function archiveForm(file: File): FormData {
  const fd = new FormData();
  fd.append("file", file);
  return fd;
}

export function parsePlugin(
  source: PluginSource,
): Promise<PluginManifestSummary | PluginErrorEnvelope> {
  if (source.kind === "file") {
    return withEnvelope(
      apiFetch<PluginManifestSummary>("/api/plugins/parse", {
        method: "POST",
        body: archiveForm(source.file),
      }),
    );
  }
  return withEnvelope(
    apiFetch<PluginManifestSummary>("/api/plugins/parse_from_url", {
      method: "POST",
      body: { url: source.url, expected_sha256: source.sha256 ?? undefined },
    }),
  );
}

/** Install and grant `permissions` in one call. The agent grants each id it
 *  can and reports the ones that landed in `granted`. */
export function installPlugin(
  source: PluginSource,
  permissions: string[],
): Promise<PluginInstallResult | PluginErrorEnvelope> {
  if (source.kind === "file") {
    const query = permissions.length
      ? `?requested_permissions=${encodeURIComponent(permissions.join(","))}`
      : "";
    return withEnvelope(
      apiFetch<PluginInstallResult>(`/api/plugins/install${query}`, {
        method: "POST",
        body: archiveForm(source.file),
      }),
    );
  }
  return withEnvelope(
    apiFetch<PluginInstallResult>("/api/plugins/install_from_url", {
      method: "POST",
      body: {
        url: source.url,
        expected_sha256: source.sha256 ?? undefined,
        from_catalog: true,
        requested_permissions: permissions,
      },
    }),
  );
}

/** The required permissions the agent did not grant. */
export function missingGrants(required: string[], granted: string[] | undefined): string[] {
  const have = new Set(granted ?? []);
  return required.filter((id) => !have.has(id));
}

export function disablePlugin(pluginId: string): Promise<unknown> {
  return apiFetch(`/api/plugins/${encodeURIComponent(pluginId)}/disable`, {
    method: "POST",
  });
}

/** A permission's risk as the agent graded it; the manifest's overall risk
 *  stands in only if the agent sent none for this row. */
export function permissionRisk(p: PluginPermission, manifestRisk: RiskLevel): RiskLevel {
  return p.risk ?? manifestRisk;
}

/** Whether approval waits out the cool-off: any critical permission, or a
 *  critical plugin overall. */
export function requiresCoolOff(manifest: PluginManifestSummary): boolean {
  return (
    manifest.risk === "critical" ||
    manifest.permissions.some((p) => permissionRisk(p, manifest.risk) === "critical")
  );
}

// Group permissions by their leading namespace ("hardware", "mavlink", "ui",
// ...), strongest grants first within each group.
export interface PermissionGroup {
  group: string;
  rows: (PluginPermission & { risk: RiskLevel })[];
}

const RISK_ORDER: Record<RiskLevel, number> = {
  critical: 3,
  high: 2,
  medium: 1,
  low: 0,
};

export function groupPermissions(manifest: PluginManifestSummary): PermissionGroup[] {
  const groups = new Map<string, PermissionGroup>();
  for (const p of manifest.permissions) {
    const ns = p.id.split(".")[0] ?? "other";
    if (!groups.has(ns)) groups.set(ns, { group: ns, rows: [] });
    groups.get(ns)!.rows.push({ ...p, risk: permissionRisk(p, manifest.risk) });
  }
  for (const g of groups.values()) {
    g.rows.sort((a, b) => RISK_ORDER[b.risk] - RISK_ORDER[a.risk]);
  }
  return Array.from(groups.values()).sort((a, b) => a.group.localeCompare(b.group));
}

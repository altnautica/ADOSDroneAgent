// Plugin install orchestration. Two-stage flow:
//   1. parsePlugin(file)        - non-committing manifest preview
//   2. installPlugin(file)      - actual install
//   3. grantPermissions(...)    - sequential POST per permission
//
// On partial-grant failure (step N of M), we call disablePlugin() so
// the plugin is left in a safe, opted-out state instead of running
// with a half-granted permission set. Spec: 17-ux-install-and-permissions
// section 2 + 7.

import { ApiError } from "@/lib/api";

export interface PluginPermission {
  id: string;
  required: boolean;
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

export type RiskLevel = "low" | "medium" | "high" | "critical";

async function postFile(
  path: string,
  file: File,
): Promise<PluginManifestSummary | PluginErrorEnvelope> {
  const fd = new FormData();
  fd.append("file", file);
  const res = await fetch(path, { method: "POST", body: fd });
  // The agent always returns JSON (success or error envelope).
  const text = await res.text();
  let body: unknown = null;
  if (text) {
    try {
      body = JSON.parse(text);
    } catch {
      throw new ApiError(`${res.status} ${res.statusText}`, res.status, text);
    }
  }
  return body as PluginManifestSummary | PluginErrorEnvelope;
}

export async function parsePlugin(
  file: File,
): Promise<PluginManifestSummary | PluginErrorEnvelope> {
  return postFile("/api/plugins/parse", file);
}

export async function installPlugin(
  file: File,
): Promise<PluginManifestSummary | PluginErrorEnvelope> {
  return postFile("/api/plugins/install", file);
}

export interface GrantOutcome {
  permission: string;
  ok: boolean;
  error?: string;
}

// Grant permissions sequentially. Stops on first failure and disables
// the plugin so the operator is left with a safe, opted-out install
// rather than a partially-granted plugin running in the background.
export async function grantPermissions(
  pluginId: string,
  permissions: string[],
): Promise<{ ok: boolean; results: GrantOutcome[]; error?: string }> {
  const results: GrantOutcome[] = [];
  for (const pid of permissions) {
    try {
      const res = await fetch(
        `/api/plugins/${encodeURIComponent(pluginId)}/grant`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ permission_id: pid }),
        },
      );
      if (res.ok) {
        results.push({ permission: pid, ok: true });
        continue;
      }
      const body = await res.json().catch(() => null);
      const err =
        body && typeof body === "object" && "detail" in body
          ? String((body as { detail: unknown }).detail)
          : `${res.status} ${res.statusText}`;
      results.push({ permission: pid, ok: false, error: err });
      // Roll back: leave the plugin disabled so an unfinished grant
      // set never runs.
      await disablePlugin(pluginId).catch(() => undefined);
      return { ok: false, results, error: err };
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      results.push({ permission: pid, ok: false, error: msg });
      await disablePlugin(pluginId).catch(() => undefined);
      return { ok: false, results, error: msg };
    }
  }
  return { ok: true, results };
}

export async function enablePlugin(pluginId: string): Promise<void> {
  const res = await fetch(
    `/api/plugins/${encodeURIComponent(pluginId)}/enable`,
    { method: "POST" },
  );
  if (!res.ok) {
    const body = await res.json().catch(() => null);
    const detail =
      body && typeof body === "object" && "detail" in body
        ? String((body as { detail: unknown }).detail)
        : `${res.status} ${res.statusText}`;
    throw new ApiError(detail, res.status, body);
  }
}

export async function disablePlugin(pluginId: string): Promise<void> {
  const res = await fetch(
    `/api/plugins/${encodeURIComponent(pluginId)}/disable`,
    { method: "POST" },
  );
  if (!res.ok) {
    const body = await res.json().catch(() => null);
    const detail =
      body && typeof body === "object" && "detail" in body
        ? String((body as { detail: unknown }).detail)
        : `${res.status} ${res.statusText}`;
    throw new ApiError(detail, res.status, body);
  }
}

// Per-capability risk classifier. Mirrors the lint.py
// `high_risk_caps` set on the agent. Anything not listed is MEDIUM
// (sensible default for a hardware/MAVLink-touching plugin).
const CRITICAL_CAPS = new Set([
  "vehicle.command",
  "vehicle.payload.actuate",
  "filesystem.host",
  "mavlink.command.send",
]);

const HIGH_CAPS = new Set([
  "mavlink.write",
  "recording.write",
  "mission.write",
  "hardware.usb.uvc",
  "mavlink.component.register",
  "network.outbound",
]);

const LOW_CAP_PREFIXES = ["ui.slot.", "ui.theme.", "telemetry.subscribe."];

export function classifyPermission(id: string): RiskLevel {
  if (CRITICAL_CAPS.has(id)) return "critical";
  if (HIGH_CAPS.has(id)) return "high";
  if (LOW_CAP_PREFIXES.some((p) => id.startsWith(p))) return "low";
  return "medium";
}

// Group permissions by their leading namespace ("hardware", "mavlink",
// "ui", "recording", ...). Within each group we sort by risk descending
// so the strongest grants are at the top per spec section 2.
export interface PermissionGroup {
  group: string;
  rows: { id: string; required: boolean; risk: RiskLevel }[];
}

const RISK_ORDER: Record<RiskLevel, number> = {
  critical: 3,
  high: 2,
  medium: 1,
  low: 0,
};

export function groupPermissions(
  perms: PluginPermission[],
): PermissionGroup[] {
  const groups = new Map<string, PermissionGroup>();
  for (const p of perms) {
    const ns = p.id.split(".")[0] ?? "other";
    if (!groups.has(ns)) groups.set(ns, { group: ns, rows: [] });
    groups.get(ns)!.rows.push({
      id: p.id,
      required: p.required,
      risk: classifyPermission(p.id),
    });
  }
  for (const g of groups.values()) {
    g.rows.sort((a, b) => RISK_ORDER[b.risk] - RISK_ORDER[a.risk]);
  }
  return Array.from(groups.values()).sort((a, b) =>
    a.group.localeCompare(b.group),
  );
}

// Plain-language description for a capability. Matches spec section 9
// ("plain language, front-load the consequence"). Falls back to the
// raw capability id for plugins that declare unknown caps.
const PERM_COPY: Record<string, string> = {
  "vehicle.command":
    "Send commands to your aircraft (arm, takeoff, RTL, mode change).",
  "vehicle.payload.actuate":
    "Actuate payloads on your aircraft (gimbal, gripper, drop).",
  "mavlink.command.send":
    "Send arbitrary MAVLink commands on the bus.",
  "mavlink.write":
    "Inject MAVLink messages onto the bus that other components see.",
  "mavlink.component.register":
    "Register on the MAVLink bus as a new component.",
  "filesystem.host":
    "Read and write files anywhere on the host.",
  "recording.write":
    "Write to the recording subsystem (logs, video, frames).",
  "mission.write":
    "Modify the mission queue (add, remove, edit waypoints).",
  "telemetry.subscribe.attitude":
    "Read live attitude (roll, pitch, yaw).",
  "telemetry.subscribe.gps":
    "Read live GPS position.",
  "telemetry.subscribe.battery":
    "Read live battery voltage and current.",
  "hardware.spi": "Read and write to the SPI controller.",
  "hardware.i2c": "Read and write to the I2C controller.",
  "hardware.uart": "Read and write to the serial port.",
  "hardware.gpio": "Read and toggle GPIO lines.",
  "hardware.usb.uvc": "Access USB UVC class video devices.",
  "network.outbound": "Make outbound network requests.",
  "ui.slot.fc-tab": "Add a tab to the flight-controller panels.",
  "ui.slot.video-overlay": "Add an overlay to the live video pane.",
  "ui.slot.mission-template": "Add a mission template.",
};

export function permissionLabel(id: string): string {
  return PERM_COPY[id] ?? id;
}

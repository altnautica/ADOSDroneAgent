// The pure half of the System screen: reading the service inventory, judging a
// restart reply, and which confirm gesture each box-level action needs. Kept
// apart from the screen so the rules are testable without a DOM.

import type { Tone } from "@/lib/tone";
import type { ConfirmGesture } from "@/lib/skills";

export interface ServiceEntry {
  name: string;
  active?: boolean;
  state?: string;
  sub_state?: string;
  pid?: number | null;
  memory_mb?: number | null;
  cpu_percent?: number | null;
}

export interface ServiceInventory {
  services: ServiceEntry[];
  /** False when the agent could not query systemd; the list is then unknown,
   *  not empty. */
  systemdAvailable: boolean;
}

/** Read `GET /api/services`. Older agents answer a bare list; current ones an
 *  object with `services` and `systemd_available`. Entries without a name are
 *  dropped rather than drawn as blank rows. */
export function parseServices(body: unknown): ServiceInventory {
  let raw: unknown[] = [];
  let systemdAvailable = true;
  if (Array.isArray(body)) {
    raw = body;
  } else if (body && typeof body === "object") {
    if ("services" in body && Array.isArray(body.services)) raw = body.services;
    if ("systemd_available" in body && body.systemd_available === false) systemdAvailable = false;
  }
  const services: ServiceEntry[] = [];
  for (const s of raw) {
    if (s && typeof s === "object" && "name" in s && typeof s.name === "string" && s.name !== "") {
      services.push({ ...s, name: s.name });
    }
  }
  return { services, systemdAvailable };
}

/** The dot tone for a unit: running is ok, a failed unit is an error, anything
 *  else (inactive, activating) is a warning. */
export function serviceTone(s: ServiceEntry): Tone {
  if (s.state === "failed" || s.sub_state === "failed") return "err";
  if (s.active === true || s.state === "active") return "ok";
  return "warn";
}

/** A short unit name without the `ados-`/`.service` decoration. */
export function shortServiceName(name: string): string {
  return name.replace(/^ados-/, "").replace(/\.service$/, "");
}

export type RestartVerdict = { ok: true; message: string | null } | { ok: false; message: string | null };

/** The restart route answers HTTP 200 for every outcome and carries the
 *  verdict in `status`: only `"ok"` is a restart; an unknown unit, a systemctl
 *  failure, a timeout and an unconfirmed restart are all refusals. */
export function restartVerdict(body: unknown): RestartVerdict {
  if (!body || typeof body !== "object") return { ok: false, message: null };
  const message = "message" in body && typeof body.message === "string" && body.message ? body.message : null;
  return "status" in body && body.status === "ok" ? { ok: true, message } : { ok: false, message };
}

/** The hottest reported temperature, or null when none is reported. */
export function maxTemp(temps: Record<string, number> | null | undefined): number | null {
  const vals = Object.values(temps ?? {}).filter((v) => typeof v === "number" && Number.isFinite(v));
  return vals.length ? Math.max(...vals) : null;
}

/** Box-level operations and the confirm gesture each requires (CS-6). Both
 *  take the node, or a part of it, down, so neither is ever a plain tap. */
export type SystemAction = "reboot" | "restart-service";

export const SYSTEM_ACTION_GESTURE: Record<SystemAction, ConfirmGesture> = {
  reboot: "guarded",
  "restart-service": "guarded",
};

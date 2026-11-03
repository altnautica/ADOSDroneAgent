// Pure readers for the ground-station receive surfaces (Mesh and Sources), so
// the rules that separate "not reporting" from "nothing there" are testable
// without React. Every counter the agent leaves null stays null; nothing is
// defaulted to zero.

/** `GET /api/v1/ground-station/wfb/receiver/combined`: the receiver's combined
 *  FEC output. Every counter is null under `stale: true`. */
export interface ReceiverCombined {
  fragmentsAfterDedup: number | null;
  fecRepaired: number | null;
  outputKbps: number | null;
  up: boolean | null;
  stale: boolean;
}

export interface RelaySource {
  mac: string;
  lastSeenMs: number | null;
  fragments: number | null;
}

/** `GET /api/v1/ground-station/wfb/receiver/relays`: per-relay fragment counts.
 *  `relays` is null (unknown) while the receive loop is not reporting. */
export interface ReceiverRelays {
  relays: RelaySource[] | null;
  stale: boolean;
}

function num(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

function obj(v: unknown): Record<string, unknown> | null {
  return v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
}

export function parseReceiverCombined(raw: unknown): ReceiverCombined | null {
  const o = obj(raw);
  if (!o) return null;
  return {
    fragmentsAfterDedup: num(o.fragments_after_dedup),
    fecRepaired: num(o.fec_repaired),
    outputKbps: num(o.output_kbps),
    up: typeof o.up === "boolean" ? o.up : null,
    stale: o.stale === true,
  };
}

export function parseReceiverRelays(raw: unknown): ReceiverRelays | null {
  const o = obj(raw);
  if (!o) return null;
  const stale = o.stale === true;
  if (stale || !Array.isArray(o.relays)) return { relays: null, stale };
  const relays: RelaySource[] = [];
  for (const r of o.relays) {
    const ro = obj(r);
    if (!ro || typeof ro.mac !== "string" || !ro.mac) continue;
    relays.push({ mac: ro.mac, lastSeenMs: num(ro.last_seen_ms), fragments: num(ro.fragments) });
  }
  return { relays, stale };
}

/** Relays sorted by contribution, biggest first, with each one's share of the
 *  total fragments (null when the total is unknown or zero). */
export function relayShares(relays: RelaySource[]): (RelaySource & { share: number | null })[] {
  const total = relays.reduce((sum, r) => sum + (r.fragments ?? 0), 0);
  return [...relays]
    .sort((a, b) => (b.fragments ?? -1) - (a.fragments ?? -1))
    .map((r) => ({ ...r, share: total > 0 && r.fragments != null ? r.fragments / total : null }));
}

/** What the Mesh screen can honestly say about `GET /api/v1/ground-station/mesh`.
 *  - `not-in-mesh`: the node answered 404 or its role is `direct`.
 *  - `no-snapshot`: the agent answered `{}`; the mesh poll loop is not
 *    reporting, which is not the same as "no peers".
 *  - `error` / `loading`: no reading yet.
 *  - `ok`: a current snapshot. */
export type MeshViewKind = "not-in-mesh" | "no-snapshot" | "error" | "loading" | "ok";

export function meshViewKind(input: {
  data: Record<string, unknown> | null;
  httpStatus: number | null;
  error: string | null;
  role: string | null | undefined;
}): MeshViewKind {
  if (input.httpStatus === 404 || input.role === "direct") return "not-in-mesh";
  if (input.data == null) return input.error ? "error" : "loading";
  return input.data.up === undefined ? "no-snapshot" : "ok";
}

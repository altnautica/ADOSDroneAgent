// The flight-controller configuration model behind the Params and MSP screens:
// the agent reads (parameter cache, FC identity), the parameter draft store,
// the sequential save, bitmask arithmetic, and the CLI command risk. Pure where
// it can be, so the screens stay layout and the logic is testable without React.

import { create } from "zustand";

import { apiFetch } from "@/shared/api-fetch";
import { mspVariant, resolveFirmwareType, type MspVariant } from "@/shared/api/fc-firmware";
import { armedWriteRefusal } from "@/shared/api/msp/fc-settings";
import type { FirmwareType, ParamMetadata } from "@/shared/api/param-metadata";
import { formatParamValue } from "@/shared/api/params";
import type { AgentHeartbeat, FcSnapshot } from "@/shared/api/types";
import i18n from "@/shared/i18n";

// ── Agent reads ──────────────────────────────────────────────

/** `GET /api/params`: the agent's MAVLink parameter cache. */
export interface ParamsResponse {
  params: Record<string, number>;
  count: number;
  cached: number;
  priming?: boolean;
  priming_timeout?: boolean;
  priming_send_failed?: boolean;
  progress?: { got: number; expected: number };
}

/** `POST /api/params/{name}`: one write and whether the FC echoed it. */
export interface ParamSetResponse {
  name: string;
  value: number;
  ack: boolean;
  cached_value: number | null;
  message: string;
}

export function getParams(signal?: AbortSignal): Promise<ParamsResponse> {
  return apiFetch<ParamsResponse>("/api/params", { signal });
}

export function postParam(name: string, value: number): Promise<ParamSetResponse> {
  return apiFetch<ParamSetResponse>(`/api/params/${encodeURIComponent(name)}`, {
    method: "POST",
    body: { value },
  });
}

/** Who the FC is and whether it reports armed, from `/api/status` (family) and
 *  the snapshot (vehicle, firmware version, armed). */
export interface FcIdentity {
  firmwareType: FirmwareType;
  /** Betaflight/iNav when the FC speaks MSP, else null. */
  msp: MspVariant | null;
  firmwareVersion: string | null;
  /** Only what the heartbeat said: null until the FC reported it. */
  armed: boolean | null;
  /** An MSP FC's armed flag the agent read off a recent status reply; null
   *  when none was read lately (unknown, not disarmed). */
  mspArmed: boolean | null;
}

export function identityFrom(hb: Partial<AgentHeartbeat> | null, fc: Partial<FcSnapshot> | null): FcIdentity {
  const firmwareType = resolveFirmwareType(hb?.fcFirmware, fc?.vehicle);
  const msp =
    firmwareType === "betaflight" || firmwareType === "inav" ? firmwareType : mspVariant(hb?.fcVariant);
  return {
    firmwareType: msp ?? firmwareType,
    msp,
    firmwareVersion: fc?.firmware ?? null,
    armed: typeof fc?.armed === "boolean" ? fc.armed : null,
    mspArmed: typeof hb?.fcMspArmed === "boolean" ? hb.fcMspArmed : null,
  };
}

export async function getFcIdentity(signal?: AbortSignal): Promise<FcIdentity> {
  const [hb, snap] = await Promise.all([
    apiFetch<AgentHeartbeat>("/api/status", { signal }),
    apiFetch<{ fc?: FcSnapshot | null }>("/api/v1/dashboard/snapshot", { signal }).catch(() => null),
  ]);
  return identityFrom(hb, snap?.fc ?? null);
}

// ── Parameter drafts ─────────────────────────────────────────

export interface ParamDraft {
  original: number;
  draft: number;
}

/** The draft map after editing `name`: a draft equal to the original is dropped. */
export function withDraft(
  drafts: ReadonlyMap<string, ParamDraft>,
  name: string,
  original: number,
  draft: number,
): Map<string, ParamDraft> {
  const next = new Map(drafts);
  if (Math.abs(original - draft) < 1e-9) next.delete(name);
  else next.set(name, { original, draft });
  return next;
}

interface ParamDraftState {
  drafts: Map<string, ParamDraft>;
  setDraft: (name: string, original: number, draft: number) => void;
  discardDraft: (name: string) => void;
  clearAll: () => void;
}

/** Session-only drafts: they survive moving between screens, never a reload
 *  (the FC can change behind the cockpit's back, so persisting would go stale). */
export const useParamDrafts = create<ParamDraftState>()((set) => ({
  drafts: new Map(),
  setDraft: (name, original, draft) => set((s) => ({ drafts: withDraft(s.drafts, name, original, draft) })),
  discardDraft: (name) =>
    set((s) => {
      const next = new Map(s.drafts);
      next.delete(name);
      return { drafts: next };
    }),
  clearAll: () => set({ drafts: new Map() }),
}));

export interface SaveOutcome {
  acked: string[];
  pending: string[];
  failed: { name: string; message: string }[];
}

/** Write each draft in turn (the agent serialises PARAM_SET and waits for the
 *  echo), reporting progress after every one. One failure never stops the rest. */
export async function saveParamDrafts(
  drafts: ReadonlyMap<string, ParamDraft>,
  post: (name: string, value: number) => Promise<ParamSetResponse>,
  onProgress?: (done: number, total: number) => void,
): Promise<SaveOutcome> {
  const out: SaveOutcome = { acked: [], pending: [], failed: [] };
  const items = [...drafts.entries()];
  for (let i = 0; i < items.length; i++) {
    const [name, d] = items[i];
    try {
      const res = await post(name, d.draft);
      (res.ack ? out.acked : out.pending).push(name);
    } catch (err) {
      out.failed.push({ name, message: err instanceof Error ? err.message : String(err) });
    }
    onProgress?.(i + 1, items.length);
  }
  return out;
}

// ── Display ──────────────────────────────────────────────────

/** Current value decorated with its enum label or set bitmask flags. */
export function displayValue(value: number, m?: ParamMetadata): string {
  if (m?.values) {
    const label = m.values.get(value);
    if (label) return `${formatParamValue(value)} · ${label}`;
  }
  if (m?.bitmask && m.bitmask.size > 0) {
    const set = [...m.bitmask.entries()].filter(([bit]) => (value & (1 << bit)) !== 0).map(([, label]) => label);
    return `${formatParamValue(value)} · ${set.length ? set.join(", ") : i18n.t("telemetry.params.bitsNone")}`;
  }
  return formatParamValue(value);
}

/** Range and units, e.g. "0…100 %", or the units alone. */
export function paramHint(m?: ParamMetadata): string {
  if (m?.range) return `${m.range.min}…${m.range.max}${m.units ? ` ${m.units}` : ""}`;
  return m?.units ?? "";
}

/** A typed entry as a finite number, or null. */
export function parseEntry(text: string): number | null {
  if (text.trim() === "") return null;
  const v = Number(text);
  return Number.isFinite(v) ? v : null;
}

// ── Bitmasks ─────────────────────────────────────────────────

/** Every documented bit set. */
export function documentedMask(bitmask: ReadonlyMap<number, string>): number {
  let m = 0;
  for (const bit of bitmask.keys()) m = (m | (1 << bit)) >>> 0;
  return m;
}

export function toggleBit(value: number, bit: number, on: boolean): number {
  return (on ? value | (1 << bit) : value & ~(1 << bit)) >>> 0;
}

/** Set or clear every documented bit, keeping undocumented ones as they were. */
export function setDocumented(value: number, bitmask: ReadonlyMap<number, string>, on: boolean): number {
  const doc = documentedMask(bitmask);
  return (on ? value | doc : value & ~doc) >>> 0;
}

export function undocumentedBits(value: number, bitmask: ReadonlyMap<number, string>): number {
  return ((value >>> 0) & ~documentedMask(bitmask)) >>> 0;
}

// ── MSP ──────────────────────────────────────────────────────

/** Why the MSP save is disabled now: only an FC the agent last read as armed.
 *  Unknown leaves it enabled, because the client re-reads the FC's armed flag
 *  at save time and refuses unless it reports disarmed. */
export function mspSaveBlock(mspArmed: boolean | null | undefined): string | null {
  return mspArmed === true ? armedWriteRefusal(true) : null;
}

/** MSP setting drafts after editing `name`: equal to the FC value drops it. */
export function withMspDraft(
  drafts: ReadonlyMap<string, string>,
  name: string,
  next: string,
  original: string,
): Map<string, string> {
  const m = new Map(drafts);
  if (next === original) m.delete(name);
  else m.set(name, next);
  return m;
}

interface MspDraftState {
  drafts: Map<string, string>;
  setDraft: (name: string, next: string, original: string) => void;
  clearAll: () => void;
}

/** Session-only MSP drafts, kept across screens like the parameter drafts. */
export const useMspDrafts = create<MspDraftState>()((set) => ({
  drafts: new Map(),
  setDraft: (name, next, original) => set((s) => ({ drafts: withMspDraft(s.drafts, name, next, original) })),
  clearAll: () => set({ drafts: new Map() }),
}));

/** What a typed CLI line does to the FC. `reboot` lines (save, exit, reboot,
 *  defaults, bootloader, mass storage) restart it or wipe its settings and go
 *  out only through a guarded control; everything else is sent as typed. */
export type CliRisk = "reboot" | "none";

const REBOOTING: Record<string, true> = { save: true, exit: true, reboot: true, defaults: true, bl: true, dfu: true, msc: true };

export function cliCommandRisk(line: string): CliRisk {
  const word = line.trim().split(/\s+/)[0]?.toLowerCase() ?? "";
  if (REBOOTING[word] !== true) return "none";
  // `exit noreboot` / `save noreboot` leave the FC running.
  return /\bnoreboot\b/i.test(line) ? "none" : "reboot";
}

/** Keep a CLI transcript bounded: the last `max` characters, cut at a line. */
export function appendTranscript(prev: string, text: string, max = 20_000): string {
  const next = prev + text;
  if (next.length <= max) return next;
  const cut = next.slice(next.length - max);
  const nl = cut.indexOf("\n");
  return nl >= 0 ? cut.slice(nl + 1) : cut;
}

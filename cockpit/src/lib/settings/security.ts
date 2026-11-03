// The Security settings page's logic: whether the agent requires a key, the
// dashboard PIN's state, the WebSocket auth enforcement confirm, and the PIN
// change form check.
//
// The agent's edge requires a key exactly when the node is paired: the pairing
// key is the credential, and `security.api.api_key` (when set) is a second key
// accepted wherever the pairing key is. An unpaired node enforces neither, so a
// configured key alone does not make one required.

import { CLOSED_GATE, openGate, settleGate, type ConfirmGate } from "@/lib/settings/apply-form";
import { isValidPin, type PinStatus } from "@/shared/api/pin";

export const PIN_POLL_MS = 15_000;
export const PAIRING_POLL_MS = 5_000;

export type ApiKeyState = "loading" | "unknown" | "required" | "notRequired";

export interface ApiKeyView {
  state: ApiKeyState;
  /** `security.api.api_key` is set (GET /api/config redacts it to "***"). */
  configuredKey: boolean;
}

/** The GET /api/config redaction sentinel for a set secret. Only the set or
 *  unset state is ever read, never the value. */
export const REDACTED = "***";

export function apiKeyView(input: {
  /** `/api/pairing/info`; null until it answers. */
  pairing: { paired?: boolean | null } | null;
  pairingError: boolean;
  configuredApiKey: unknown;
}): ApiKeyView {
  const configuredKey = input.configuredApiKey === REDACTED;
  if (input.pairing === null) {
    return { state: input.pairingError ? "unknown" : "loading", configuredKey };
  }
  return { state: input.pairing.paired === true ? "required" : "notRequired", configuredKey };
}

/** What clearing the operator key does to this panel's own sign-in.
 *  `operator`: the panel holds the operator key (it stored it from a rotation),
 *  so it falls back to the PIN. `possible`: it may. `none`: it cannot. */
export type OwnKeyRisk = "none" | "operator" | "possible";

/**
 * The rule. Neither key is readable by the panel: `GET /api/config` redacts the
 * operator key and no route returns the pairing key. So:
 * - No stored key, no configured operator key, or an unpaired node (which
 *   enforces no key) → `none`.
 * - The stored key equals the key this page stored from a rotation (the one
 *   moment the operator key's value is known) → `operator`.
 * - Any other stored key may be either the pairing key or the operator key →
 *   `possible`. The clear then checks the stored key on its own and drops it
 *   only when the node refuses it.
 */
export function ownKeyRisk(input: {
  storedKey: string | null;
  rotatedKey: string | null;
  view: ApiKeyView;
}): OwnKeyRisk {
  if (!input.storedKey || !input.view.configuredKey || input.view.state === "notRequired") return "none";
  if (input.rotatedKey !== null && input.storedKey === input.rotatedKey) return "operator";
  return "possible";
}

export type PinState = "loading" | "unknown" | "locked" | "set" | "notSet";

/** The PIN state for the status row. A failed read is `unknown`, never `notSet`. */
export function pinState(status: PinStatus | null, failed: boolean): PinState {
  if (failed) return "unknown";
  if (!status) return "loading";
  if (status.locked) return "locked";
  return status.pin_set ? "set" : "notSet";
}

/** One step of the WS auth enforcement toggle: the write to send now (null for
 *  none) and the confirm gate to show. */
export interface WsEnforceStep {
  write: boolean | null;
  gate: ConfirmGate<true>;
}

/** Turning enforcement on can lock out a ground station that holds no pairing
 *  key, so that direction waits behind a slide. Turning it off writes at once. */
export function requestWsEnforce(next: boolean): WsEnforceStep {
  return next ? { write: null, gate: openGate(true as const) } : { write: false, gate: CLOSED_GATE };
}

/** Settle the slide: only a confirmed open gate writes `true`. */
export function settleWsEnforce(gate: ConfirmGate<true>, confirmed: boolean): WsEnforceStep {
  return { write: settleGate(gate, confirmed), gate: CLOSED_GATE };
}

export type PinFormProblem = "current" | "pin" | "mismatch" | "same";

export interface PinFormInput {
  /** True when a PIN is set, so the current one is needed to change it. */
  pinSet: boolean;
  current: string;
  pin: string;
  confirm: string;
}

/** What blocks the PIN form, or null when it may be sent. The guard stays
 *  disabled while anything blocks it. */
export function pinFormProblem(input: PinFormInput): PinFormProblem | null {
  if (input.pinSet && !isValidPin(input.current)) return "current";
  if (!isValidPin(input.pin)) return "pin";
  if (input.pin !== input.confirm) return "mismatch";
  if (input.pinSet && input.pin === input.current) return "same";
  return null;
}

/** The `/api/dashboard/pin/set` arguments for a valid form: a change carries
 *  the current PIN, a first set does not. */
export function pinFormRequest(input: PinFormInput): { pin: string; currentPin?: string } | null {
  if (pinFormProblem(input) !== null) return null;
  return input.pinSet ? { pin: input.pin, currentPin: input.current } : { pin: input.pin };
}

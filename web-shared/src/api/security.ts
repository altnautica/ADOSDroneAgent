// Credential management from an authorized panel: rotate or clear the
// operator API key (`security.api.api_key`), and reset the dashboard PIN.
//
// The operator key is a second key the agent accepts wherever the pairing key
// is; the pairing key itself is never touched here. A rotation answers the new
// key exactly once, so `rotateApiKey` stores it as this browser's credential
// before handing it back: the panel's own requests keep working even when the
// key it held was the one just replaced. The agent refuses both key routes
// over a cloud or ground-station relay (403), so they work only on a direct
// reach of the node.
//
// Clearing can take away the key this panel itself signs in with. Neither key
// is readable by the panel (`GET /api/config` redacts the operator key and no
// route returns the pairing key), so after a clear the stored key is checked
// on its own against the gated status route: a 401 means it was the operator
// key just cleared, so it is dropped and the access gate is raised at once,
// which falls back to the PIN. Any other answer keeps it (it is the pairing
// key, the reach is on-box, or the check could not tell).

import { apiFetch, raiseAuthChallenge } from "../api-fetch";
import { getApiKey, setApiKey } from "../api-key";
import i18n from "../i18n";

export const API_KEY_PATH = "/api/v1/security/api-key";
export const API_KEY_ROTATE_PATH = "/api/v1/security/api-key/rotate";
export const PIN_CLEAR_PATH = "/api/dashboard/pin/clear";
/** A gated route the stored-key check probes. */
export const KEY_CHECK_PATH = "/api/status";

/** The operator key this browser stored from its last rotation, the one time
 *  the panel knows the configured operator key's value. */
let rotatedKey: string | null = null;

/** The key this browser stored from a rotation in this page's life, or null.
 *  Equal to the stored key exactly when the panel holds the operator key. */
export function rotatedOperatorKey(): string | null {
  return rotatedKey;
}

export interface RotatedApiKey {
  ok: true;
  /** The new operator key. Shown once; the agent never returns it again. */
  api_key: string;
  /** RFC 3339. */
  rotated_at: string;
}

/** `POST /api/v1/security/api-key/rotate`. Stores the new key for this
 *  browser, then returns it so the caller can show it once. */
export async function rotateApiKey(): Promise<RotatedApiKey> {
  const res = await apiFetch<Partial<RotatedApiKey> | null>(API_KEY_ROTATE_PATH, { method: "POST" });
  const key = typeof res?.api_key === "string" ? res.api_key.trim() : "";
  if (!key) throw new Error(i18n.t("cockpit.settingsSections.security.rotateNoKey"));
  setApiKey(key);
  rotatedKey = key;
  return { ok: true, api_key: key, rotated_at: typeof res?.rotated_at === "string" ? res.rotated_at : "" };
}

export interface ClearedApiKey {
  ok: true;
  /** The panel's stored key was the cleared operator key and was dropped. */
  droppedStoredKey: boolean;
}

/** Whether the agent still accepts `key` on its own (no session alongside). A
 *  network failure counts as accepted: only a definite 401 drops a key. */
async function keyStillAccepted(key: string): Promise<boolean> {
  try {
    const res = await fetch(KEY_CHECK_PATH, { headers: { Accept: "application/json", "X-ADOS-Key": key } });
    return res.status !== 401;
  } catch {
    return true;
  }
}

/** `DELETE /api/v1/security/api-key`: clear the operator key. The pairing key
 *  keeps working. When the panel's own stored key no longer authorizes, it is
 *  dropped and the access gate is raised so the panel falls back to the PIN. */
export async function clearApiKey(): Promise<ClearedApiKey> {
  await apiFetch<{ ok: boolean }>(API_KEY_PATH, { method: "DELETE" });
  rotatedKey = null;
  const held = getApiKey();
  if (!held || (await keyStillAccepted(held))) return { ok: true, droppedStoredKey: false };
  // Another tab may have stored a different key meanwhile; drop only this one.
  if (getApiKey() === held) setApiKey(null);
  raiseAuthChallenge(401);
  return { ok: true, droppedStoredKey: true };
}

/** `POST /api/dashboard/pin/clear`: reset the dashboard PIN. Live sessions end
 *  when the next PIN is set; until then the access splash offers a first set. */
export function clearDashboardPin(): Promise<{ ok: boolean }> {
  return apiFetch<{ ok: boolean }>(PIN_CLEAR_PATH, { method: "POST" });
}

// Whether the agent requires a key from off-box callers, for the Security card.
// Kept free of React so it is unit-testable.
//
// The agent's edge requires a key exactly when the node is paired: the pairing
// key in pairing.json is the credential, and `security.api.api_key` (when set)
// is a second key accepted wherever the pairing key is. An unpaired node
// enforces neither, so a configured key alone does not make one required.

export type ApiKeyState = "loading" | "unknown" | "required" | "notRequired";

export interface ApiKeyView {
  state: ApiKeyState;
  /** `security.api.api_key` is set (GET /api/config redacts it to "***"). */
  configuredKey: boolean;
}

/** The GET /api/config redaction sentinel: a non-empty api_key comes back as
 *  this. Only the set/unset state is ever read, never the value. */
export const REDACTED = "***";

export function apiKeyView(input: {
  /** `/api/pairing/info`; undefined until it answers. */
  pairing: { paired: boolean } | undefined;
  pairingError: boolean;
  configuredApiKey: unknown;
}): ApiKeyView {
  const configuredKey = input.configuredApiKey === REDACTED;
  if (input.pairing === undefined) {
    return { state: input.pairingError ? "unknown" : "loading", configuredKey };
  }
  return { state: input.pairing.paired ? "required" : "notRequired", configuredKey };
}

// Moving the primary controller moves RC input: blocked while the vehicle is
// armed or its arm state is unknown, and a refusal the agent raises anyway
// (armed since the last reading) becomes an explicit force question.

import { apiFetch } from "@/shared/api-fetch";
import { withArmedRetry } from "@/shared/api/armed-retry";

export type PrimaryBlock = "armed" | "unknown" | null;

/** Why the primary cannot change now: only a reported disarm allows it. */
export function primaryBlock(armed: boolean | null | undefined): PrimaryBlock {
  if (armed === false) return null;
  return armed === true ? "armed" : "unknown";
}

type Put = (path: string, init: { method: "PUT"; body: unknown }) => Promise<unknown>;

/** PUT the new primary; on the agent's armed refusal ask `confirm`, and only
 *  on a yes repeat it with the force override. */
export function setPrimaryPad(
  deviceId: string,
  confirm: (msg: string) => Promise<boolean>,
  put: Put = (path, init) => apiFetch(path, init),
): Promise<unknown> {
  return withArmedRetry(
    (force) =>
      put("/api/v1/ground-station/gamepads/primary", {
        method: "PUT",
        body: force ? { device_id: deviceId, force: true } : { device_id: deviceId },
      }),
    confirm,
  );
}

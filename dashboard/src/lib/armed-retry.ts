// Retry a link-dropping action once with the armed override.
//
// The agent refuses an action that can drop the aircraft's link while the
// vehicle is armed: HTTP 409 with `{"error":"E_ARMED","override":"force"}`.
// `withArmedRetry` turns that refusal into an explicit operator decision: it
// asks `confirm`, and only on a yes repeats the call with `force=true`.

import { ApiError } from "@/shared/api-fetch";

export const ARMED_CONFIRM_MESSAGE =
  "The vehicle is armed. Applying this now can drop the link. Apply anyway?";

export async function withArmedRetry<T>(
  call: (force: boolean) => Promise<T>,
  confirm: (msg: string) => Promise<boolean>,
): Promise<T> {
  try {
    return await call(false);
  } catch (err) {
    const armed = err instanceof ApiError && err.status === 409 && err.code === "E_ARMED";
    if (!armed) throw err;
    if (!(await confirm(ARMED_CONFIRM_MESSAGE))) throw err;
    return call(true);
  }
}

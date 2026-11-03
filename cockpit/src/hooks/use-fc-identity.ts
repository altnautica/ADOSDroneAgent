import { useCallback } from "react";

import { getFcIdentity, type FcIdentity } from "@/lib/fc-config";
import { useResource, type Resource } from "@/shared/use-resource";

const IDENTITY_POLL_MS = 2000;

/** The FC identity the Params and MSP screens route on (family, MSP variant,
 *  firmware version, armed). */
export function useFcIdentity(): Resource<FcIdentity> {
  return useResource(useCallback((s: AbortSignal) => getFcIdentity(s), []), IDENTITY_POLL_MS);
}

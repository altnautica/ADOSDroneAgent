import type { Profile } from "@/lib/types";
import { useProfile } from "@/shared/use-profile";

/** The node profile from the public pairing-info probe, in the dashboard's
 *  vocabulary: `auto` until the agent first answers, `unknown` for a profile
 *  the probe does not recognise. */
export function useNodeProfile(): Profile {
  return useProfile() ?? "auto";
}

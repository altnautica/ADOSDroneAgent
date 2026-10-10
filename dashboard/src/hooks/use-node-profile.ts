import type { Profile } from "@/lib/types";
import { useProfile } from "@/shared/use-profile";

/** The node profile from the public pairing-info probe, in the dashboard's
 *  vocabulary: `auto` until the agent first answers, `unknown` for a profile
 *  the dashboard has no pages for. */
export function useNodeProfile(): Profile {
  const profile = useProfile();
  if (profile === null) return "auto";
  if (profile === "drone" || profile === "ground_station") return profile;
  return "unknown";
}

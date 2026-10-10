// The inputs every skill gate reads, assembled from the status and flight
// stores. "Commandable" means a flight-controller link this node's
// `POST /api/command` reaches: a drone's own FC (`fc_connected`), or on a
// ground station a relayed aircraft whose lane the agent reports fresh (the
// station's router carries the command over the radio to it).

import { useShallow } from "zustand/react/shallow";

import type { SkillContext } from "@/lib/skills";
import { useProfile } from "@/shared/use-profile";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useFlightStore } from "@/stores/flight-store";
import { useStatusStore } from "@/stores/status-store";

export function useSkillContext(): SkillContext {
  const profile = useProfile();
  const droneFc = useStatusStore((s) => s.status?.fc_connected === true);
  const { live, armed, relayedFresh } = useFlightStore(
    useShallow((s) => ({
      live: s.live,
      armed: s.telemetry?.armed === true,
      relayedFresh: s.relayed && s.telemetry?.relayed_link?.fresh !== false,
    })),
  );
  const reported = useExtensionsStore((s) => s.reported);
  const fcConnected =
    profile === "drone" ? droneFc : profile === "ground_station" ? relayedFresh : false;
  return { fcConnected, live, armed, reported };
}

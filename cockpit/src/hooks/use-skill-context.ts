// The inputs every skill gate reads, assembled from the status and flight
// stores. "Commandable" means the node's own flight controller link
// (`fc_connected`): `POST /api/command` writes only the local MAVLink socket,
// so a ground station relaying another aircraft cannot command it from here,
// and its skills say so.

import { useShallow } from "zustand/react/shallow";

import type { SkillContext } from "@/lib/skills";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useFlightStore } from "@/stores/flight-store";
import { useStatusStore } from "@/stores/status-store";

export const RELAYED_LINK_REASON = "Commands go through the drone's link";

export function useSkillContext(): SkillContext {
  const fcConnected = useStatusStore((s) => s.status?.fc_connected === true);
  const { live, armed, relayed } = useFlightStore(
    useShallow((s) => ({ live: s.live, armed: s.telemetry?.armed === true, relayed: s.relayed })),
  );
  const reported = useExtensionsStore((s) => s.reported);
  return {
    fcConnected,
    live,
    armed,
    reported,
    linkReason: !fcConnected && relayed ? RELAYED_LINK_REASON : undefined,
  };
}

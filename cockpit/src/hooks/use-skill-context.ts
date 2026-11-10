// The inputs every skill gate reads, assembled from the status, flight,
// nav-stream (the drone's pilot state), pilot-link and extensions stores.
// "Commandable" means the node's own flight controller link (`fc_connected`):
// `POST /api/command` writes only the local MAVLink socket, so a ground
// station relaying another aircraft cannot command it from here, and its
// skills say so. `skillContextNow()` reads the same inputs outside React (the
// runner and the cockpit actions).

import { useShallow } from "zustand/react/shallow";

import { useNow } from "@/hooks/use-now";
import { usePilotLinkStore } from "@/lib/pilot-link";
import { PILOT_FRESH_MS, type SkillContext } from "@/lib/skills";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStreamStore, type NavStreamState } from "@/stores/nav-stream-store";
import { useStatusStore } from "@/stores/status-store";

export const RELAYED_LINK_REASON = "Commands go through the drone's link";

interface ContextInputs {
  fcConnected: boolean;
  live: boolean;
  armed: boolean;
  lastArmed: boolean | null;
  relayed: boolean;
  pilot: NavStreamState["pilot"];
  pilotLinkOpen: boolean;
  reported: SkillContext["reported"];
  pending: SkillContext["pending"];
  now: number;
}

function buildContext(i: ContextInputs): SkillContext {
  const p = i.pilot;
  return {
    fcConnected: i.fcConnected,
    live: i.live,
    armed: i.armed,
    lastArmed: i.lastArmed,
    linkReason: !i.fcConnected && i.relayed ? RELAYED_LINK_REASON : undefined,
    pilot: p
      ? {
          armed: p.data.armed,
          landed: p.data.landed,
          vehicleClass: p.data.vehicleClass,
          fcFailsafe: p.data.fcFailsafe,
          fresh: i.now - p.at <= PILOT_FRESH_MS,
        }
      : null,
    pilotLinkOpen: i.pilotLinkOpen,
    reported: i.reported,
    pending: i.pending,
    now: i.now,
  };
}

/** The skill context now, outside React. */
export function skillContextNow(): SkillContext {
  const flight = useFlightStore.getState();
  const ext = useExtensionsStore.getState();
  return buildContext({
    fcConnected: useStatusStore.getState().status?.fc_connected === true,
    live: flight.live,
    armed: flight.telemetry?.armed === true,
    lastArmed: flight.armedAtLastLive,
    relayed: flight.relayed,
    pilot: useNavStreamStore.getState().pilot,
    pilotLinkOpen: usePilotLinkStore.getState().link === "connected",
    reported: ext.reported,
    pending: ext.pending,
    now: Date.now(),
  });
}

export function useSkillContext(): SkillContext {
  const fcConnected = useStatusStore((s) => s.status?.fc_connected === true);
  const flight = useFlightStore(
    useShallow((s) => ({
      live: s.live,
      armed: s.telemetry?.armed === true,
      lastArmed: s.armedAtLastLive,
      relayed: s.relayed,
    })),
  );
  const pilot = useNavStreamStore((s) => s.pilot);
  const pilotLinkOpen = usePilotLinkStore((s) => s.link === "connected");
  const { reported, pending } = useExtensionsStore(useShallow((s) => ({ reported: s.reported, pending: s.pending })));
  // Freshness (the pilot's 2 s, a plugin's 3 s) ages with the clock.
  const now = useNow(1000);
  return buildContext({ fcConnected, ...flight, pilot, pilotLinkOpen, reported, pending, now });
}

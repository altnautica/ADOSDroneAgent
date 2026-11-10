// The read-only flight-state chip for the dashboard's status card: the pilot
// entry of the shared nav-status poll through the cockpit's chip rules
// (`chipInputs`). A failed or late read keeps the last pilot frame and its
// receipt time, so the chip ages into LINK instead of vanishing. Null only
// before any pilot frame was received.

import { useQuery } from "@tanstack/react-query";
import { useMemo, useState } from "react";

import { useNavStatus, type NavStatusBody } from "@/hooks/use-nav-status";
import { useNow } from "@/hooks/use-now";
import { useVehicleTelemetry, type VehicleTelemetry } from "@/hooks/use-vehicle-telemetry";
import { skillDetailFromState, skillStateTopic } from "@/shared/hud/skill-detail";
import { apiFetch } from "@/shared/api-fetch";
import { chipInputs, freshLocalPos } from "@/shared/hud/chip-inputs";
import { parseArFrame, parsePilotState, type PilotState } from "@/shared/hud/cockpit-frames";
import { flightStateChip, type ChipView } from "@/shared/hud/flight-state";

export interface ChipSources {
  /** The last nav-status body received (kept across failed reads). */
  body: NavStatusBody | undefined;
  /** Wall-clock ms the body was received. */
  receivedAtMs: number;
  nowMs: number;
  telemetry: Pick<VehicleTelemetry, "home_distance" | "local_position"> | null | undefined;
  modeDetail: string | null;
  completeAgeMs: number | null;
}

/** The pilot state of a nav-status body, or null without one. */
export function navPilot(body: NavStatusBody | undefined): PilotState | null {
  return body?.pilot ? parsePilotState(body.pilot.data) : null;
}

/** The chip for the last received body read at `nowMs`: the pilot's age is
 *  its age at the agent plus the time since receipt. */
export function navStatusChip(s: ChipSources): ChipView | null {
  const entry = s.body?.pilot;
  const pilot = navPilot(s.body);
  if (!entry || !pilot) return null;
  const ar = s.body?.ar;
  return flightStateChip(
    chipInputs(pilot, s.nowMs, s.receivedAtMs - (entry.age_ms ?? 0), {
      homeDistM: s.telemetry?.home_distance ?? null,
      // The agent passes a local position through until 5 s old; the chip
      // measures only from one at most 2 s old.
      localPos: freshLocalPos(s.telemetry?.local_position),
      arFrame: ar ? parseArFrame(ar.data) : null,
      arAgeMs: ar ? s.nowMs - s.receivedAtMs + (ar.age_ms ?? 0) : 0,
      modeDetail: s.modeDetail,
      completeAgeMs: s.completeAgeMs,
    }),
  );
}

/** The owning skill's chip detail, polled only while a skill owns. */
function useSkillDetail(pilot: PilotState | null): string | null {
  const skill = pilot?.owner === "skill" ? pilot.skill : null;
  const pluginPath = skill ? `/api/plugins/${encodeURIComponent(skill.pluginId)}` : "";
  const detail = useQuery<unknown>({
    queryKey: ["plugin-detail", pluginPath],
    queryFn: ({ signal }) => apiFetch<unknown>(pluginPath, { signal }),
    enabled: skill !== null,
    staleTime: 60_000,
    retry: false,
  });
  const topic = skill ? skillStateTopic(detail.data, skill.skillId) : null;
  const state = useQuery<unknown>({
    queryKey: ["plugin-state", pluginPath],
    queryFn: ({ signal }) => apiFetch<unknown>(`${pluginPath}/state`, { signal }),
    enabled: topic !== null,
    refetchInterval: 1_000,
    retry: false,
  });
  return topic ? skillDetailFromState(state.data, topic) : null;
}

export function useFlightChip(): ChipView | null {
  const nav = useNavStatus();
  const tel = useVehicleTelemetry();
  const now = useNow(500);
  const pilot = useMemo(() => navPilot(nav.data), [nav.data]);
  const modeDetail = useSkillDetail(pilot);
  const [completeAt, setCompleteAt] = useState<number | null>(null);
  const complete = pilot?.mission?.state === "complete";
  // When the mission turned complete, adjusted while rendering.
  if (complete !== (completeAt !== null)) setCompleteAt(complete ? now : null);
  return navStatusChip({
    body: nav.data,
    receivedAtMs: nav.dataUpdatedAt,
    nowMs: now,
    telemetry: tel.data,
    modeDetail,
    completeAgeMs: completeAt === null ? null : now - completeAt,
  });
}

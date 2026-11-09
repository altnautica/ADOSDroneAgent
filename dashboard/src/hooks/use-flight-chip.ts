// The read-only flight-state chip for the dashboard's status card: the pilot
// entry of `GET /api/v1/nav/status` through the cockpit's chip rules. Null on
// a node with no pilot (the request fails or carries no `pilot` entry).

import { useQuery } from "@tanstack/react-query";

import { apiFetch } from "@/shared/api-fetch";
import { parsePilotState } from "@/shared/hud/cockpit-frames";
import { chipFacts, flightStateChip, type ChipView } from "@/shared/hud/flight-state";

interface NavStatusPilot {
  pilot?: { age_ms: number; data: unknown } | null;
}

/** The chip for one nav-status body, or null without a pilot entry. */
export function navStatusChip(body: NavStatusPilot | undefined): ChipView | null {
  const entry = body?.pilot;
  if (!entry) return null;
  const p = parsePilotState(entry.data);
  if (!p) return null;
  return flightStateChip(chipFacts(p, entry.age_ms, { homeDistM: null, modeDetail: null, completeAgeMs: null }));
}

export function useFlightChip(): ChipView | null {
  const q = useQuery<NavStatusPilot>({
    queryKey: ["nav-status-pilot"],
    queryFn: ({ signal }) => apiFetch<NavStatusPilot>("/api/v1/nav/status", { signal }),
    refetchInterval: 1_000,
    retry: false,
  });
  return q.isError ? null : navStatusChip(q.data);
}

// What the EKF navigates on, resolved from the telemetry poll, the EKF source
// sets and vision-nav's fresh `nav.state`: read by the nav-source chip and by
// the status pill (whose shared rules raise a degraded source).

import { useEffect } from "react";

import { useNow } from "@/hooks/use-now";
import { loadEkfSourceParams, navSourceInput } from "@/lib/ekf-sources";
import { resolveNavSource, type NavSourceResult } from "@/shared/hud/nav-source";
import type { NavState } from "@/shared/hud/nav-types";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStreamStore, type Stamped } from "@/stores/nav-stream-store";
import { useStatusStore } from "@/stores/status-store";

/** vision-nav's report counts while younger than this; from this age it is stale. */
export const NAV_STATE_FRESH_MS = 2000;

/** vision-nav's report while it counts, else null. */
export function freshNavState(nav: Stamped<NavState> | null, nowMs: number): NavState | null {
  return nav && nowMs - nav.at < NAV_STATE_FRESH_MS ? nav.data : null;
}

/** `loadParams`: read the EKF source sets once per FC link. One always-mounted
 *  reader (the status pill) loads them; the chip, which can move into the
 *  Menu, only reads. */
export function useNavSource(loadParams: boolean): {
  result: NavSourceResult;
  freshNav: NavState | null;
  params: Record<string, number> | null;
} {
  const telemetry = useFlightStore((s) => s.telemetry);
  const live = useFlightStore((s) => s.live);
  const fcConnected = useStatusStore((s) => s.status?.fc_connected === true);
  const nav = useNavStreamStore((s) => s.nav);
  const lastCommandedSet = useNavStreamStore((s) => s.lastCommandedSet);
  const params = useNavStreamStore((s) => s.ekfParams);
  const now = useNow(nav ? 1000 : null);

  useEffect(() => {
    if (!loadParams || !fcConnected) return;
    const ac = new AbortController();
    void loadEkfSourceParams(ac.signal).catch(() => undefined);
    return () => ac.abort();
  }, [loadParams, fcConnected]);

  const freshNav = freshNavState(nav, now);
  return { result: resolveNavSource(navSourceInput(telemetry, live, freshNav, lastCommandedSet, params)), freshNav, params };
}

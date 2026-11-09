// The cockpit's active alerts (`lib/alerts` rules) from the live stores, aged
// by one 1 Hz tick so the link timers advance between telemetry frames.

import { useEffect, useState } from "react";

import { computeAlerts, type CockpitAlert } from "@/lib/alerts";
import { useFeedStore } from "@/stores/feed-store";
import { useFlightStore } from "@/stores/flight-store";
import { useStatusStore } from "@/stores/status-store";

export function useCockpitAlerts(): CockpitAlert[] {
  const status = useStatusStore((s) => s.status);
  const battery = useStatusStore((s) => s.battery);
  const telemetry = useFlightStore((s) => s.telemetry);
  const live = useFlightStore((s) => s.live);
  const video = useFeedStore((s) => (s.videoMounted ? s.video.state : null));
  const lastLiveAt = useFlightStore((s) => s.lastLiveAt);
  const armedAtLastLive = useFlightStore((s) => s.armedAtLastLive);
  const [now, setNow] = useState(() => performance.now());

  useEffect(() => {
    const id = setInterval(() => setNow(performance.now()), 1000);
    return () => clearInterval(id);
  }, []);

  return computeAlerts({
    status,
    telemetry,
    battery,
    msSinceLive: live ? 0 : lastLiveAt === null ? null : Math.max(0, now - lastLiveAt),
    video,
    armedAtLastLive,
  });
}

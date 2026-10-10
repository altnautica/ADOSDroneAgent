// The Feed's alert stack, under the status strip. Critical alerts flash their
// border at 4 Hz for 3 s and then hold steady; warnings pulse at 2 Hz;
// advisories do not move. Text never flashes and no level is colour-only: each
// row carries an icon and a level label. Critical rows announce assertively,
// the rest politely. Rules live in `lib/alerts`.

import { useEffect, useState } from "react";

import { computeAlerts, type AlertLevel } from "@/lib/alerts";
import { cn } from "@/lib/utils";
import { useFeedStore } from "@/stores/feed-store";
import { useFlightStore } from "@/stores/flight-store";
import { useStatusStore } from "@/stores/status-store";

const MAX_VISIBLE = 3;

const LEVEL_STYLE: Record<AlertLevel, string> = {
  critical: "border-err bg-err/20 text-err alert-flash-critical",
  warning: "border-warn bg-warn/15 text-warn alert-pulse-warning",
  advisory: "border-hud-hair bg-hud-glass text-hud-ink",
};

const LEVEL_LABEL: Record<AlertLevel, string> = {
  critical: "Critical",
  warning: "Warning",
  advisory: "Advisory",
};

export function AlertStack() {
  const status = useStatusStore((s) => s.status);
  const battery = useStatusStore((s) => s.battery);
  const telemetry = useFlightStore((s) => s.telemetry);
  const live = useFlightStore((s) => s.live);
  const video = useFeedStore((s) => (s.videoMounted ? s.video.state : null));
  const lastLiveAt = useFlightStore((s) => s.lastLiveAt);
  const armedAtLastLive = useFlightStore((s) => s.armedAtLastLive);
  const [now, setNow] = useState(() => performance.now());

  // One shared 1 Hz tick ages the link timers.
  useEffect(() => {
    const id = setInterval(() => setNow(performance.now()), 1000);
    return () => clearInterval(id);
  }, []);

  const alerts = computeAlerts({
    status,
    telemetry,
    battery,
    msSinceLive: live ? 0 : lastLiveAt === null ? null : Math.max(0, now - lastLiveAt),
    video,
    armedAtLastLive,
  });
  const visible = alerts.slice(0, MAX_VISIBLE);
  const critical = visible.filter((a) => a.level === "critical");
  const other = visible.filter((a) => a.level !== "critical");

  const row = (a: (typeof visible)[number]) => {
    const Icon = a.icon;
    return (
      <div
        key={a.id}
        className={cn(
          "flex max-w-[42rem] items-center gap-[0.6rem] rounded-lg border-2 px-[0.9rem] py-[0.4rem] backdrop-blur-hud",
          LEVEL_STYLE[a.level],
        )}
      >
        <Icon className="h-[1.3rem] w-[1.3rem] shrink-0" aria-hidden />
        <div className="min-w-0">
          <div className="text-[0.9rem] font-semibold leading-tight">
            <span className="mr-[0.4rem] text-[0.75rem] uppercase tracking-wide">{LEVEL_LABEL[a.level]}</span>
            {a.title}
          </div>
          {a.detail ? (
            <div className="truncate text-[0.75rem] text-hud-ink-2">{a.detail}</div>
          ) : null}
        </div>
      </div>
    );
  };

  return (
    <div className="pointer-events-none flex flex-col items-center gap-[0.3rem]">
      <div role="alert" aria-live="assertive" className="flex flex-col items-center gap-[0.3rem]">
        {critical.map(row)}
      </div>
      <div role="status" aria-live="polite" className="flex flex-col items-center gap-[0.3rem]">
        {other.map(row)}
      </div>
    </div>
  );
}

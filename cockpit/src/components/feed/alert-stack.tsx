// The Feed's alert stack, under the status strip. Critical alerts flash their
// border at 4 Hz for 3 s and then hold steady; warnings pulse at 2 Hz;
// advisories do not move. Text never flashes and no level is colour-only: each
// row carries an icon and a level label. Critical rows announce assertively,
// the rest politely. Rules live in `lib/alerts`.

import { useCockpitAlerts } from "@/hooks/use-cockpit-alerts";
import { useWidthClass } from "@/hooks/use-width-class";
import { classScale } from "@/lib/flight-layout";
import { cn } from "@/lib/utils";
import type { AlertLevel } from "@/shared/hud/status-pill";

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
  const alerts = useCockpitAlerts();
  // Sized in px × the class scale: the stack sits outside the scaled zones.
  const k = classScale(useWidthClass().widthClass);
  const px = (n: number) => `${n * k}px`;
  const visible = alerts.slice(0, MAX_VISIBLE);
  const critical = visible.filter((a) => a.level === "critical");
  const other = visible.filter((a) => a.level !== "critical");

  const row = (a: (typeof visible)[number]) => {
    const Icon = a.icon;
    return (
      <div
        key={a.id}
        className={cn("flex items-center rounded-lg border-2 backdrop-blur-hud", LEVEL_STYLE[a.level])}
        style={{ maxWidth: px(560), gap: px(8), padding: `${px(6)} ${px(12)}` }}
      >
        <Icon className="shrink-0" style={{ width: px(20), height: px(20) }} aria-hidden />
        <div className="min-w-0">
          <div className="font-semibold leading-tight" style={{ fontSize: px(14) }}>
            <span className="uppercase tracking-wide" style={{ fontSize: px(11), marginRight: px(6) }}>
              {LEVEL_LABEL[a.level]}
            </span>
            {a.title}
          </div>
          {a.detail ? (
            <div className="truncate text-hud-ink-2" style={{ fontSize: px(12) }}>
              {a.detail}
            </div>
          ) : null}
        </div>
      </div>
    );
  };

  return (
    <div className="pointer-events-none flex flex-col items-center" style={{ gap: px(4) }}>
      <div role="alert" aria-live="assertive" className="flex flex-col items-center" style={{ gap: px(4) }}>
        {critical.map(row)}
      </div>
      <div role="status" aria-live="polite" className="flex flex-col items-center" style={{ gap: px(4) }}>
        {other.map(row)}
      </div>
    </div>
  );
}

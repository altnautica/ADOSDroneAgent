// The Feed's alert stack, under the status strip. Each row is the flight
// chrome's dark panel with the level's hue mixed in (at most 18 %), so its
// words read over any picture: the title in the panel's text colour and only
// the level word in the level's text colour. Critical alerts flash their
// border at 4 Hz for 3 s and then hold steady; warnings pulse at 2 Hz;
// advisories do not move. Text never flashes and no level is colour-only:
// each row carries an icon and a level label. Critical rows announce
// assertively, the rest politely. Rules live in `lib/alerts`.

import { useCockpitAlerts } from "@/hooks/use-cockpit-alerts";
import { useWidthClass } from "@/hooks/use-width-class";
import { CHROME_TOKENS } from "@/lib/chrome-tokens";
import { ALERT_TINT_PCT, classScale, labelPx } from "@/lib/flight-layout";
import { cn } from "@/lib/utils";
import { specText } from "@/shared/hud/spec-text";
import { severityTextColor, type AlertLevel } from "@/shared/hud/status-pill";

const MAX_VISIBLE = 3;

/** The motion each level's border carries. */
const LEVEL_STYLE: Record<AlertLevel, string> = {
  critical: "alert-flash-critical",
  warning: "alert-pulse-warning",
  advisory: "",
};

const LEVEL_HUE: Record<AlertLevel, string> = {
  critical: CHROME_TOKENS.severity_critical,
  warning: CHROME_TOKENS.severity_warning,
  advisory: CHROME_TOKENS.border,
};

/** The level word is the only text in the level's colour. */
function levelTextColor(level: AlertLevel): string {
  return level === "advisory" ? CHROME_TOKENS.text_dim : severityTextColor(level);
}

const levelLabel = (level: AlertLevel) => specText(`alert.level.${level}`);

export function AlertStack() {
  const alerts = useCockpitAlerts();
  // Sized in px × the class scale: the stack sits outside the scaled zones.
  const { widthClass } = useWidthClass();
  const k = classScale(widthClass);
  const px = (n: number) => `${n * k}px`;
  const visible = alerts.slice(0, MAX_VISIBLE);
  const critical = visible.filter((a) => a.level === "critical");
  const other = visible.filter((a) => a.level !== "critical");

  const row = (a: (typeof visible)[number]) => {
    const Icon = a.icon;
    return (
      <div
        key={a.id}
        data-testid="alert-row"
        data-level={a.level}
        className={cn("chrome-panel flex items-center !border-2", LEVEL_STYLE[a.level])}
        style={{
          maxWidth: px(560),
          gap: px(8),
          padding: `${px(6)} ${px(12)}`,
          borderColor: LEVEL_HUE[a.level],
          // The level's hue tints the panel, never replaces it.
          background: `color-mix(in srgb, ${LEVEL_HUE[a.level]} ${ALERT_TINT_PCT[a.level]}%, var(--fs-panel-bg))`,
        }}
      >
        <Icon className="shrink-0" style={{ width: px(20), height: px(20), color: LEVEL_HUE[a.level] }} aria-hidden />
        <div className="min-w-0">
          <div className="font-semibold leading-tight" style={{ fontSize: px(14) }}>
            <span
              className="uppercase tracking-wide"
              style={{ fontSize: `${labelPx(11, widthClass)}px`, marginRight: px(6), color: levelTextColor(a.level) }}
              data-testid="alert-level"
            >
              {levelLabel(a.level)}
            </span>
            {a.title}
          </div>
          {a.detail ? (
            <div className="chrome-dim truncate" style={{ fontSize: `${labelPx(12, widthClass)}px` }}>
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

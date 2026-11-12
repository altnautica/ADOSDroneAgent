// The top bar's status pill and its alert list.

import { useEffect, useId, useRef, useState } from "react";
import { CheckCircle2 } from "lucide-react";

import { Overlay } from "@/components/ui/overlay";
import { useCockpitAlerts } from "@/hooks/use-cockpit-alerts";
import { useNow } from "@/hooks/use-now";
import { usePilot } from "@/hooks/use-pilot";
import { pillWiring } from "@/lib/alerts";
import { chromeStyle, severityColor, severityIcon } from "@/lib/chrome-tokens";
import { useNavSource } from "@/hooks/use-nav-source";
import { HIT_AREA } from "@/lib/flight-layout";
import { preflightChecks } from "@/shared/hud/preflight";
import { cn } from "@/lib/utils";
import { specText } from "@/shared/hud/spec-text";
import { pillAlerts, pillSummary, severityTextColor, type PillAlert } from "@/shared/hud/status-pill";
import { useFlightStore } from "@/stores/flight-store";
import { useStatusPillStore } from "@/stores/status-pill-store";
import { useStatusStore } from "@/stores/status-store";

/** The status pill: "All systems OK", the one alert, or "<n> warnings",
 *  flashing on a new alert until the list is opened. The shared rules also
 *  read the pilot frame's age (no or a stale frame never reads OK, and drops
 *  what that frame said), whether the agent answers, a lost controller, and
 *  on the ground the pre-flight checklist's failing items. `fit`: every
 *  movable item is in the Menu, so the pill truncates with an ellipsis. */
export function StatusPill({ compact, fit }: { compact: boolean; fit: boolean }) {
  const alerts = useCockpitAlerts();
  const stamped = usePilot();
  const pilot = stamped?.data;
  const now = useNow(500);
  const readiness = pilot?.readiness ?? [];
  const fcStatus = pilot?.statustext;
  const telemetry = useFlightStore((s) => s.telemetry);
  const live = useFlightStore((s) => s.live);
  const battery = useStatusStore((s) => s.battery);
  const statusStale = useStatusStore((s) => s.stale);
  const prearm = telemetry?.prearm;
  const storagePct = useStatusStore((s) => s.status?.system?.disk_pct ?? null);
  const acked = useStatusPillStore((s) => s.acked);
  const ack = useStatusPillStore((s) => s.ack);
  // The list's anchor, read when the pill is pressed; null while closed.
  const [rect, setRect] = useState<DOMRect | null>(null);
  const pillRef = useRef<HTMLButtonElement>(null);
  const open = rect !== null;
  const navSource = useNavSource(true).result;
  // The pilot frame's arm state while there is one (null: its FC link is
  // down, possibly airborne), else the telemetry's.
  const armed: boolean | null | undefined = pilot ? pilot.armed : telemetry?.armed;
  const wiring = pillWiring({
    pilot: stamped,
    nowMs: now,
    statusStale,
    armed: armed ?? undefined,
    checks: preflightChecks({ telemetry, battery, live }),
    alerts,
  });
  const summary = pillSummary(pillAlerts({ alerts, readiness, prearm, storagePct, fcStatus, ...wiring, armed, navSource }), acked);
  const color = severityColor(summary.severity);

  return (
    <>
      <button
        ref={pillRef}
        type="button"
        onClick={(e) => setRect(open ? null : e.currentTarget.getBoundingClientRect())}
        aria-label={`${specText("cockpit.menu.status")}: ${summary.text}`}
        aria-expanded={open}
        data-zone="status_pill"
        data-severity={summary.severity}
        className={cn(
          HIT_AREA,
          "flex h-[var(--fs-control)] min-w-[44px] items-center gap-[6px] rounded-[var(--fs-radius-sm)] border bg-transparent px-[10px] font-semibold",
          fit ? "min-w-0 shrink" : "shrink-0",
          summary.flashing && "chrome-pill-flash",
        )}
        // A dot in the severity colour; the outline in it unless all is well;
        // the words in the severity's text colour.
        style={{ color: severityTextColor(summary.severity), borderColor: summary.severity === "ok" ? "var(--fs-border)" : `${color}80` }}
      >
        <span aria-hidden data-testid="status-pill-dot" className="h-[6px] w-[6px] shrink-0 rounded-full" style={{ background: color }} />
        {compact && summary.count > 0 ? (
          <span className="tabular-nums">{summary.count}</span>
        ) : compact ? null : (
          <span className="min-w-0 truncate">{summary.text}</span>
        )}
      </button>
      {rect ? (
        <AlertList rect={rect} alerts={summary.alerts} ack={ack} onClose={() => setRect(null)} />
      ) : null}
    </>
  );
}

/** The pill's alert list, on the shared overlay frame below the pill: one
 *  overlay at a time, Escape, controller back or a press outside close it
 *  (STOP still takes that first press). Mounting it is showing it, so it
 *  acknowledges what it lists, and an alert arriving while it is open. */
function AlertList({
  rect,
  alerts,
  ack,
  onClose,
}: {
  rect: DOMRect;
  alerts: readonly PillAlert[];
  ack: (ids: string[]) => void;
  onClose: () => void;
}) {
  const titleId = useId();
  const idKey = alerts.map((a) => a.id).join("|");
  useEffect(() => {
    ack(idKey ? idKey.split("|") : []);
  }, [idKey, ack]);
  return (
    <Overlay
      onDismiss={onClose}
      scrim="clear"
      labelledBy={titleId}
      panelClassName="chrome-panel chrome-bar-text w-[min(360px,90vw)]"
      panelStyle={{ ...chromeStyle(), position: "absolute", left: Math.max(8, Math.min(rect.left, window.innerWidth - 368)), top: rect.bottom + 8 }}
    >
      <div className="flex max-h-[60vh] flex-col gap-[4px] overflow-y-auto p-[8px]" data-testid="status-pill-list">
        <div id={titleId} className="chrome-label px-[4px]" data-testid="status-pill-list-title">
          {specText("cockpit.alerts.title")}
        </div>
        {alerts.length === 0 ? (
          <div className="flex items-center gap-[8px] px-[4px] py-[6px]" style={{ color: severityColor("ok") }}>
            <CheckCircle2 className="chrome-icon" aria-hidden />
            {specText("cockpit.status.ok")}
          </div>
        ) : (
          alerts.map((a) => (
            // Text first, its source under it; the glyph is the spec's
            // severity icon in the severity colour.
            <div key={a.id} data-testid="alert-item" data-severity={a.severity} className="flex items-start gap-[8px] rounded-[var(--fs-radius-sm)] px-[4px] py-[6px]">
              <span
                data-testid="alert-item-icon"
                className="chrome-icon mt-[1px] flex shrink-0 items-center justify-center font-bold leading-none"
                style={{ color: severityColor(a.severity) }}
                aria-hidden
              >
                {severityIcon(a.severity)}
              </span>
              <div className="min-w-0">
                <div data-testid="alert-item-text" style={a.severity === "critical" ? { color: severityTextColor("critical") } : undefined}>
                  {a.text}
                </div>
                <div data-testid="alert-item-source" className="chrome-label">
                  {specText(`cockpit.status.source.${a.source}`)}
                </div>
              </div>
            </div>
          ))
        )}
      </div>
    </Overlay>
  );
}

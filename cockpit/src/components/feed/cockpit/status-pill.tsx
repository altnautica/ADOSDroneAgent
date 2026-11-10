// The top bar's status pill and its alert list.

import { useEffect, useId, useRef, useState } from "react";
import { AlertTriangle, CheckCircle2, XCircle } from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { Overlay } from "@/components/ui/overlay";
import { useCockpitAlerts } from "@/hooks/use-cockpit-alerts";
import { useNow } from "@/hooks/use-now";
import { usePilot } from "@/hooks/use-pilot";
import { pillWiring } from "@/lib/alerts";
import { chromeStyle, severityColor } from "@/lib/chrome-tokens";
import { HIT_AREA, severityTextColor } from "@/lib/flight-layout";
import { preflightChecks } from "@/lib/safety-band";
import { cn } from "@/lib/utils";
import { specText } from "@/shared/hud/spec-text";
import { pillAlerts, pillSummary, type PillAlert, type PillSeverity } from "@/shared/hud/status-pill";
import { useFlightStore } from "@/stores/flight-store";
import { useStatusPillStore } from "@/stores/status-pill-store";
import { useStatusStore } from "@/stores/status-store";

const SEVERITY_ICON: Record<PillSeverity, LucideIcon> = { ok: CheckCircle2, warning: AlertTriangle, critical: XCircle };

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
  const home = useFlightStore((s) => s.home);
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
  const wiring = pillWiring({
    pilot: stamped,
    nowMs: now,
    statusStale,
    armed: telemetry?.armed,
    checks: preflightChecks({ telemetry, battery, live, home }),
    alerts,
  });
  const summary = pillSummary(pillAlerts({ alerts, readiness, prearm, storagePct, fcStatus, ...wiring }), acked);
  const color = severityColor(summary.severity);
  const Icon = SEVERITY_ICON[summary.severity];

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
          "flex min-h-[36px] min-w-[44px] items-center gap-[6px] rounded-full border px-[10px] font-semibold",
          fit ? "min-w-0 shrink" : "shrink-0",
          summary.flashing && "chrome-pill-flash",
        )}
        style={{ color, borderColor: `${color}80` }}
      >
        <Icon className="chrome-icon shrink-0" aria-hidden />
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
        <div id={titleId} className="chrome-label px-[4px] uppercase tracking-wide">
          {specText("cockpit.alerts.title")}
        </div>
        {alerts.length === 0 ? (
          <div className="flex items-center gap-[8px] px-[4px] py-[6px]" style={{ color: severityColor("ok") }}>
            <CheckCircle2 className="chrome-icon" aria-hidden />
            {specText("cockpit.status.ok")}
          </div>
        ) : (
          alerts.map((a) => {
            const AIcon = SEVERITY_ICON[a.severity];
            return (
              <div key={a.id} className="flex items-start gap-[8px] rounded-[var(--fs-radius-sm)] px-[4px] py-[6px]">
                <AIcon className="chrome-icon mt-[1px] shrink-0" style={{ color: severityColor(a.severity) }} aria-hidden />
                <div className="min-w-0">
                  <div className="chrome-label">{specText(`cockpit.status.source.${a.source}`)}</div>
                  <div style={a.severity === "critical" ? { color: severityTextColor("critical") } : undefined}>{a.text}</div>
                </div>
              </div>
            );
          })
        )}
      </div>
    </Overlay>
  );
}

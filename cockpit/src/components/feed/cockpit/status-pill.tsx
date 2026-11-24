// The top bar's status pill and its alert list.

import { useEffect, useId, useRef, useState } from "react";

import { Overlay } from "@/components/ui/overlay";
import { useCockpitAlerts } from "@/hooks/use-cockpit-alerts";
import { useNow } from "@/hooks/use-now";
import { usePilot } from "@/hooks/use-pilot";
import { cockpitAlertActions, useAlertCentreStore } from "@/lib/alert-centre";
import { pillWiring } from "@/lib/alerts";
import { popoverStyle, severityColor } from "@/lib/chrome-tokens";
import { useNavSource } from "@/hooks/use-nav-source";
import { HIT_AREA } from "@/lib/flight-layout";
import { useBeatPhase } from "@/lib/redraw-beat";
import { useChromeScale } from "@/lib/ui-scale";
import { CHROME_ALERT_SKIN, useAlertSnooze, type AlertActions } from "@/shared/hud/alert-centre";
import { AlertCentre } from "@/shared/hud/alert-rows";
import { preflightChecks } from "@/shared/hud/preflight";
import { cn } from "@/lib/utils";
import { specText } from "@/shared/hud/spec-text";
import { pillAlerts, pillSummary, severityTextColor, unsnoozedAlerts, type PillAlert } from "@/shared/hud/status-pill";
import { simClockLabel } from "@/shared/hud/sim-clock";
import { useNodeIdentity } from "@/shared/use-profile";
import { useFlightStore } from "@/stores/flight-store";
import { useStatusPillStore } from "@/stores/status-pill-store";
import { useStatusStore } from "@/stores/status-store";

/** The status pill: "All systems OK", the one alert, or "<n> warnings",
 *  flashing on a new alert until the list is opened: it steps between two
 *  looks every 500 ms on the redraw beat, so an unattended pill costs two
 *  frames a second, not a 60 fps fade. The shared rules also
 *  read the pilot frame's age (no or a stale frame never reads OK, and drops
 *  what that frame said), whether the agent answers, a lost controller, and
 *  on the ground the pre-flight checklist's failing items. `fit`: every
 *  movable item is in the Menu, so the pill truncates with an ellipsis. */
export function StatusPill({ compact, fit }: { compact: boolean; fit: boolean }) {
  const alerts = useCockpitAlerts();
  const stamped = usePilot();
  const pilot = stamped?.data;
  const now = useNow(500);
  const flashPhase = useBeatPhase(5);
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
  const { profile } = useNodeIdentity();
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
  const all = pillAlerts({ alerts, readiness, prearm, storagePct, fcStatus, ...wiring, armed, navSource });
  // Keys prune only while the pilot frame and the agent answer: a stale
  // frame drops its items without their conditions having cleared.
  const { snoozed, setSnoozed } = useAlertSnooze(all, !all.some((a) => a.id === "pilot_not_answering" || a.id === "agent_unreachable"));
  const summary = pillSummary(unsnoozedAlerts(all, snoozed), acked);
  const color = severityColor(summary.severity);
  const warnings = summary.alerts.filter((a) => a.severity === "warning").length;
  useEffect(() => {
    useAlertCentreStore.setState({ warnings });
  }, [warnings]);
  const openSeq = useAlertCentreStore((s) => s.openSeq);
  useEffect(() => {
    if (openSeq > 0 && pillRef.current) setRect(pillRef.current.getBoundingClientRect());
  }, [openSeq]);

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
          // The flash owns the background while it runs; `bg-transparent`
          // is a utility and would outrank the component-layer flash class.
          "flex h-[var(--fs-control)] min-w-[44px] items-center gap-[6px] rounded-[var(--fs-radius-sm)] border px-[10px] font-semibold",
          fit ? "min-w-0 shrink" : "shrink-0",
          summary.flashing ? "chrome-pill-flashing" : "bg-transparent",
          summary.flashing && flashPhase % 2 === 1 && "chrome-pill-dim",
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
      {pilot?.simClock ? (
        // A run on a simulator's clock never reads as real time.
        <span
          data-zone="sim_clock"
          className="shrink-0 rounded-[var(--fs-radius-sm)] border px-[6px] text-[11px] font-semibold tabular-nums"
          style={{ color: severityColor("warning"), borderColor: severityColor("warning") }}
        >
          {simClockLabel(pilot.simClock)}
        </span>
      ) : null}
      {rect ? (
        <AlertList
          rect={rect}
          alerts={all}
          counted={summary.alerts}
          snoozed={snoozed}
          onSnooze={setSnoozed}
          actions={cockpitAlertActions(profile, () => setRect(null))}
          ack={ack}
          onClose={() => setRect(null)}
        />
      ) : null}
    </>
  );
}

/** The pill's alert centre, on the shared overlay frame below the pill: one
 *  overlay at a time, Escape, controller back or a press outside close it
 *  (STOP still takes that first press). Mounting it is showing it, so it
 *  acknowledges what the pill counts, and an alert arriving while it is
 *  open. It is drawn at the chrome scale and scrolls inside the room under
 *  the pill. */
function AlertList({
  rect,
  alerts,
  counted,
  snoozed,
  onSnooze,
  actions,
  ack,
  onClose,
}: {
  rect: DOMRect;
  alerts: readonly PillAlert[];
  counted: readonly PillAlert[];
  snoozed: ReadonlySet<string>;
  onSnooze: (key: string, on: boolean) => void;
  actions: AlertActions;
  ack: (ids: string[]) => void;
  onClose: () => void;
}) {
  const titleId = useId();
  const pop = popoverStyle(rect, 440, useChromeScale());
  const idKey = counted.map((a) => a.id).join("|");
  useEffect(() => {
    ack(idKey ? idKey.split("|") : []);
  }, [idKey, ack]);
  return (
    <Overlay
      onDismiss={onClose}
      scrim="clear"
      labelledBy={titleId}
      panelClassName="chrome-panel chrome-bar-text"
      panelStyle={pop.panel}
    >
      <div className="overflow-y-auto overscroll-contain p-[8px]" style={{ maxHeight: pop.maxHeight }} data-testid="status-pill-list">
        <AlertCentre alerts={alerts} snoozed={snoozed} onSnooze={onSnooze} actions={actions} skin={CHROME_ALERT_SKIN} titleId={titleId} />
      </div>
    </Overlay>
  );
}

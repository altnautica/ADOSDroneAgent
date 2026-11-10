// The cockpit top bar (cockpit spec `layout.json` zone `top_bar`), one row
// that never scrolls: the flight-state chip, the status pill, battery
// (percent, time left and the RTL-reserve marker), link, navigation source,
// the controller, the layer-level button with its layer menu, Menu, and STOP
// at the right end. When the row runs out of room items move into the Menu in
// the spec's order (`TOP_BAR_OVERFLOW`); the chip, the pill, battery, Menu
// and STOP stay, and with every movable item gone the chip and the pill
// truncate with an ellipsis. Every control's hit area is at least
// `touch_target.min` (`HIT_AREA`), whatever its drawn size.

import { useEffect, useId, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { AlertTriangle, CheckCircle2, ChevronDown, Gamepad2, Layers, Menu, OctagonX, Radio, XCircle } from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { FlightStateChip } from "@/components/feed/cockpit/flight-state-chip";
import { NavSourceChip } from "@/components/feed/nav-source-chip";
import { Overlay } from "@/components/ui/overlay";
import { useCockpitAlerts } from "@/hooks/use-cockpit-alerts";
import { padStatusText, usePadStatus } from "@/hooks/use-gamepad";
import { useFreshPilot, usePilot } from "@/hooks/use-pilot";
import { useSkillContext } from "@/hooks/use-skill-context";
import { pillAlertStale, pillStale } from "@/lib/alerts";
import { chromeStyle, severityColor } from "@/lib/chrome-tokens";
import { BUILTIN_LAYERS, layerFlag } from "@/lib/cockpit-view";
import { barFull, HIT_AREA, movedItems, TOP_BAR_OVERFLOW, type TopBarOverflowItem } from "@/lib/flight-layout";
import { fmtClock, linkLabel } from "@/lib/safety-band";
import { requestSkill } from "@/lib/skill-runner";
import { BUILTIN_SKILL_BY_ID } from "@/lib/skills";
import { cn } from "@/lib/utils";
import { BATTERY_LOW_PCT, batteryReading } from "@/shared/hud/battery";
import { specText } from "@/shared/hud/spec-text";
import { pillAlerts, pillSummary, type PillAlert, type PillSeverity } from "@/shared/hud/status-pill";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStore } from "@/stores/nav-store";
import { useStatusPillStore } from "@/stores/status-pill-store";
import { useStatusStore } from "@/stores/status-store";
import { useTopBarStore } from "@/stores/top-bar-store";

const levelLabel = (level: string) => specText(`cockpit.level.${level}`);

const SEVERITY_ICON: Record<PillSeverity, LucideIcon> = { ok: CheckCircle2, warning: AlertTriangle, critical: XCircle };

function Item({ icon: Icon, label, zone, children }: { icon?: LucideIcon; label: string; zone: string; children?: ReactNode }) {
  return (
    <span className="flex shrink-0 items-center gap-[6px] whitespace-nowrap" aria-label={label} data-zone={zone}>
      {Icon ? <Icon className="chrome-icon chrome-dim" aria-hidden /> : null}
      {children}
    </span>
  );
}

/** Battery: percent, time left to the reserve, and a bar with the reserve
 *  marker. */
function BatteryItem({ compact }: { compact: boolean }) {
  const battery = useStatusStore((s) => s.battery);
  const telemetry = useFlightStore((s) => s.telemetry);
  const r = batteryReading(battery, telemetry);
  const reserve = battery?.thresholds?.reserve_percent;
  const reservePct = typeof reserve === "number" && reserve > 0 && reserve < 100 ? reserve : null;
  const label = specText("cockpit.status.source.battery");
  if (!r) {
    return (
      <Item label={label} zone="battery">
        <span className="chrome-dim">{specText("cockpit.battery.none")}</span>
      </Item>
    );
  }
  const low = reservePct ?? BATTERY_LOW_PCT;
  const severity: PillSeverity = r.pct <= low ? "critical" : r.pct <= low + 10 ? "warning" : "ok";
  const color = severity === "ok" ? undefined : severityColor(severity);
  return (
    <span className="flex shrink-0 items-center gap-[6px] whitespace-nowrap" aria-label={label} data-zone="battery">
      <span className="relative h-[11px] w-[26px] rounded-[3px] border border-[var(--fs-text-dim)]" aria-hidden>
        <span
          className="absolute inset-y-[1px] left-[1px] rounded-[1px]"
          style={{ width: `calc(${Math.max(0, Math.min(100, r.pct))}% - 2px)`, background: severityColor(severity) }}
        />
        {reservePct !== null ? (
          <span
            className="absolute inset-y-[-3px] w-[2px] rounded-full bg-[var(--fs-text)]"
            style={{ left: `${reservePct}%` }}
            title={specText("cockpit.battery.reserve")}
            data-testid="rtl-reserve-marker"
          />
        ) : null}
      </span>
      <span className="font-semibold" style={{ color }}>
        {Math.round(r.pct)}%
      </span>
      {!compact && r.timeToReserveS !== null ? (
        <span className="chrome-dim" aria-label={specText("cockpit.battery.time_left", { time: fmtClock(r.timeToReserveS) })}>
          {fmtClock(r.timeToReserveS)}
        </span>
      ) : null}
    </span>
  );
}

/** The status pill: "All systems OK", the one alert, or "<n> warnings",
 *  flashing on a new alert until the list is opened. Items read from the
 *  pilot frame (readiness, the FC's status texts) stay listed once that
 *  frame is stale, drawn dimmed and never as live; a pill reporting only
 *  stale items dims. `fit`: every movable item is in the Menu, so the pill
 *  truncates with an ellipsis. */
function StatusPill({ compact, fit }: { compact: boolean; fit: boolean }) {
  const alerts = useCockpitAlerts();
  const pilot = usePilot()?.data;
  const pilotFresh = useFreshPilot() !== null;
  const readiness = pilot?.readiness ?? [];
  const fcStatus = pilot?.statustext;
  const prearm = useFlightStore((s) => s.telemetry?.prearm);
  const storagePct = useStatusStore((s) => s.status?.system?.disk_pct ?? null);
  const acked = useStatusPillStore((s) => s.acked);
  const ack = useStatusPillStore((s) => s.ack);
  // The list's anchor, read when the pill is pressed; null while closed.
  const [rect, setRect] = useState<DOMRect | null>(null);
  const pillRef = useRef<HTMLButtonElement>(null);
  const open = rect !== null;
  const summary = pillSummary(pillAlerts({ alerts, readiness, prearm, storagePct, fcStatus }), acked);
  const dim = pillStale(summary.alerts, pilotFresh);
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
        data-stale={dim || undefined}
        className={cn(
          HIT_AREA,
          "flex min-h-[36px] min-w-[44px] items-center gap-[6px] rounded-full border px-[10px] font-semibold",
          fit ? "min-w-0 shrink" : "shrink-0",
          summary.flashing && !dim && "chrome-pill-flash",
          dim && "opacity-55",
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
        <AlertList rect={rect} alerts={summary.alerts} pilotFresh={pilotFresh} ack={ack} onClose={() => setRect(null)} />
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
  pilotFresh,
  ack,
  onClose,
}: {
  rect: DOMRect;
  alerts: readonly PillAlert[];
  pilotFresh: boolean;
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
      panelClassName="chrome-panel chrome-bar-text flex max-h-[60vh] w-[min(360px,90vw)] flex-col gap-[4px] overflow-y-auto p-[8px]"
      panelStyle={{ ...chromeStyle(), position: "absolute", left: Math.max(8, Math.min(rect.left, window.innerWidth - 368)), top: rect.bottom + 8 }}
    >
      <div className="contents" data-testid="status-pill-list">
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
            const stale = pillAlertStale(a, pilotFresh);
            return (
              <div
                key={a.id}
                className={cn("flex items-start gap-[8px] rounded-[var(--fs-radius-sm)] px-[4px] py-[6px]", stale && "opacity-55")}
                data-stale={stale || undefined}
              >
                <AIcon className="chrome-icon mt-[1px] shrink-0" style={{ color: severityColor(a.severity) }} aria-hidden />
                <div className="min-w-0">
                  <div className="chrome-label">{specText(`cockpit.status.source.${a.source}`)}</div>
                  <div>{a.text}</div>
                </div>
              </div>
            );
          })
        )}
      </div>
    </Overlay>
  );
}

function ControllerItem() {
  const pad = usePadStatus();
  const c = padStatusText(pad);
  return (
    <Item icon={Gamepad2} label={pad.name ? specText("cockpit.controller.named", { name: pad.name }) : specText("cockpit.controller")} zone="controller">
      <span className={c.ok === null ? "chrome-dim" : undefined} style={c.ok === false ? { color: severityColor("warning") } : undefined}>
        {c.text}
      </span>
    </Item>
  );
}

/** The link: its icon, and its text unless that has moved into the Menu. */
function LinkItem({ text }: { text: boolean }) {
  const status = useStatusStore((s) => s.status);
  const [, setTick] = useState(0);
  // The link label carries an age; re-read it once a second.
  useEffect(() => {
    const id = setInterval(() => setTick((t) => t + 1), 1000);
    return () => clearInterval(id);
  }, []);
  const label = linkLabel(status);
  return (
    <Item icon={Radio} label={specText("cockpit.link.named", { label })} zone="link">
      {text ? <span>{label}</span> : null}
    </Item>
  );
}

/** The layer menu: one checkbox per layer flag (`layers.json` layers). */
function LayerFlagsList({ className }: { className?: string }) {
  const layers = useCockpitViewStore((s) => s.layers);
  const toggleLayer = useCockpitViewStore((s) => s.toggleLayer);
  return (
    <div role="group" aria-label={specText("cockpit.layers.flags")} className={cn("flex flex-col", className)} data-testid="layer-flags">
      {BUILTIN_LAYERS.map((l) => (
        <label key={l.id} className="flex min-h-[48px] cursor-pointer items-center justify-between gap-[12px] px-[6px]">
          <span>{l.label}</span>
          <input
            type="checkbox"
            className="h-[18px] w-[18px] accent-[var(--fs-ok)]"
            checked={layerFlag(layers, l.id)}
            onChange={(e) => toggleLayer(l.id, e.target.checked)}
            data-layer={l.id}
          />
        </label>
      ))}
    </div>
  );
}

function LayerFlagsPopover({ rect, onClose }: { rect: DOMRect; onClose: () => void }) {
  return (
    <Overlay
      onDismiss={onClose}
      scrim="clear"
      panelClassName="chrome-panel chrome-bar-text w-[220px] p-[6px]"
      panelStyle={{ ...chromeStyle(), position: "absolute", left: Math.max(8, Math.min(rect.right - 220, window.innerWidth - 228)), top: rect.bottom + 8 }}
    >
      <LayerFlagsList />
    </Overlay>
  );
}

/** The layer level (a tap cycles it) and the ▾ that lists every layer flag. */
function LayerButton({ withLabel = true }: { withLabel?: boolean }) {
  const level = useCockpitViewStore((s) => s.level);
  const cycleLevel = useCockpitViewStore((s) => s.cycleLevel);
  const [rect, setRect] = useState<DOMRect | null>(null);
  return (
    <span className="flex shrink-0 items-center" data-zone="layer_button" data-level={level}>
      <button
        type="button"
        onClick={cycleLevel}
        aria-label={specText("cockpit.layers.next", { level: levelLabel(level) })}
        className={cn(HIT_AREA, "flex h-[40px] min-w-[34px] items-center gap-[6px] rounded-[var(--fs-radius-sm)] px-[8px] hover:bg-white/10")}
      >
        <Layers className="chrome-icon" aria-hidden />
        {withLabel ? levelLabel(level) : null}
      </button>
      <button
        type="button"
        onClick={(e) => setRect(rect ? null : e.currentTarget.getBoundingClientRect())}
        aria-label={specText("cockpit.layers.flags")}
        aria-expanded={rect !== null}
        aria-haspopup="dialog"
        data-testid="layer-flags-button"
        className={cn(HIT_AREA, "flex h-[40px] w-[48px] items-center justify-center rounded-[var(--fs-radius-sm)] hover:bg-white/10")}
      >
        <ChevronDown className="chrome-icon" aria-hidden />
      </button>
      {rect ? <LayerFlagsPopover rect={rect} onClose={() => setRect(null)} /> : null}
    </span>
  );
}

/** How many of `TOP_BAR_OVERFLOW` are moved out of the row: one more each
 *  frame the row overflows, back to none when the bar is resized. The row is
 *  re-checked once a second as its values change width. The count is shared
 *  with the Menu, which lists exactly the moved items. */
function useOverflowCount(row: React.RefObject<HTMLDivElement>): number {
  const hidden = useTopBarStore((s) => s.movedCount);
  const setHidden = useTopBarStore((s) => s.setMovedCount);
  useLayoutEffect(() => {
    const el = row.current;
    const bar = el?.parentElement;
    if (!el || !bar) return;
    let count = 0;
    let width = -1;
    let raf = 0;
    const check = () => {
      raf = 0;
      if (count >= TOP_BAR_OVERFLOW.length || el.scrollWidth <= el.clientWidth + 1) return;
      count += 1;
      setHidden(count);
      // Re-measure after React has drawn the shorter row.
      raf = requestAnimationFrame(() => {
        raf = requestAnimationFrame(check);
      });
    };
    const schedule = () => {
      if (!raf) raf = requestAnimationFrame(check);
    };
    const ro = new ResizeObserver(() => {
      if (bar.clientWidth !== width) {
        width = bar.clientWidth;
        count = 0;
        setHidden(0);
      }
      cancelAnimationFrame(raf);
      raf = 0;
      schedule();
    });
    ro.observe(bar);
    const tick = setInterval(schedule, 1000);
    return () => {
      ro.disconnect();
      clearInterval(tick);
      cancelAnimationFrame(raf);
      setHidden(0);
    };
  }, [row, setHidden]);
  return hidden;
}

export function TopBar({ compact = false }: { compact?: boolean }) {
  const command = useNavStore((s) => s.command);
  const ctx = useSkillContext();
  const kill = BUILTIN_SKILL_BY_ID.kill;
  const row = useRef<HTMLDivElement>(null);
  const hiddenCount = useOverflowCount(row);
  const moved = movedItems(hiddenCount);
  const full = barFull(hiddenCount);

  return (
    <div
      role="region"
      aria-label={specText("cockpit.top_bar")}
      data-zone="top_bar"
      data-moved={[...moved].join(" ")}
      className="chrome-panel chrome-bar-text pointer-events-auto flex h-full w-full items-center gap-[6px] overflow-x-clip p-[4px] pl-[6px]"
    >
      <div ref={row} className="flex min-w-0 flex-1 items-center gap-[12px] overflow-x-clip">
        <FlightStateChip compact={compact} fit={full} />
        <StatusPill compact={compact} fit={full} />
        <BatteryItem compact={compact} />
        {!moved.has("link_icon") ? <LinkItem text={!moved.has("link_text")} /> : null}
        {!moved.has("nav_source") ? (
          <span className="shrink-0" data-zone="nav_source">
            <NavSourceChip />
          </span>
        ) : null}
        {!moved.has("controller") ? <ControllerItem /> : null}
      </div>
      {!moved.has("layer_button") ? <LayerButton withLabel={!compact} /> : null}
      <button
        type="button"
        onClick={() => command("quick-menu")}
        aria-label={specText("cockpit.menu")}
        data-zone="menu"
        className={cn(HIT_AREA, "flex h-[40px] w-[44px] shrink-0 items-center justify-center rounded-[var(--fs-radius-sm)] hover:bg-white/10")}
      >
        <Menu className="chrome-icon" aria-hidden />
      </button>
      <button
        type="button"
        // Always enabled, always opens the hold-to-confirm; a refused press
        // says why on the cockpit's toast channel (`requestSkill`).
        onClick={() => requestSkill(kill, ctx)}
        title={specText("stop.hold_label")}
        aria-label={specText("stop.hold_label")}
        data-zone="stop"
        data-testid="top-bar-stop"
        className={cn(HIT_AREA, "chrome-stop flex h-[40px] w-[84px] shrink-0 items-center justify-center gap-[6px] font-bold tracking-wide")}
      >
        <OctagonX className="chrome-icon" aria-hidden />
        {specText("cockpit.stop")}
      </button>
    </div>
  );
}

/** The top-bar items the bar has moved into the Menu, shown there in full,
 *  in the order they moved. */
export function FlightMenuSection({ moved }: { moved: ReadonlySet<TopBarOverflowItem> }) {
  return (
    <div
      className="chrome-bar-text flex flex-wrap items-center gap-x-[16px] gap-y-[8px] rounded-md border border-border p-[8px]"
      style={chromeStyle()}
      aria-label={specText("cockpit.flight_status")}
      data-testid="menu-overflow"
    >
      {moved.has("controller") ? <ControllerItem /> : null}
      {moved.has("nav_source") ? (
        <span className="shrink-0" data-zone="nav_source">
          <NavSourceChip />
        </span>
      ) : null}
      {moved.has("link_text") || moved.has("link_icon") ? <LinkItem text /> : null}
      {moved.has("layer_button") ? <LayerButton /> : null}
    </div>
  );
}

/** The Menu's Layers entry: the level button and every layer flag. */
export function LayersMenuSection() {
  const level = useCockpitViewStore((s) => s.level);
  const cycleLevel = useCockpitViewStore((s) => s.cycleLevel);
  return (
    <div className="chrome-bar-text flex flex-col gap-[4px] rounded-md border border-border p-[8px]" style={chromeStyle()} data-testid="menu-layers">
      <button
        type="button"
        onClick={cycleLevel}
        aria-label={specText("cockpit.layers.next", { level: levelLabel(level) })}
        className="flex min-h-[48px] items-center gap-[8px] rounded-[var(--fs-radius-sm)] px-[6px] hover:bg-white/10"
      >
        <Layers className="chrome-icon" aria-hidden />
        {specText("cockpit.menu.layers")} · {levelLabel(level)}
      </button>
      <LayerFlagsList />
    </div>
  );
}

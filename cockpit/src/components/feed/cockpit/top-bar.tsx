// The cockpit top bar (cockpit spec `layout.json` zone `top_bar`), one row
// that never scrolls: the flight-state chip, the status pill, battery
// (percent, time left and the RTL-reserve marker), link, navigation source,
// the controller, the layer-level button, Menu, and STOP at the right end.
// When the row runs out of room the lowest-priority items move into the Menu
// (`TOP_BAR_OVERFLOW`); the chip, the pill, battery, Menu and STOP stay.

import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { AlertTriangle, CheckCircle2, Gamepad2, Layers, Menu, OctagonX, Radio, XCircle } from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { FlightStateChip } from "@/components/feed/cockpit/flight-state-chip";
import { NavSourceChip } from "@/components/feed/nav-source-chip";
import { useCockpitAlerts } from "@/hooks/use-cockpit-alerts";
import { usePadStatus, type PadStatus } from "@/hooks/use-gamepad";
import { usePilot } from "@/hooks/use-pilot";
import { useSkillContext } from "@/hooks/use-skill-context";
import { batteryReading } from "@/lib/alerts";
import { chromeStyle, severityColor } from "@/lib/chrome-tokens";
import { cockpitText } from "@/lib/cockpit-text";
import { TOP_BAR_OVERFLOW, type TopBarOverflowItem } from "@/lib/flight-layout";
import { fmtClock, linkLabel } from "@/lib/safety-band";
import { requestSkill } from "@/lib/skill-runner";
import { BUILTIN_SKILL_BY_ID, resolveSkillState } from "@/lib/skills";
import { pillAlerts, pillSummary, type PillSeverity } from "@/lib/status-pill";
import { cn } from "@/lib/utils";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStore } from "@/stores/nav-store";
import { useStatusPillStore } from "@/stores/status-pill-store";
import { useStatusStore } from "@/stores/status-store";

const levelLabel = (level: string) => cockpitText(`cockpit.level.${level}`);

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
  if (!r) {
    return (
      <Item label="Battery" zone="battery">
        <span className="chrome-dim">BAT —</span>
      </Item>
    );
  }
  const low = reservePct ?? 20;
  const severity: PillSeverity = r.pct <= low ? "critical" : r.pct <= low + 10 ? "warning" : "ok";
  const color = severity === "ok" ? undefined : severityColor(severity);
  return (
    <span className="flex shrink-0 items-center gap-[6px] whitespace-nowrap" aria-label="Battery" data-zone="battery">
      <span className="relative h-[11px] w-[26px] rounded-[3px] border border-[var(--fs-text-dim)]" aria-hidden>
        <span
          className="absolute inset-y-[1px] left-[1px] rounded-[1px]"
          style={{ width: `calc(${Math.max(0, Math.min(100, r.pct))}% - 2px)`, background: severityColor(severity) }}
        />
        {reservePct !== null ? (
          <span
            className="absolute inset-y-[-3px] w-[2px] rounded-full bg-[var(--fs-text)]"
            style={{ left: `${reservePct}%` }}
            title={cockpitText("cockpit.battery.reserve")}
            data-testid="rtl-reserve-marker"
          />
        ) : null}
      </span>
      <span className="font-semibold" style={{ color }}>
        {Math.round(r.pct)}%
      </span>
      {!compact && r.timeToReserveS !== null ? (
        <span className="chrome-dim" aria-label={cockpitText("cockpit.battery.time_left", { time: fmtClock(r.timeToReserveS) })}>
          {fmtClock(r.timeToReserveS)}
        </span>
      ) : null}
    </span>
  );
}

/** The status pill: "All systems OK", the one alert, or "<n> warnings",
 *  flashing on a new alert until the list is opened. */
function StatusPill({ compact }: { compact: boolean }) {
  const alerts = useCockpitAlerts();
  const readiness = usePilot()?.data.readiness ?? [];
  const prearm = useFlightStore((s) => s.telemetry?.prearm);
  const storagePct = useStatusStore((s) => s.status?.system?.disk_pct ?? null);
  const acked = useStatusPillStore((s) => s.acked);
  const ack = useStatusPillStore((s) => s.ack);
  const [open, setOpen] = useState(false);
  const anchor = useRef<HTMLButtonElement>(null);
  const summary = pillSummary(pillAlerts({ alerts, readiness, prearm, storagePct }), acked);
  const color = severityColor(summary.severity);
  const Icon = SEVERITY_ICON[summary.severity];
  const ids = summary.alerts.map((a) => a.id);

  // Opening the list acknowledges what it shows; an alert arriving while it
  // is open is acknowledged too, since the pilot is looking at it.
  const idKey = ids.join("|");
  useEffect(() => {
    if (open) ack(idKey ? idKey.split("|") : []);
  }, [open, idKey, ack]);

  const rect = open ? anchor.current?.getBoundingClientRect() : undefined;
  return (
    <>
      <button
        ref={anchor}
        type="button"
        onClick={() => setOpen((o) => !o)}
        aria-label={`${cockpitText("cockpit.menu.status")}: ${summary.text}`}
        aria-expanded={open}
        data-zone="status_pill"
        data-severity={summary.severity}
        className={cn(
          "flex min-h-[36px] items-center gap-[6px] rounded-full border px-[10px] font-semibold",
          "shrink-0",
          summary.flashing && "chrome-pill-flash",
        )}
        style={{ color, borderColor: `${color}80` }}
      >
        <Icon className="chrome-icon shrink-0" aria-hidden />
        {compact && summary.count > 0 ? (
          <span className="tabular-nums">{summary.count}</span>
        ) : compact ? null : (
          <span className="truncate">{summary.text}</span>
        )}
      </button>
      {open && rect ? (
        <>
          <button type="button" aria-label="Close" className="fixed inset-0 z-50 cursor-default" onClick={() => setOpen(false)} />
          <div
            role="dialog"
            aria-label={cockpitText("cockpit.alerts.title")}
            className="chrome-panel fixed z-50 flex max-h-[60vh] w-[min(360px,90vw)] flex-col gap-[4px] overflow-y-auto p-[8px]"
            style={{ left: Math.max(8, Math.min(rect.left, window.innerWidth - 368)), top: rect.bottom + 8 }}
            onKeyDown={(e) => {
              if (e.key === "Escape") setOpen(false);
            }}
          >
            <div className="chrome-label px-[4px] uppercase tracking-wide">{cockpitText("cockpit.alerts.title")}</div>
            {summary.alerts.length === 0 ? (
              <div className="flex items-center gap-[8px] px-[4px] py-[6px]" style={{ color: severityColor("ok") }}>
                <CheckCircle2 className="chrome-icon" aria-hidden />
                {cockpitText("cockpit.status.ok")}
              </div>
            ) : (
              summary.alerts.map((a) => {
                const AIcon = SEVERITY_ICON[a.severity];
                return (
                  <div key={a.id} className="flex items-start gap-[8px] rounded-[var(--fs-radius-sm)] px-[4px] py-[6px]">
                    <AIcon className="chrome-icon mt-[1px] shrink-0" style={{ color: severityColor(a.severity) }} aria-hidden />
                    <div className="min-w-0">
                      <div className="chrome-label">{cockpitText(`cockpit.status.source.${a.source}`)}</div>
                      <div>{a.text}</div>
                    </div>
                  </div>
                );
              })
            )}
          </div>
        </>
      ) : null}
    </>
  );
}

function controllerLabel(pad: PadStatus): { text: string; ok: boolean | null } {
  if (!pad.connected) return { text: "No pad", ok: null };
  const battery = pad.batteryPct !== null ? ` ${Math.round(pad.batteryPct)}%` : "";
  if (pad.native) return { text: `Native${battery}`, ok: true };
  return pad.standard ? { text: `Pad${battery}`, ok: true } : { text: "Not standard", ok: false };
}

function ControllerItem() {
  const pad = usePadStatus();
  const c = controllerLabel(pad);
  return (
    <Item icon={Gamepad2} label={pad.name ? `Controller ${pad.name}` : "Controller"} zone="controller">
      <span className={c.ok === null ? "chrome-dim" : undefined} style={c.ok === false ? { color: severityColor("warning") } : undefined}>
        {c.text}
      </span>
    </Item>
  );
}

function LinkItem({ detail }: { detail: boolean }) {
  const status = useStatusStore((s) => s.status);
  const [, setTick] = useState(0);
  // The link label carries an age; re-read it once a second.
  useEffect(() => {
    const id = setInterval(() => setTick((t) => t + 1), 1000);
    return () => clearInterval(id);
  }, []);
  const label = linkLabel(status);
  return (
    <Item icon={Radio} label={`Link ${label}`} zone="link">
      {detail ? <span>{label}</span> : null}
    </Item>
  );
}

function LayerButton({ withLabel = true }: { withLabel?: boolean }) {
  const level = useCockpitViewStore((s) => s.level);
  const cycleLevel = useCockpitViewStore((s) => s.cycleLevel);
  return (
    <button
      type="button"
      onClick={cycleLevel}
      aria-label={`Layers ${levelLabel(level)}. Next level`}
      data-zone="layer_button"
      data-level={level}
      className="flex min-h-[40px] shrink-0 items-center gap-[6px] rounded-[var(--fs-radius-sm)] px-[8px] hover:bg-white/10"
    >
      <Layers className="chrome-icon" aria-hidden />
      {withLabel ? levelLabel(level) : null}
    </button>
  );
}

/** How many of `TOP_BAR_OVERFLOW` are moved out of the row: one more each
 *  time the row overflows, back to none when the bar is resized. */
function useOverflowCount(row: React.RefObject<HTMLDivElement>): number {
  const [hidden, setHidden] = useState(0);
  useLayoutEffect(() => {
    const el = row.current?.parentElement;
    if (!el) return;
    let width = el.clientWidth;
    const ro = new ResizeObserver(() => {
      if (el.clientWidth === width) return;
      width = el.clientWidth;
      setHidden(0);
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, [row]);
  useLayoutEffect(() => {
    const el = row.current;
    if (el && hidden < TOP_BAR_OVERFLOW.length && el.scrollWidth > el.clientWidth + 1) setHidden(hidden + 1);
  });
  return hidden;
}

export function TopBar({ compact = false }: { compact?: boolean }) {
  const command = useNavStore((s) => s.command);
  const ctx = useSkillContext();
  const kill = BUILTIN_SKILL_BY_ID.kill;
  const killState = resolveSkillState(kill, ctx);
  const row = useRef<HTMLDivElement>(null);
  const hiddenCount = useOverflowCount(row);
  const moved = new Set<TopBarOverflowItem>(TOP_BAR_OVERFLOW.slice(0, hiddenCount));

  return (
    <div
      role="region"
      aria-label="Top bar"
      data-zone="top_bar"
      data-moved={[...moved].join(" ")}
      className="chrome-panel chrome-bar-text pointer-events-auto flex h-full w-full items-center gap-[6px] overflow-hidden p-[4px] pl-[6px]"
    >
      <div ref={row} className="flex min-w-0 flex-1 items-center gap-[12px] overflow-hidden">
        <FlightStateChip compact={compact} fit={hiddenCount >= TOP_BAR_OVERFLOW.length} />
        <StatusPill compact={compact} />
        <BatteryItem compact={compact} />
        {!moved.has("link") ? <LinkItem detail={!moved.has("link_detail")} /> : null}
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
        aria-label={cockpitText("cockpit.menu")}
        data-zone="menu"
        className="flex h-[40px] w-[44px] shrink-0 items-center justify-center rounded-[var(--fs-radius-sm)] hover:bg-white/10"
      >
        <Menu className="chrome-icon" aria-hidden />
      </button>
      <button
        type="button"
        onClick={() => requestSkill(kill, ctx)}
        disabled={!killState.enabled}
        title={killState.enabled ? "Emergency stop (hold 3 s, then confirm)" : killState.reason}
        aria-label={killState.enabled ? "Emergency stop" : `Emergency stop: ${killState.reason ?? "unavailable"}`}
        data-zone="stop"
        data-testid="top-bar-stop"
        className="chrome-stop flex h-[40px] w-[84px] shrink-0 items-center justify-center gap-[6px] font-bold tracking-wide disabled:opacity-50"
      >
        <OctagonX className="chrome-icon" aria-hidden />
        {cockpitText("cockpit.stop")}
      </button>
    </div>
  );
}

/** The top-bar items that move into the Menu, shown there in full: link,
 *  navigation source, controller and the layer level. */
export function FlightMenuSection() {
  return (
    <div
      className="chrome-bar-text flex flex-wrap items-center gap-x-[16px] gap-y-[8px] rounded-md border border-border p-[8px]"
      style={chromeStyle()}
      aria-label="Flight status"
    >
      <LinkItem detail />
      <span className="shrink-0">
        <NavSourceChip />
      </span>
      <ControllerItem />
      <LayerButton />
    </div>
  );
}

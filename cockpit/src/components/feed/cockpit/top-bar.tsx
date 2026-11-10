// The cockpit top bar (cockpit spec `layout.json` zone `top_bar`), one row
// that never scrolls: the flight-state chip, the status pill, battery
// (percent, time left and the RTL-reserve marker), link, navigation source,
// the controller, the layer-level button, Menu, and STOP at the right end.
// When the row runs out of room the lowest-priority items move into the Menu
// (`TOP_BAR_OVERFLOW`); the chip, the pill, battery, Menu and STOP stay.

import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { AlertTriangle, CheckCircle2, Gamepad2, Layers, Menu, OctagonX, Radio, XCircle } from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { FlightStateChip } from "@/components/feed/cockpit/flight-state-chip";
import { NavSourceChip } from "@/components/feed/nav-source-chip";
import { useCockpitAlerts } from "@/hooks/use-cockpit-alerts";
import { usePadStatus, type PadStatus } from "@/hooks/use-gamepad";
import { usePilot } from "@/hooks/use-pilot";
import { useSkillContext } from "@/hooks/use-skill-context";
import { chromeStyle, severityColor } from "@/lib/chrome-tokens";
import { specText } from "@/shared/hud/spec-text";
import { TOP_BAR_OVERFLOW, type TopBarOverflowItem } from "@/lib/flight-layout";
import { fmtClock, linkLabel } from "@/lib/safety-band";
import { requestSkill } from "@/lib/skill-runner";
import { BUILTIN_SKILL_BY_ID, resolveSkillState } from "@/lib/skills";
import { Z } from "@/lib/cockpit-layout";
import { cn } from "@/lib/utils";
import { BATTERY_LOW_PCT, batteryReading } from "@/shared/hud/battery";
import { pillAlerts, pillSummary, type PillAlert, type PillSeverity } from "@/shared/hud/status-pill";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStore } from "@/stores/nav-store";
import { useStatusPillStore } from "@/stores/status-pill-store";
import { useStatusStore } from "@/stores/status-store";

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
  if (!r) {
    return (
      <Item label="Battery" zone="battery">
        <span className="chrome-dim">BAT —</span>
      </Item>
    );
  }
  const low = reservePct ?? BATTERY_LOW_PCT;
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
 *  flashing on a new alert until the list is opened. */
function StatusPill({ compact }: { compact: boolean }) {
  const alerts = useCockpitAlerts();
  const pilot = usePilot()?.data;
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
      {rect ? <AlertList rect={rect} pill={pillRef} alerts={summary.alerts} ack={ack} onClose={() => setRect(null)} /> : null}
    </>
  );
}

/** The pill's alert list, drawn over the whole window (a portal, so the top
 *  bar's blur and clipping cannot trap or hide it) below the pill. There is no
 *  scrim: a press anywhere outside closes it and still reaches what it hit, so
 *  STOP takes the first press with the list open. Mounting it is showing it,
 *  so it acknowledges what it lists, and an alert arriving while it is open. */
function AlertList({
  rect,
  pill,
  alerts,
  ack,
  onClose,
}: {
  rect: DOMRect;
  pill: React.RefObject<HTMLButtonElement>;
  alerts: readonly PillAlert[];
  ack: (ids: string[]) => void;
  onClose: () => void;
}) {
  const listRef = useRef<HTMLDivElement>(null);
  const closeRef = useRef(onClose);
  useEffect(() => {
    closeRef.current = onClose;
  });
  const idKey = alerts.map((a) => a.id).join("|");
  useEffect(() => {
    ack(idKey ? idKey.split("|") : []);
  }, [idKey, ack]);
  useEffect(() => {
    const outside = (e: Event) => {
      const t = e.target as Node | null;
      if (t && (listRef.current?.contains(t) || pill.current?.contains(t))) return;
      closeRef.current();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") closeRef.current();
    };
    document.addEventListener("pointerdown", outside, true);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", outside, true);
      document.removeEventListener("keydown", onKey);
    };
  }, [pill]);
  return createPortal(
    <div
      ref={listRef}
      role="dialog"
      aria-label={specText("cockpit.alerts.title")}
      data-testid="status-pill-list"
      className={cn("chrome-panel chrome-bar-text fixed flex max-h-[60vh] w-[min(360px,90vw)] flex-col gap-[4px] overflow-y-auto p-[8px]", Z.sheets)}
      style={{ ...chromeStyle(), left: Math.max(8, Math.min(rect.left, window.innerWidth - 368)), top: rect.bottom + 8 }}
    >
      <div className="chrome-label px-[4px] uppercase tracking-wide">{specText("cockpit.alerts.title")}</div>
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
                <div>{a.text}</div>
              </div>
            </div>
          );
        })
      )}
    </div>,
    document.body,
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
 *  frame the row overflows, back to none when the bar is resized. The row is
 *  re-checked once a second as its values change width. */
function useOverflowCount(row: React.RefObject<HTMLDivElement>): number {
  const [hidden, setHidden] = useState(0);
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
    };
  }, [row]);
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
        aria-label={specText("cockpit.menu")}
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
        {specText("cockpit.stop")}
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

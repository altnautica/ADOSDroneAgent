// The cockpit top bar (cockpit spec `layout.json` zone `top_bar`), one row
// that never scrolls: the flight-state chip, the status pill, battery
// (percent, time left and the RTL-reserve marker), link, navigation source,
// the controller, the layer-level button with its layer menu, Menu, and STOP
// at the right end. When the row runs out of room items move into the Menu in
// the spec's order (`TOP_BAR_OVERFLOW`); the chip, the pill, battery, Menu
// and STOP stay, and with every movable item gone the chip and the pill
// truncate with an ellipsis. Every control's hit area is at least
// `touch_target.min` (`HIT_AREA`), whatever its drawn size.

import { useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { ChevronDown, Gamepad2, Layers, Menu, OctagonX, Radio } from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { FlightStateChip } from "@/components/feed/cockpit/flight-state-chip";
import { StatusPill } from "@/components/feed/cockpit/status-pill";
import { NavSourceChip } from "@/components/feed/nav-source-chip";
import { Overlay } from "@/components/ui/overlay";
import { padStatusText, usePadStatus } from "@/hooks/use-gamepad";
import { useSkillContext } from "@/hooks/use-skill-context";
import { chromeStyle, severityColor } from "@/lib/chrome-tokens";
import { BUILTIN_LAYERS, layerFlag } from "@/lib/cockpit-view";
import {
  barFull,
  HIT_AREA,
  linkCell,
  linkDetail,
  movedItems,
  reserveText,
  severityTextColor,
  TOP_BAR_OVERFLOW,
  type TopBarOverflowItem,
} from "@/lib/flight-layout";
import { fmtClock } from "@/lib/safety-band";
import { requestSkill } from "@/lib/skill-runner";
import { BUILTIN_SKILL_BY_ID } from "@/lib/skills";
import { cn } from "@/lib/utils";
import { BATTERY_LOW_PCT, batteryReading } from "@/shared/hud/battery";
import { specText } from "@/shared/hud/spec-text";
import type { PillSeverity } from "@/shared/hud/status-pill";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStore } from "@/stores/nav-store";
import { useStatusStore } from "@/stores/status-store";
import { useTopBarStore } from "@/stores/top-bar-store";

const levelLabel = (level: string) => specText(`cockpit.level.${level}`);

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
  const color = severity === "ok" ? undefined : severityTextColor(severity);
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
      {r.timeToReserveS !== null ? (
        <span className="chrome-dim" aria-label={reserveText(fmtClock(r.timeToReserveS), false)} data-testid="battery-to-reserve">
          {reserveText(fmtClock(r.timeToReserveS), compact)}
        </span>
      ) : null}
    </span>
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

/** The link: the newest vehicle frame's age (`top_bar.link`), its icon
 *  coloured by the spec's age thresholds, and its text unless that has moved
 *  into the Menu. A press opens the link popover with the RSSI. */
function LinkItem({ text }: { text: boolean }) {
  const ageS = useStatusStore((s) => s.status?.heartbeat_age_s);
  const rssi = useStatusStore((s) => s.status?.link?.rssi_dbm);
  const [rect, setRect] = useState<DOMRect | null>(null);
  const cell = linkCell(typeof ageS === "number" ? ageS * 1000 : null);
  const detail = linkDetail(rssi);
  return (
    <span className="flex shrink-0 items-center" data-zone="link" data-severity={cell.severity}>
      <button
        type="button"
        onClick={(e) => setRect(rect ? null : e.currentTarget.getBoundingClientRect())}
        aria-label={specText("cockpit.link.named", { label: cell.text })}
        aria-expanded={rect !== null}
        className={cn(HIT_AREA, "flex h-[40px] items-center gap-[6px] whitespace-nowrap rounded-[var(--fs-radius-sm)] px-[4px] hover:bg-white/10")}
      >
        <Radio className="chrome-icon" style={{ color: severityColor(cell.severity) }} aria-hidden data-testid="link-icon" />
        {text ? <span style={cell.severity === "critical" ? { color: severityTextColor("critical") } : undefined}>{cell.text}</span> : null}
      </button>
      {rect ? (
        <Overlay
          onDismiss={() => setRect(null)}
          scrim="clear"
          panelClassName="chrome-panel chrome-bar-text w-[220px] p-[10px]"
          panelStyle={{ ...chromeStyle(), position: "absolute", left: Math.max(8, Math.min(rect.left, window.innerWidth - 228)), top: rect.bottom + 8 }}
        >
          <div data-testid="link-popover" className="flex flex-col gap-[4px]">
            <div>{cell.text}</div>
            {detail ? <div className="chrome-dim">{detail}</div> : null}
          </div>
        </Overlay>
      ) : null}
    </span>
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
      // The chip's width is capped at a fraction of the bar in `cqw`.
      style={{ containerType: "inline-size" }}
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

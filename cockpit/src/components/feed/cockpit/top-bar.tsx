// The cockpit top bar (cockpit spec `layout.json` zone `top_bar`): the
// flight-state chip, the status pill (arm state and flight time), battery
// (percent, time to reserve and the RTL-reserve marker), link, navigation
// source, the controller, the layer-level button and settings, then the
// guarded Kill. Exit to the dashboard sits first off the kiosk. Values never
// wrap; on a narrow panel the bar scrolls sideways.

import { useEffect, useState, type ReactNode } from "react";
import { ArrowLeft, Gamepad2, Layers, OctagonX, Radio, Settings, Timer } from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { FlightStateChip } from "@/components/feed/cockpit/flight-state-chip";
import { NavSourceChip } from "@/components/feed/nav-source-chip";
import { usePadStatus } from "@/hooks/use-gamepad";
import { useNow } from "@/hooks/use-now";
import { useSkillContext } from "@/hooks/use-skill-context";
import { batteryReading } from "@/lib/alerts";
import { isKiosk } from "@/lib/kiosk";
import { armLabel, fmtClock, linkLabel } from "@/lib/safety-band";
import { requestSkill } from "@/lib/skill-runner";
import { BUILTIN_SKILL_BY_ID, resolveSkillState } from "@/lib/skills";
import { cn } from "@/lib/utils";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStore } from "@/stores/nav-store";
import { useStatusStore } from "@/stores/status-store";

const LEVEL_LABEL = { full: "FULL", route: "ROUTE", off: "OFF" } as const;

function Item({ icon: Icon, tone, label, children }: { icon?: LucideIcon; tone?: string; label: string; children: ReactNode }) {
  return (
    <span className="flex shrink-0 items-center gap-[0.25rem] whitespace-nowrap font-mono text-[0.75rem]" aria-label={label}>
      {Icon ? <Icon className={cn("h-[0.9rem] w-[0.9rem]", tone ?? "text-hud-ink-2")} aria-hidden /> : null}
      <span className={tone ?? "text-hud-ink"}>{children}</span>
    </span>
  );
}

/** Battery: percent, time to reserve, and a bar with the reserve marker. */
function BatteryItem() {
  const battery = useStatusStore((s) => s.battery);
  const telemetry = useFlightStore((s) => s.telemetry);
  const r = batteryReading(battery, telemetry);
  const reserve = battery?.thresholds?.reserve_percent;
  const reservePct = typeof reserve === "number" && reserve > 0 && reserve < 100 ? reserve : null;
  if (!r) return <Item label="Battery">BAT —</Item>;
  const tone = r.pct <= (reservePct ?? 20) ? "text-err" : r.pct <= (reservePct ?? 20) + 10 ? "text-warn" : "text-hud-ink";
  return (
    <span className="flex shrink-0 items-center gap-[0.3rem] font-mono text-[0.75rem]" aria-label="Battery">
      <span className="relative h-[0.7rem] w-[2.2rem] rounded-sm border border-hud-ink-2" aria-hidden>
        <span
          className={cn("absolute inset-y-0 left-0", tone === "text-err" ? "bg-err" : tone === "text-warn" ? "bg-warn" : "bg-ok")}
          style={{ width: `${Math.max(0, Math.min(100, r.pct))}%` }}
        />
        {reservePct !== null ? (
          <span className="absolute inset-y-[-2px] w-[2px] bg-hud-ink" style={{ left: `${reservePct}%` }} data-testid="rtl-reserve-marker" />
        ) : null}
      </span>
      <span className={tone}>{Math.round(r.pct)}%</span>
      {r.timeToReserveS !== null ? <span className="text-hud-ink-2">{fmtClock(r.timeToReserveS)}</span> : null}
    </span>
  );
}

function StatusPill() {
  const live = useFlightStore((s) => s.live);
  const armed = useFlightStore((s) => s.telemetry?.armed);
  const armedSince = useFlightStore((s) => s.armedSince);
  const now = useNow(armedSince !== null ? 1000 : null);
  const arm = armLabel(live, armed);
  return (
    <span
      className={cn(
        "flex shrink-0 items-center gap-[0.3rem] rounded-full border px-[0.45rem] py-[0.1rem] font-mono text-[0.72rem]",
        arm === "ARMED" ? "border-err/70 text-err" : arm === "DISARMED" ? "border-ok/70 text-ok" : "border-hud-hair text-hud-ink-2",
      )}
      aria-label="Status"
    >
      {arm}
      {armedSince !== null ? (
        <>
          <Timer className="h-[0.75rem] w-[0.75rem]" aria-hidden />
          {fmtClock((now - armedSince) / 1000)}
        </>
      ) : null}
    </span>
  );
}

function ControllerItem() {
  const pad = usePadStatus();
  const label = !pad.connected ? "NO PAD" : pad.native ? "PAD · NATIVE" : pad.standard ? "PAD" : "PAD · CALIBRATE";
  const tone = !pad.connected ? "text-hud-ink-2" : pad.standard || pad.native ? "text-ok" : "text-warn";
  return (
    <Item icon={Gamepad2} tone={tone} label={pad.name ? `Controller ${pad.name}` : "Controller"}>
      {label}
      {pad.batteryPct !== null ? ` ${Math.round(pad.batteryPct)}%` : ""}
    </Item>
  );
}

export function TopBar({ compact = false }: { compact?: boolean }) {
  const kiosk = isKiosk();
  const status = useStatusStore((s) => s.status);
  const level = useCockpitViewStore((s) => s.level);
  const cycleLevel = useCockpitViewStore((s) => s.cycleLevel);
  const goTab = useNavStore((s) => s.goTab);
  const ctx = useSkillContext();
  const kill = BUILTIN_SKILL_BY_ID.kill;
  const killState = resolveSkillState(kill, ctx);
  const [, setTick] = useState(0);
  // The link label carries an age; re-read it once a second.
  useEffect(() => {
    const id = setInterval(() => setTick((t) => t + 1), 1000);
    return () => clearInterval(id);
  }, []);

  return (
    <div
      role="region"
      aria-label="Top bar"
      className="pointer-events-auto flex min-h-[max(2.6rem,48px)] items-center gap-[0.6rem] overflow-x-auto rounded-lg border border-hud-hair bg-hud-glass px-[0.4rem] backdrop-blur-hud"
    >
      {!kiosk ? (
        <button
          type="button"
          onClick={() => window.location.assign("/")}
          aria-label="Exit to dashboard"
          className="flex min-h-[48px] min-w-[48px] shrink-0 items-center justify-center rounded-md text-hud-ink hover:bg-hud-hair"
        >
          <ArrowLeft className="h-[1rem] w-[1rem]" aria-hidden />
        </button>
      ) : null}
      <FlightStateChip compact={compact} />
      <StatusPill />
      <BatteryItem />
      <Item icon={Radio} label="Link">
        {linkLabel(status)}
      </Item>
      {!compact ? <NavSourceChip /> : null}
      <ControllerItem />
      <button
        type="button"
        onClick={cycleLevel}
        aria-label={`Layers ${LEVEL_LABEL[level]}. Next level`}
        className="flex min-h-[48px] shrink-0 items-center gap-[0.25rem] rounded-md px-[0.4rem] font-mono text-[0.72rem] text-hud-ink hover:bg-hud-hair"
      >
        <Layers className="h-[0.95rem] w-[0.95rem]" aria-hidden />
        {LEVEL_LABEL[level]}
      </button>
      <button
        type="button"
        onClick={() => goTab("settings")}
        aria-label="Settings"
        className="flex min-h-[48px] min-w-[48px] shrink-0 items-center justify-center rounded-md text-hud-ink hover:bg-hud-hair"
      >
        <Settings className="h-[1rem] w-[1rem]" aria-hidden />
      </button>
      <button
        type="button"
        onClick={() => requestSkill(kill, ctx)}
        disabled={!killState.enabled}
        title={killState.enabled ? "Emergency stop (hold 3 s, then confirm)" : killState.reason}
        aria-label={killState.enabled ? "Emergency stop" : `Emergency stop: ${killState.reason ?? "unavailable"}`}
        className="ml-auto flex min-h-[48px] shrink-0 items-center gap-[0.25rem] whitespace-nowrap rounded-md border border-err/60 bg-err/15 px-[0.5rem] text-[0.75rem] font-semibold text-err disabled:opacity-45"
      >
        <OctagonX className="h-[0.9rem] w-[0.9rem]" aria-hidden />
        STOP
      </button>
    </div>
  );
}

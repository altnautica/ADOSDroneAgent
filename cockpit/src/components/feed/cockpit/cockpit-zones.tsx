// The cockpit grid's zones around the video (cockpit spec `layout.json`): the left
// column (Take off / Land, Return home, Pause) and the bottom telemetry row
// (D, H, H.S, V.S, AGL); the right column lives in `right-column.tsx`. Every
// control fires a cockpit action or a built-in skill through the same gates as
// a controller binding. An unavailable control stays pressable and shows its
// reason in the cockpit toast. Panels use the spec's chrome tokens (`chrome-*`
// classes); sizes are CSS px, scaled by the zone's class scale.

import { House, Pause, PlaneLanding, PlaneTakeoff } from "lucide-react";

import { useNow } from "@/hooks/use-now";
import { useFreshPilot, usePilot } from "@/hooks/use-pilot";
import { useSkillContext } from "@/hooks/use-skill-context";
import { distanceFromHomeM, readoutParts, rowAgl } from "@/lib/bottom-row";
import { leftColumnCaption } from "@/lib/cockpit-layout";
import { FLIGHT_PX } from "@/lib/flight-layout";
import { requestSkill } from "@/lib/skill-runner";
import { BUILTIN_SKILL_BY_ID, resolveSkillState } from "@/lib/skills";
import { cn } from "@/lib/utils";
import builtinsJson from "@/shared/cockpit-spec/builtins.json";
import { aglMarkText } from "@/shared/hud/chip-inputs";
import { specText } from "@/shared/hud/spec-text";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

import { groundFrom } from "./on-ground";
import { ZoneButton } from "./zone-button";

/** A built-in's full label (`builtins.json`): the button's accessible name
 *  and tooltip; the short label under the icon is `builtin.short.<id>`. */
function builtinLabel(id: string): string {
  return builtinsJson.builtins.find((x) => x.id === id)?.label ?? id;
}

/** Whether the aircraft is on the ground: the fresh pilot's `landed`, else
 *  disarmed telemetry (`on-ground.ts`). */
function useOnGround(): boolean {
  const fresh = useFreshPilot();
  const armed = useFlightStore((s) => s.telemetry?.armed === true);
  return groundFrom(fresh, armed);
}

/** The left column: Take off (opens the Launch sheet) or Land, Return home,
 *  Pause. In portrait it lays out as a row. With `labels` each button carries
 *  its short spec label under the icon. */
export function LeftColumn({ row = false, labels = false }: { row?: boolean; labels?: boolean }) {
  const ctx = useSkillContext();
  const onGround = useOnGround();
  const openSheet = useCockpitUiStore((s) => s.openSheet);
  const land = BUILTIN_SKILL_BY_ID.land;
  const rth = BUILTIN_SKILL_BY_ID.rth;
  const pause = BUILTIN_SKILL_BY_ID.pause;
  const takeoff = BUILTIN_SKILL_BY_ID.takeoff;
  const landState = resolveSkillState(land, ctx);
  const rthState = resolveSkillState(rth, ctx);
  const pauseState = resolveSkillState(pause, ctx);
  const takeoffState = resolveSkillState(takeoff, ctx);
  const caption = (id: string) => (labels ? leftColumnCaption(id) : undefined);
  return (
    <div
      className={cn("pointer-events-auto flex items-center", row ? "flex-row" : "flex-col")}
      style={{ gap: `var(--cockpit-col-gap, ${FLIGHT_PX.colGap}px)` }}
      role="group"
      aria-label={specText("cockpit.actions")}
      data-zone="left_column"
    >
      {onGround ? (
        <ZoneButton
          icon={PlaneTakeoff}
          label={builtinLabel("takeoff")}
          caption={caption("takeoff")}
          testId="left-takeoff"
          onPress={() => openSheet("launch")}
          reason={takeoffState.enabled ? null : takeoffState.reason}
        />
      ) : (
        <ZoneButton
          icon={PlaneLanding}
          label={builtinLabel("land")}
          caption={caption("land")}
          testId="left-land"
          onPress={() => requestSkill(land, ctx)}
          reason={landState.enabled ? null : landState.reason}
        />
      )}
      <ZoneButton
        icon={House}
        label={builtinLabel("rth")}
        caption={caption("rth")}
        testId="left-rth"
        onPress={() => requestSkill(rth, ctx)}
        reason={rthState.enabled ? null : rthState.reason}
      />
      <ZoneButton
        icon={Pause}
        label={builtinLabel("pause")}
        caption={caption("pause")}
        testId="left-pause"
        onPress={() => requestSkill(pause, ctx)}
        reason={pauseState.enabled ? null : pauseState.reason}
        tone="danger"
      />
    </div>
  );
}

/** One bottom-row value. `mark` names a source that is not what the label
 *  says (an AGL that is the height above home), in the warning colour; `dim`
 *  marks a value whose source has gone stale. */
function Readout({
  label,
  value,
  mark = null,
  dim = false,
}: {
  label: string;
  value: { text: string; unit: string };
  mark?: string | null;
  dim?: boolean;
}) {
  return (
    <span
      className={cn("flex flex-col items-center leading-none", dim && "opacity-45")}
      aria-label={mark ? `${label} ${mark}` : label}
      data-mark={mark ?? undefined}
      data-stale={dim || undefined}
    >
      <span className="chrome-label mb-[3px] tracking-wide">
        {label}
        {mark ? <span className="ml-[3px] text-hud-warn">{mark}</span> : null}
      </span>
      <span className="chrome-value whitespace-nowrap">
        {value.text}
        <span className="chrome-label ml-[2px] font-normal">{value.unit}</span>
      </span>
    </span>
  );
}

/** The bottom telemetry row: distance from home, height, horizontal and
 *  vertical speed, height above ground; distance and height only where the
 *  row is short. A missing source reads a dash; the AGL from a stalled pilot
 *  dims to a dash. */
export function BottomRow({ short = false }: { short?: boolean }) {
  const t = useFlightStore((s) => s.telemetry);
  const home = useFlightStore((s) => s.home);
  const pilot = usePilot();
  const follow = useNavStreamStore((s) => s.follow?.data ?? null);
  const now = useNow(pilot ? 500 : null);
  const agl = rowAgl(pilot, follow ? { aglM: follow.aglM, aglSource: follow.aglSource } : null, now);
  return (
    <div
      className="chrome-panel pointer-events-auto flex h-full w-full items-center justify-evenly px-[10px]"
      role="group"
      aria-label={specText("cockpit.telemetry")}
      data-testid="bottom-row"
      data-zone="bottom_row"
    >
      <Readout label={specText("cockpit.readout.distance")} value={readoutParts("distance", distanceFromHomeM(t, home))} />
      <Readout label={specText("cockpit.readout.height")} value={readoutParts("metres", t?.position?.alt_rel)} />
      {!short ? (
        <>
          <Readout label={specText("cockpit.readout.hspeed")} value={readoutParts("speed", t?.velocity?.groundspeed)} />
          <Readout label={specText("cockpit.readout.vspeed")} value={readoutParts("speed", t?.velocity?.climb)} />
          <Readout label={specText("cockpit.readout.agl")} value={readoutParts("metres", agl.m)} mark={aglMarkText(agl.mark)} dim={agl.stale} />
        </>
      ) : null}
    </div>
  );
}

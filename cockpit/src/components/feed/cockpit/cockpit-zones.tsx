// The cockpit grid's zones around the video (cockpit spec `layout.json`): the left
// column (Take off / Land, Return home, Pause), the right column (shutter,
// photo/video switch, zoom, gimbal pad, camera settings) and the bottom
// telemetry row (D, H, H.S, V.S, AGL). Every control fires a cockpit action or
// a built-in skill through the same gates as a controller binding. Panels use
// the spec's chrome tokens (`chrome-*` classes).

import { useRef, type ReactNode } from "react";
import {
  Camera,
  Circle,
  Crosshair,
  House,
  ImageIcon,
  Minus,
  Pause,
  PlaneLanding,
  PlaneTakeoff,
  Plus,
  SlidersHorizontal,
  Square,
  Video,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { useGimbalPadStore } from "@/hooks/use-gimbal-rate";
import { useBottomRowAgl, usePilot } from "@/hooks/use-pilot";
import { useSkillContext } from "@/hooks/use-skill-context";
import { cameraSkillFor, type CameraAction } from "@/lib/camera-actions";
import { runCockpitAction } from "@/lib/cockpit-actions";
import { cockpitText } from "@/lib/cockpit-text";
import { FLIGHT_PX, type RightItem } from "@/lib/flight-layout";
import { GIMBAL_MAX_DPS } from "@/lib/gimbal-rate";
import { requestSkill } from "@/lib/skill-runner";
import { BUILTIN_SKILL_BY_ID, resolveSkillState } from "@/lib/skills";
import { cn } from "@/lib/utils";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStore } from "@/stores/nav-store";
import { useStatusStore } from "@/stores/status-store";

function ZoneButton({
  icon: Icon,
  label,
  onClick,
  disabled,
  reason,
  tone = "default",
  size = "var(--cockpit-col)",
  testId,
}: {
  icon: LucideIcon;
  label: string;
  onClick: () => void;
  disabled?: boolean;
  reason?: string;
  tone?: "default" | "danger" | "active";
  size?: string;
  testId?: string;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      data-testid={testId}
      title={disabled && reason ? reason : label}
      aria-label={disabled && reason ? `${label}: ${reason}` : label}
      className={cn(
        "chrome-panel flex shrink-0 items-center justify-center !rounded-full transition-[transform,background-color] duration-150 ease-out active:scale-[0.96] disabled:opacity-45",
        tone === "danger" && "!border-[var(--fs-warn)] text-[var(--fs-warn)]",
        tone === "active" && "!border-[var(--fs-crit)] !bg-[var(--fs-crit)] text-[var(--fs-stop-text)]",
      )}
      style={{ width: size, height: size }}
    >
      <Icon className="h-[24px] w-[24px]" strokeWidth={1.75} aria-hidden />
    </button>
  );
}

/** Whether the aircraft is on the ground: the pilot's `landed`, else
 *  disarmed. */
function useOnGround(): boolean {
  const pilot = usePilot();
  const armed = useFlightStore((s) => s.telemetry?.armed === true);
  return pilot ? pilot.data.landed || !pilot.data.armed : !armed;
}

/** The left column: Take off (opens the Launch sheet) or Land, Return home,
 *  Pause. In portrait it lays out as a row. */
export function LeftColumn({ row = false }: { row?: boolean }) {
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
  return (
    <div
      className={cn("pointer-events-auto flex items-center", row ? "flex-row" : "flex-col")}
      style={{ gap: `var(--cockpit-col-gap, ${FLIGHT_PX.colGap}px)` }}
      role="group"
      aria-label="Flight actions"
      data-zone="left_column"
    >
      {onGround ? (
        <ZoneButton
          icon={PlaneTakeoff}
          label="Take off"
          testId="left-takeoff"
          onClick={() => openSheet("launch")}
          disabled={!takeoffState.enabled}
          reason={takeoffState.reason}
        />
      ) : (
        <ZoneButton
          icon={PlaneLanding}
          label="Land"
          testId="left-land"
          onClick={() => requestSkill(land, ctx)}
          disabled={!landState.enabled}
          reason={landState.reason}
        />
      )}
      <ZoneButton
        icon={House}
        label="Return home"
        testId="left-rth"
        onClick={() => requestSkill(rth, ctx)}
        disabled={!rthState.enabled}
        reason={rthState.reason}
      />
      <ZoneButton
        icon={Pause}
        label="Pause"
        testId="left-pause"
        onClick={() => requestSkill(pause, ctx)}
        disabled={!pauseState.enabled}
        reason={pauseState.reason}
        tone="danger"
      />
    </div>
  );
}

function useCameraAction(action: CameraAction): { run: () => void; reason: string | null } {
  const extensions = useExtensionsStore((s) => s.extensions);
  const skill = cameraSkillFor(action, extensions);
  return skill
    ? { run: () => runCockpitAction(`skill:${skill}`), reason: null }
    : { run: () => undefined, reason: "No camera extension offers this" };
}

/** Zoom in over zoom out, one pill. */
function ZoomPill() {
  const zoomIn = useCameraAction("zoom_in");
  const zoomOut = useCameraAction("zoom_out");
  const btn = "flex h-1/2 w-full items-center justify-center disabled:opacity-45 active:bg-white/10";
  return (
    <div
      className="chrome-panel flex flex-col overflow-hidden !rounded-full"
      style={{ width: "var(--cockpit-col)", height: FLIGHT_PX.zoomH }}
      role="group"
      aria-label="Zoom"
      data-zone="zoom"
    >
      <button type="button" className={btn} onClick={zoomIn.run} disabled={zoomIn.reason !== null} title={zoomIn.reason ?? "Zoom in"} aria-label="Zoom in">
        <Plus className="h-[20px] w-[20px]" aria-hidden />
      </button>
      <span className="mx-[12px] h-px bg-[var(--fs-border)]" aria-hidden />
      <button type="button" className={btn} onClick={zoomOut.run} disabled={zoomOut.reason !== null} title={zoomOut.reason ?? "Zoom out"} aria-label="Zoom out">
        <Minus className="h-[20px] w-[20px]" aria-hidden />
      </button>
    </div>
  );
}

/** The gimbal pad: drag from the centre to aim (rate grows with the drag);
 *  release stops it; a tap on the centre recentres. Shown only where the
 *  vehicle takes gimbal rates. */
function GimbalPad() {
  const setRate = useGimbalPadStore((s) => s.setRate);
  const pitch = useGimbalPadStore((s) => s.pitch);
  const yaw = useGimbalPadStore((s) => s.yaw);
  const recenter = useCameraAction("recenter");
  const origin = useRef<{ x: number; y: number; r: number } | null>(null);
  const size = FLIGHT_PX.gimbal;
  const knob = 36;
  const travel = (size - knob) / 2;
  const kx = (yaw / GIMBAL_MAX_DPS) * travel;
  const ky = (-pitch / GIMBAL_MAX_DPS) * travel;
  const release = () => {
    origin.current = null;
    setRate(0, 0);
  };
  return (
    <div
      className="chrome-panel relative shrink-0 touch-none !rounded-full"
      style={{ width: size, height: size }}
      role="group"
      aria-label={cockpitText("cockpit.gimbal_pad")}
      title={cockpitText("cockpit.gimbal_pad.hint")}
      data-zone="gimbal_pad"
      onPointerDown={(e) => {
        const r = e.currentTarget.getBoundingClientRect();
        e.currentTarget.setPointerCapture?.(e.pointerId);
        origin.current = { x: r.left + r.width / 2, y: r.top + r.height / 2, r: r.width / 2 };
      }}
      onPointerMove={(e) => {
        const o = origin.current;
        if (!o) return;
        const dx = Math.max(-1, Math.min(1, (e.clientX - o.x) / o.r));
        const dy = Math.max(-1, Math.min(1, (e.clientY - o.y) / o.r));
        // A small dead zone round the centre so a tap does not nudge.
        const shape = (v: number) => (Math.abs(v) < 0.15 ? 0 : Math.sign(v) * ((Math.abs(v) - 0.15) / 0.85) ** 1.5);
        setRate(-shape(dy) * GIMBAL_MAX_DPS, shape(dx) * GIMBAL_MAX_DPS);
      }}
      onPointerUp={release}
      onPointerCancel={release}
    >
      {[0, 90, 180, 270].map((deg) => (
        <span
          key={deg}
          className="absolute left-1/2 top-1/2 h-0 w-0 border-x-[5px] border-b-[6px] border-x-transparent border-b-[var(--fs-text-dim)]"
          style={{ transform: `translate(-50%, -50%) rotate(${deg}deg) translateY(${-(size / 2 - 10)}px)` }}
          aria-hidden
        />
      ))}
      <button
        type="button"
        onClick={recenter.run}
        disabled={recenter.reason !== null}
        aria-label="Recentre gimbal"
        title={recenter.reason ?? "Recentre gimbal"}
        className="absolute left-1/2 top-1/2 flex items-center justify-center rounded-full border border-[var(--fs-border)] bg-white/15 transition-transform duration-100 ease-out disabled:opacity-60"
        style={{ width: knob, height: knob, transform: `translate(calc(-50% + ${kx}px), calc(-50% + ${ky}px))` }}
      >
        <Crosshair className="h-[16px] w-[16px]" aria-hidden />
      </button>
    </div>
  );
}

/** The right column: shutter, photo/video switch, zoom, gimbal pad and the
 *  camera settings, as the layout fits them. Photo, zoom and recentre drive
 *  the installed camera extension's skills; video drives this node's
 *  recorder; the pad sends gimbal rates. */
export function RightColumn({ items }: { items: readonly RightItem[] }) {
  const mode = useCockpitUiStore((s) => s.shutterMode);
  const setMode = useCockpitUiStore((s) => s.setShutterMode);
  const recording = useStatusStore((s) => s.status?.recording === true || s.status?.video?.recording === true);
  const photo = useCameraAction("photo");
  const goTab = useNavStore((s) => s.goTab);
  const render = (item: RightItem): ReactNode => {
    switch (item) {
      case "shutter":
        return (
          <ZoneButton
            key={item}
            icon={mode === "photo" ? Camera : recording ? Square : Circle}
            label={mode === "photo" ? "Take photo" : recording ? "Stop recording" : "Start recording"}
            testId="shutter"
            size="var(--cockpit-shutter)"
            tone={mode === "video" && recording ? "active" : "default"}
            onClick={() => runCockpitAction(mode === "photo" ? "camera.photo" : "camera.record")}
            disabled={mode === "photo" && photo.reason !== null}
            reason={photo.reason ?? undefined}
          />
        );
      case "mode":
        return (
          <div key={item} className="chrome-panel flex shrink-0 overflow-hidden !rounded-full p-[2px]" style={{ height: FLIGHT_PX.modeH }} role="radiogroup" aria-label="Shutter mode">
            {(["photo", "video"] as const).map((m) => (
              <button
                key={m}
                type="button"
                role="radio"
                aria-checked={mode === m}
                aria-label={m === "photo" ? "Photo" : "Video"}
                onClick={() => setMode(m)}
                className={cn(
                  "flex h-full w-[30px] items-center justify-center rounded-full transition-colors duration-150",
                  mode === m ? "bg-white/90 text-black" : "chrome-dim",
                )}
              >
                {m === "photo" ? <ImageIcon className="h-[15px] w-[15px]" aria-hidden /> : <Video className="h-[15px] w-[15px]" aria-hidden />}
              </button>
            ))}
          </div>
        );
      case "zoom":
        return <ZoomPill key={item} />;
      case "gimbal":
        return <GimbalPad key={item} />;
      case "settings":
        return <ZoneButton key={item} icon={SlidersHorizontal} label="Camera settings" onClick={() => goTab("video")} />;
    }
  };
  return (
    <div className="pointer-events-auto flex flex-col items-center" style={{ gap: FLIGHT_PX.colGap }} role="group" aria-label="Camera" data-zone="right_column">
      {items.map(render)}
    </div>
  );
}

function Readout({ label, value, unit }: { label: string; value: number | null; unit: string }) {
  return (
    <span className="flex flex-col items-center leading-none" aria-label={label}>
      <span className="chrome-label mb-[3px] tracking-wide">{label}</span>
      <span className="chrome-value whitespace-nowrap">
        {value === null || !Number.isFinite(value) ? "—" : value.toFixed(Math.abs(value) < 10 ? 1 : 0)}
        <span className="chrome-label ml-[2px] font-normal">{unit}</span>
      </span>
    </span>
  );
}

/** The bottom telemetry row: distance from home, height, horizontal and
 *  vertical speed, height above ground; distance and height only where the
 *  row is short. A missing source reads a dash. */
export function BottomRow({ short = false }: { short?: boolean }) {
  const t = useFlightStore((s) => s.telemetry);
  const agl = useBottomRowAgl();
  return (
    <div
      className="chrome-panel pointer-events-auto flex h-full w-full items-center justify-evenly px-[10px]"
      role="group"
      aria-label="Telemetry"
      data-testid="bottom-row"
      data-zone="bottom_row"
    >
      <Readout label="D" value={t?.home_distance ?? null} unit="m" />
      <Readout label="H" value={t?.position?.alt_rel ?? null} unit="m" />
      {!short ? (
        <>
          <Readout label="H.S" value={t?.velocity?.groundspeed ?? null} unit="m/s" />
          <Readout label="V.S" value={t?.velocity?.climb ?? null} unit="m/s" />
          <Readout label="AGL" value={agl} unit="m" />
        </>
      ) : null}
    </div>
  );
}

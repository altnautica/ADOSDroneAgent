// The DJI-grid zones around the video (cockpit spec `layout.json`): the left
// column (Take off / Land, Return home, Pause), the right column (shutter,
// photo/video switch, zoom, gimbal pad, camera settings) and the bottom
// telemetry row (D, H, H.S, V.S, AGL). Every control fires a cockpit action or
// a built-in skill through the same gates as a controller binding.

import type { ReactNode } from "react";
import {
  Camera,
  Circle,
  Crosshair,
  House,
  ImageIcon,
  Pause,
  PlaneLanding,
  PlaneTakeoff,
  SlidersHorizontal,
  Square,
  Video,
  ZoomIn,
  ZoomOut,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { usePilot } from "@/hooks/use-pilot";
import { useSkillContext } from "@/hooks/use-skill-context";
import { cameraSkillFor, type CameraAction } from "@/lib/camera-actions";
import { runCockpitAction } from "@/lib/cockpit-actions";
import { requestSkill } from "@/lib/skill-runner";
import { BUILTIN_SKILL_BY_ID, resolveSkillState } from "@/lib/skills";
import { cn } from "@/lib/utils";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";
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
  showLabel = true,
  testId,
}: {
  icon: LucideIcon;
  label: string;
  onClick: () => void;
  disabled?: boolean;
  reason?: string;
  tone?: "default" | "danger" | "active";
  size?: string;
  showLabel?: boolean;
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
        "flex shrink-0 flex-col items-center justify-center gap-[0.15rem] rounded-full border backdrop-blur-hud transition-colors duration-quick disabled:opacity-45",
        tone === "danger"
          ? "border-err/60 bg-err/20 text-err"
          : tone === "active"
            ? "border-err bg-err/80 text-on-status"
            : "border-hud-hair bg-hud-glass text-hud-ink hover:bg-hud-glass-strong",
      )}
      style={{ width: size, height: size }}
    >
      <Icon className="h-[45%] w-[45%] max-h-[1.6rem] max-w-[1.6rem]" aria-hidden />
      {showLabel ? <span className="sr-only">{label}</span> : null}
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
      className={cn("pointer-events-auto flex items-center gap-[0.6rem]", row ? "flex-row" : "flex-col")}
      role="group"
      aria-label="Flight actions"
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

/** The right column: shutter, photo/video switch, zoom, gimbal pad and the
 *  camera settings. Photo, zoom and gimbal drive the installed camera
 *  extension's skills; video drives this node's recorder. */
export function RightColumn({ compact = false }: { compact?: boolean }) {
  const mode = useCockpitUiStore((s) => s.shutterMode);
  const setMode = useCockpitUiStore((s) => s.setShutterMode);
  const recording = useStatusStore((s) => s.status?.recording === true || s.status?.video?.recording === true);
  const photo = useCameraAction("photo");
  const zoomIn = useCameraAction("zoom_in");
  const zoomOut = useCameraAction("zoom_out");
  const recenter = useCameraAction("recenter");
  const goTab = useNavStore((s) => s.goTab);
  const shutterSize = "var(--cockpit-shutter)";
  return (
    <div className="pointer-events-auto flex flex-col items-center gap-[0.5rem]" role="group" aria-label="Camera">
      <ZoneButton
        icon={mode === "photo" ? Camera : recording ? Square : Circle}
        label={mode === "photo" ? "Take photo" : recording ? "Stop recording" : "Start recording"}
        testId="shutter"
        size={shutterSize}
        tone={mode === "video" && recording ? "active" : "default"}
        onClick={() => runCockpitAction(mode === "photo" ? "camera.photo" : "camera.record")}
        disabled={mode === "photo" && photo.reason !== null}
        reason={photo.reason ?? undefined}
      />
      <div className="flex overflow-hidden rounded-full border border-hud-hair bg-hud-glass backdrop-blur-hud" role="radiogroup" aria-label="Shutter mode">
        {(["photo", "video"] as const).map((m) => (
          <button
            key={m}
            type="button"
            role="radio"
            aria-checked={mode === m}
            aria-label={m === "photo" ? "Photo" : "Video"}
            onClick={() => setMode(m)}
            className={cn("flex h-[2rem] w-[2.2rem] items-center justify-center", mode === m ? "bg-primary/40 text-hud-ink" : "text-hud-ink-2")}
          >
            {m === "photo" ? <ImageIcon className="h-[0.9rem] w-[0.9rem]" aria-hidden /> : <Video className="h-[0.9rem] w-[0.9rem]" aria-hidden />}
          </button>
        ))}
      </div>
      {!compact ? (
        <>
          <ZoneButton icon={ZoomIn} label="Zoom in" onClick={zoomIn.run} disabled={zoomIn.reason !== null} reason={zoomIn.reason ?? undefined} />
          <ZoneButton icon={ZoomOut} label="Zoom out" onClick={zoomOut.run} disabled={zoomOut.reason !== null} reason={zoomOut.reason ?? undefined} />
          <ZoneButton
            icon={Crosshair}
            label="Recentre gimbal"
            onClick={recenter.run}
            disabled={recenter.reason !== null}
            reason={recenter.reason ?? undefined}
          />
        </>
      ) : null}
      <ZoneButton icon={SlidersHorizontal} label="Camera settings" onClick={() => goTab("video")} />
    </div>
  );
}

function Readout({ label, value, unit, distanceOnly }: { label: string; value: number | null; unit: string; distanceOnly: boolean }) {
  return (
    <span className="flex items-baseline gap-[0.25rem] whitespace-nowrap font-mono" aria-label={label}>
      {!distanceOnly ? <span className="text-[0.68rem] text-hud-ink-2">{label}</span> : null}
      <span className="text-[0.95rem] font-semibold text-hud-ink">
        {value === null || !Number.isFinite(value) ? "—" : value.toFixed(Math.abs(value) < 10 ? 1 : 0)}
      </span>
      <span className="text-[0.68rem] text-hud-ink-2">{unit}</span>
    </span>
  );
}

/** The bottom telemetry row: distance from home, height, horizontal and
 *  vertical speed, height above ground. A missing source reads a dash. */
export function BottomRow({ distanceOnly = false }: { distanceOnly?: boolean }) {
  const t = useFlightStore((s) => s.telemetry);
  const agl = useNavStreamStore((s) => s.follow?.data.aglM ?? null);
  const items: ReactNode[] = [
    <Readout key="d" label="D" value={t?.home_distance ?? null} unit="m" distanceOnly={distanceOnly} />,
    <Readout key="h" label="H" value={t?.position?.alt_rel ?? null} unit="m" distanceOnly={distanceOnly} />,
  ];
  if (!distanceOnly) {
    items.push(
      <Readout key="hs" label="H.S" value={t?.velocity?.groundspeed ?? null} unit="m/s" distanceOnly={false} />,
      <Readout key="vs" label="V.S" value={t?.velocity?.climb ?? null} unit="m/s" distanceOnly={false} />,
      <Readout key="agl" label="AGL" value={agl} unit="m" distanceOnly={false} />,
    );
  }
  return (
    <div
      className="pointer-events-auto flex items-center gap-[0.9rem] rounded-full border border-hud-hair bg-hud-glass px-[0.8rem] py-[0.3rem] backdrop-blur-hud"
      role="group"
      aria-label="Telemetry"
      data-testid="bottom-row"
    >
      {items}
    </div>
  );
}

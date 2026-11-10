// The cockpit grid's zones around the video (cockpit spec `layout.json`): the left
// column (Take off / Land, Return home, Pause), the right column (shutter,
// photo/video switch, zoom, gimbal pad, camera settings) and the bottom
// telemetry row (D, H, H.S, V.S, AGL). Every control fires a cockpit action or
// a built-in skill through the same gates as a controller binding. An
// unavailable control stays pressable and shows its reason in the cockpit
// toast. Panels use the spec's chrome tokens (`chrome-*` classes); sizes are
// CSS px, scaled by the zone's class scale.

import { useRef, useState, type KeyboardEvent, type ReactNode } from "react";
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

import { Sheet } from "@/components/ui/sheet";
import { useGimbalPadStore } from "@/hooks/use-gimbal-rate";
import { useNow } from "@/hooks/use-now";
import { usePilot } from "@/hooks/use-pilot";
import { useSkillContext } from "@/hooks/use-skill-context";
import { distanceFromHomeM, rowAgl } from "@/lib/bottom-row";
import { cameraRoute, recordRoute, runCameraAction, type CameraAction } from "@/lib/camera-actions";
import { runCockpitAction } from "@/lib/cockpit-actions";
import { cameraLabel } from "@/lib/feed-legs";
import { FLIGHT_PX, type RightItem } from "@/lib/flight-layout";
import { GIMBAL_KEY_DPS, GIMBAL_MAX_DPS } from "@/lib/gimbal-rate";
import { requestSkill } from "@/lib/skill-runner";
import { BUILTIN_SKILL_BY_ID, resolveSkillState } from "@/lib/skills";
import type { RosterCamera } from "@/lib/types";
import { cn } from "@/lib/utils";
import { pressOrExplain } from "@/lib/zone-press";
import builtinsJson from "@/shared/cockpit-spec/builtins.json";
import { aglMarkText } from "@/shared/hud/chip-inputs";
import { specText } from "@/shared/hud/spec-text";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useFeedStore } from "@/stores/feed-store";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";
import { useStatusStore } from "@/stores/status-store";

/** A built-in's label for the left column: its short label when the spec
 *  gives one, else its label (`builtins.json`). */
function builtinLabel(id: string): string {
  const b = builtinsJson.builtins.find((x) => x.id === id);
  if (!b) return id;
  return "short_label" in b && typeof b.short_label === "string" ? b.short_label : b.label;
}

function ZoneButton({
  icon: Icon,
  label,
  caption,
  onPress,
  reason,
  tone = "default",
  size = "var(--cockpit-col)",
  testId,
}: {
  icon: LucideIcon;
  label: string;
  /** A short label drawn under the icon. */
  caption?: string;
  onPress: () => void;
  /** Why the control is unavailable now; a press shows it. */
  reason?: string | null;
  tone?: "default" | "danger" | "active";
  size?: string;
  testId?: string;
}) {
  const circle = cn(
    "chrome-panel flex shrink-0 items-center justify-center !rounded-full transition-[transform,background-color] duration-150 ease-out",
    tone === "danger" && "!border-[var(--fs-warn)] text-[var(--fs-warn)]",
    tone === "active" && "!border-[var(--fs-crit)] !bg-[var(--fs-crit)] text-[var(--fs-stop-text)]",
  );
  const icon = <Icon className="h-[24px] w-[24px]" strokeWidth={1.75} aria-hidden />;
  return (
    <button
      type="button"
      onClick={() => pressOrExplain(reason, onPress)}
      aria-disabled={reason ? true : undefined}
      data-testid={testId}
      title={reason ?? label}
      aria-label={reason ? `${label}: ${reason}` : label}
      className={cn(
        "flex shrink-0 flex-col items-center active:scale-[0.96] aria-disabled:opacity-45",
        caption === undefined && circle,
      )}
      style={caption === undefined ? { width: size, height: size } : { minWidth: size }}
    >
      {caption === undefined ? (
        icon
      ) : (
        <>
          <span className={circle} style={{ width: size, height: size }}>
            {icon}
          </span>
          <span
            className="chrome-label whitespace-nowrap text-center font-medium"
            style={{ height: FLIGHT_PX.leftLabelH, fontSize: 11, lineHeight: `${FLIGHT_PX.leftLabelH}px` }}
            aria-hidden
          >
            {caption}
          </span>
        </>
      )}
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
  const caption = (id: string) => (labels ? builtinLabel(id) : undefined);
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

function useVideoLive(): boolean {
  return useFeedStore((s) => s.video.state === "live");
}

/** Why a camera action is unavailable now, or null. */
function useCameraReason(action: CameraAction): string | null {
  const extensions = useExtensionsStore((s) => s.extensions);
  const route = cameraRoute(action, extensions, useVideoLive());
  return route.kind === "unavailable" ? route.reason : null;
}

/** Zoom in over zoom out, one pill. Without a camera extension's zoom the
 *  pill zooms the shown video digitally and names the level beside it. */
function ZoomPill() {
  const inReason = useCameraReason("zoom_in");
  const outReason = useCameraReason("zoom_out");
  const extensions = useExtensionsStore((s) => s.extensions);
  const digital = cameraRoute("zoom_in", extensions, true).kind === "local";
  const zoom = useCockpitViewStore((s) => s.digitalZoom);
  const level = digital ? specText("camera.digital_zoom", { n: zoom }) : null;
  const btn = "flex h-1/2 w-full items-center justify-center active:bg-white/10 aria-disabled:opacity-45";
  const press = (action: CameraAction, reason: string | null) => () => pressOrExplain(reason, () => runCameraAction(action));
  const inLabel = specText("cockpit.zoom.in");
  const outLabel = specText("cockpit.zoom.out");
  return (
    <div
      className="chrome-panel relative flex flex-col !rounded-full"
      style={{ width: "var(--cockpit-col)", height: FLIGHT_PX.zoomH }}
      role="group"
      aria-label={level ?? specText("cockpit.zoom")}
      data-zone="zoom"
    >
      <button type="button" className={btn} onClick={press("zoom_in", inReason)} aria-disabled={inReason ? true : undefined} title={inReason ?? inLabel} aria-label={inReason ? `${inLabel}: ${inReason}` : inLabel}>
        <Plus className="h-[20px] w-[20px]" aria-hidden />
      </button>
      <span className="mx-[12px] h-px bg-[var(--fs-border)]" aria-hidden />
      <button type="button" className={btn} onClick={press("zoom_out", outReason)} aria-disabled={outReason ? true : undefined} title={outReason ?? outLabel} aria-label={outReason ? `${outLabel}: ${outReason}` : outLabel}>
        <Minus className="h-[20px] w-[20px]" aria-hidden />
      </button>
      {level && zoom > 1 ? (
        <span
          className="chrome-panel chrome-label absolute right-full top-1/2 mr-[6px] -translate-y-1/2 whitespace-nowrap !rounded-full px-[8px]"
          style={{ fontSize: 12, lineHeight: "24px" }}
          data-testid="digital-zoom"
        >
          {level}
        </span>
      ) : null}
    </div>
  );
}

const ARROW_RATE: Readonly<Record<string, [number, number]>> = {
  ArrowUp: [GIMBAL_KEY_DPS, 0],
  ArrowDown: [-GIMBAL_KEY_DPS, 0],
  ArrowLeft: [0, -GIMBAL_KEY_DPS],
  ArrowRight: [0, GIMBAL_KEY_DPS],
};

/** The gimbal pad: drag from the centre to aim (rate grows with the drag);
 *  release stops it; a tap on the centre recentres. With the pad focused the
 *  arrow keys aim at a fixed rate while held and Enter recentres; Space is
 *  left to the rest of the cockpit. Shown only where the vehicle takes
 *  gimbal rates. */
function GimbalPad() {
  const setRate = useGimbalPadStore((s) => s.setRate);
  const pitch = useGimbalPadStore((s) => s.pitch);
  const yaw = useGimbalPadStore((s) => s.yaw);
  const recenterReason = useCameraReason("recenter");
  const recenter = () => pressOrExplain(recenterReason, () => runCameraAction("recenter"));
  const origin = useRef<{ x: number; y: number; r: number; captured: boolean } | null>(null);
  const keys = useRef(new Set<string>());
  const size = FLIGHT_PX.gimbal;
  const knob = FLIGHT_PX.knob;
  const travel = (size - knob) / 2;
  const kx = (yaw / GIMBAL_MAX_DPS) * travel;
  const ky = (-pitch / GIMBAL_MAX_DPS) * travel;
  const release = () => {
    origin.current = null;
    setRate(0, 0);
  };
  const keyRate = () => {
    let p = 0;
    let y = 0;
    for (const k of keys.current) {
      p += ARROW_RATE[k][0];
      y += ARROW_RATE[k][1];
    }
    setRate(p, y);
  };
  const recentreLabel = specText("cockpit.gimbal.recentre");
  return (
    <div
      className="chrome-panel relative shrink-0 touch-none !rounded-full"
      style={{ width: size, height: size }}
      role="group"
      tabIndex={0}
      aria-label={specText("cockpit.gimbal_pad")}
      title={specText("cockpit.gimbal_pad.hint")}
      data-zone="gimbal_pad"
      onKeyDown={(e: KeyboardEvent<HTMLDivElement>) => {
        if (e.target !== e.currentTarget) return;
        if (e.key === "Enter") {
          e.preventDefault();
          recenter();
          return;
        }
        if (!(e.key in ARROW_RATE)) return;
        e.preventDefault();
        if (keys.current.has(e.key)) return;
        keys.current.add(e.key);
        keyRate();
      }}
      onKeyUp={(e) => {
        if (!keys.current.delete(e.key)) return;
        keyRate();
      }}
      onBlur={() => {
        if (keys.current.size === 0) return;
        keys.current.clear();
        setRate(0, 0);
      }}
      onPointerDown={(e) => {
        const r = e.currentTarget.getBoundingClientRect();
        // A press on the recentre button is not captured: capture retargets
        // the mouse click to the pad and the button would never run. Any
        // other press captures at once, so the release always reaches the pad.
        const onKnob = (e.target as Element).closest("button") !== null;
        if (!onKnob) e.currentTarget.setPointerCapture?.(e.pointerId);
        origin.current = { x: r.left + r.width / 2, y: r.top + r.height / 2, r: r.width / 2, captured: !onKnob };
      }}
      onPointerMove={(e) => {
        const o = origin.current;
        if (!o) return;
        // No button down: the release happened elsewhere, so the drag is over.
        if (e.buttons === 0) {
          release();
          return;
        }
        const dx = Math.max(-1, Math.min(1, (e.clientX - o.x) / o.r));
        const dy = Math.max(-1, Math.min(1, (e.clientY - o.y) / o.r));
        // A small dead zone round the centre so a tap does not nudge.
        const shape = (v: number) => (Math.abs(v) < 0.15 ? 0 : Math.sign(v) * ((Math.abs(v) - 0.15) / 0.85) ** 1.5);
        const ratePitch = -shape(dy) * GIMBAL_MAX_DPS;
        const rateYaw = shape(dx) * GIMBAL_MAX_DPS;
        if (!o.captured && (ratePitch !== 0 || rateYaw !== 0)) {
          o.captured = true;
          e.currentTarget.setPointerCapture?.(e.pointerId);
        }
        setRate(ratePitch, rateYaw);
      }}
      onPointerUp={release}
      onPointerCancel={release}
      onLostPointerCapture={release}
    >
      {[0, 90, 180, 270].map((deg) => (
        <span
          key={deg}
          className="absolute left-1/2 top-1/2 h-0 w-0 border-x-[5px] border-b-[6px] border-x-transparent border-b-[var(--fs-text-dim)]"
          style={{ transform: `translate(-50%, -50%) rotate(${deg}deg) translateY(${-(size / 2 - 10)}px)` }}
          aria-hidden
        />
      ))}
      {/* The knob is drawn 36 px; its button is the 48 px touch floor. */}
      <button
        type="button"
        tabIndex={-1}
        onClick={recenter}
        aria-disabled={recenterReason ? true : undefined}
        aria-label={recenterReason ? `${recentreLabel}: ${recenterReason}` : recentreLabel}
        title={recenterReason ?? recentreLabel}
        className="group absolute left-1/2 top-1/2 flex items-center justify-center rounded-full transition-transform duration-100 ease-out"
        style={{ width: FLIGHT_PX.knobHit, height: FLIGHT_PX.knobHit, transform: `translate(calc(-50% + ${kx}px), calc(-50% + ${ky}px))` }}
      >
        <span
          className="flex items-center justify-center rounded-full border border-[var(--fs-border)] bg-white/15 group-aria-disabled:opacity-60"
          style={{ width: knob, height: knob }}
        >
          <Crosshair className="h-[16px] w-[16px]" aria-hidden />
        </span>
      </button>
    </div>
  );
}

/** The camera settings over the flight screen: which camera the main video
 *  shows, and the recorder. */
function CameraSettingsSheet({ cameras, onClose }: { cameras: readonly RosterCamera[]; onClose: () => void }) {
  const active = useFeedStore((s) => s.mainCameraId);
  const setActive = useFeedStore((s) => s.setActiveCamera);
  const recording = useStatusStore((s) => s.status?.recording === true || s.status?.video?.recording === true);
  const recordReason = recordRoute(useVideoLive()).reason;
  return (
    <Sheet title={specText("cockpit.camera.settings")} onDismiss={onClose} closeLabel={specText("cockpit.close")}>
      <div className="flex flex-col gap-[16px] px-[14px] pb-[14px]" data-testid="camera-settings-sheet">
        {cameras.length > 1 ? (
          <section role="radiogroup" aria-label={specText("cockpit.menu.cameras")} className="flex flex-col gap-[6px]">
            <h3 className="text-[13px] font-semibold text-muted-foreground">{specText("cockpit.menu.cameras")}</h3>
            {cameras.map((c) => (
              <button
                key={c.id}
                type="button"
                role="radio"
                aria-checked={c.id === active}
                onClick={() => setActive(c.id)}
                className={cn(
                  "flex min-h-[48px] items-center rounded-lg border px-[12px] text-left text-[15px]",
                  c.id === active ? "border-primary bg-primary/10" : "border-border",
                )}
              >
                {cameraLabel(c)}
              </button>
            ))}
          </section>
        ) : null}
        <button
          type="button"
          onClick={() => pressOrExplain(recordReason, () => runCockpitAction("camera.record"))}
          aria-disabled={recordReason ? true : undefined}
          title={recordReason ?? undefined}
          className="flex min-h-[48px] items-center justify-center gap-[8px] rounded-lg border border-border text-[15px] font-semibold aria-disabled:opacity-45"
        >
          {recording ? <Square className="h-[18px] w-[18px]" aria-hidden /> : <Circle className="h-[18px] w-[18px] text-err" aria-hidden />}
          {specText(recording ? "cockpit.camera.stop_recording" : "cockpit.camera.start_recording")}
        </button>
      </div>
    </Sheet>
  );
}

/** The right column: shutter, photo/video switch, zoom, gimbal pad and the
 *  camera settings, as the layout fits them. Photo, zoom and recentre go
 *  through the camera chain (`lib/camera-actions`); video drives this node's
 *  recorder; the pad sends gimbal rates; settings open a sheet over the
 *  flight screen. */
export function RightColumn({ items, cameras }: { items: readonly RightItem[]; cameras: readonly RosterCamera[] }) {
  const mode = useCockpitUiStore((s) => s.shutterMode);
  const setMode = useCockpitUiStore((s) => s.setShutterMode);
  const recording = useStatusStore((s) => s.status?.recording === true || s.status?.video?.recording === true);
  const photoReason = useCameraReason("photo");
  const recordReason = recordRoute(useVideoLive()).reason;
  const [settings, setSettings] = useState(false);
  const render = (item: RightItem): ReactNode => {
    switch (item) {
      case "shutter":
        return (
          <ZoneButton
            key={item}
            icon={mode === "photo" ? Camera : recording ? Square : Circle}
            label={specText(mode === "photo" ? "cockpit.camera.take_photo" : recording ? "cockpit.camera.stop_recording" : "cockpit.camera.start_recording")}
            testId="shutter"
            size="var(--cockpit-shutter)"
            tone={mode === "video" && recording ? "active" : "default"}
            onPress={() => runCockpitAction(mode === "photo" ? "camera.photo" : "camera.record")}
            reason={mode === "photo" ? photoReason : recordReason}
          />
        );
      case "photo_video_switch": {
        // One control: the whole switch toggles, so its hit area is the
        // touch floor whatever the segments' drawn size.
        const next = mode === "photo" ? "video" : "photo";
        return (
          <button
            key={item}
            type="button"
            role="switch"
            aria-checked={mode === "video"}
            aria-label={specText("cockpit.camera.shutter_mode")}
            title={specText(mode === "photo" ? "cockpit.camera.photo" : "cockpit.camera.video")}
            onClick={() => setMode(next)}
            className="flex shrink-0 items-center justify-center"
            style={{ width: FLIGHT_PX.modeW, height: FLIGHT_PX.modeH }}
            data-zone="photo_video_switch"
          >
            <span className="chrome-panel flex h-[36px] w-full overflow-hidden !rounded-full p-[2px]" aria-hidden>
              {(["photo", "video"] as const).map((m) => (
                <span
                  key={m}
                  className={cn(
                    "flex h-full flex-1 items-center justify-center rounded-full transition-colors duration-150",
                    mode === m ? "bg-white/90 text-black" : "chrome-dim",
                  )}
                >
                  {m === "photo" ? <ImageIcon className="h-[15px] w-[15px]" /> : <Video className="h-[15px] w-[15px]" />}
                </span>
              ))}
            </span>
          </button>
        );
      }
      case "zoom":
        return <ZoomPill key={item} />;
      case "gimbal_pad":
        return <GimbalPad key={item} />;
      case "camera_settings":
        return (
          <ZoneButton
            key={item}
            icon={SlidersHorizontal}
            label={specText("cockpit.camera.settings")}
            testId="camera-settings"
            onPress={() => setSettings(true)}
          />
        );
    }
  };
  return (
    <div
      className="pointer-events-auto flex flex-col items-center"
      style={{ gap: FLIGHT_PX.colGap }}
      role="group"
      aria-label={specText("skill.picker.tab.camera")}
      data-zone="right_column"
    >
      {items.map(render)}
      {settings ? <CameraSettingsSheet cameras={cameras} onClose={() => setSettings(false)} /> : null}
    </div>
  );
}

/** One bottom-row value. `mark` names a source that is not what the label
 *  says (an AGL that is the height above home), in the warning colour; `dim`
 *  marks a value whose source has gone stale. */
function Readout({
  label,
  value,
  unit,
  mark = null,
  dim = false,
}: {
  label: string;
  value: number | null;
  unit: string;
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
        {value === null || !Number.isFinite(value) ? "—" : value.toFixed(Math.abs(value) < 10 ? 1 : 0)}
        <span className="chrome-label ml-[2px] font-normal">{unit}</span>
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
      <Readout label={specText("cockpit.readout.distance")} value={distanceFromHomeM(t, home)} unit="m" />
      <Readout label={specText("cockpit.readout.height")} value={t?.position?.alt_rel ?? null} unit="m" />
      {!short ? (
        <>
          <Readout label={specText("cockpit.readout.hspeed")} value={t?.velocity?.groundspeed ?? null} unit="m/s" />
          <Readout label={specText("cockpit.readout.vspeed")} value={t?.velocity?.climb ?? null} unit="m/s" />
          <Readout label={specText("cockpit.readout.agl")} value={agl.m} unit="m" mark={aglMarkText(agl.mark)} dim={agl.stale} />
        </>
      ) : null}
    </div>
  );
}

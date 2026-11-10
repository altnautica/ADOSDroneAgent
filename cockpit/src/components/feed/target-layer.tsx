// The host target layer (CS-1, z 12) over one video: the vision engine's
// detections and follow-me's phase drawn in the HUD v2 symbology
// (13-hud-v2 §5), above the extensions' `video.overlay` frames (z 10) and
// under the instrument HUD (z 15). Every state comes from the shared resolver
// (`@/shared/hud/target-state`) and is laid out by the shared geometry, so the
// cockpit draws exactly what Mission Control and the Android app draw.
//
// Boxes arrive in the inference frame's pixels and are placed inside the
// letterboxed rendered rect, never as a percentage of the panel. The batch
// ages like every layer (`layers.json` `staleness`): past 500 ms its boxes
// draw at half alpha, past 2 s they hide and, while a detector runs, the Feed
// shows "Targets unavailable"; a lost video (not playing frames) draws
// nothing. Boxes ease toward each batch with the 120 ms smoothing, which runs a
// frame loop only while a box is still moving; reduced motion snaps them.
//
// Interactive (the main video and the PiP): a tap on a box opens the
// target-action menu (Lock / Release and the plugins' target actions); a drag
// on empty video (after a 500 ms long-press on touch, past 12 px with a mouse)
// draws a selection rect that designates on release; `[` / `]` cycle a focus
// ring over the boxes and Enter opens the menu; Esc clears the focus.
//
// With `holdDetection` (a node whose own engine feeds this overlay), the layer
// holds a detection lease on its camera while mounted, since the engine runs
// detection only on cameras someone is watching, and says so when the
// detector is idle or unavailable rather than showing an empty frame.

import { useEffect, useLayoutEffect, useMemo, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { Crosshair, Lock, LockOpen, Puzzle } from "lucide-react";

import { GlyphMarks, GlyphTags } from "@/components/feed/target-glyphs";
import { layoutPoint, showTargetsUnavailable } from "@/components/feed/target-pointer";
import { spokenState } from "@/components/feed/target-spoken";
import { Menu, type MenuItem } from "@/components/ui/menu";
import { useElementSize } from "@/hooks/use-element-size";
import { useDetectionLease } from "@/hooks/use-detection-lease";
import { useNow } from "@/hooks/use-now";
import { useSkillContext } from "@/hooks/use-skill-context";
import { CHROME_TOKENS } from "@/lib/chrome-tokens";
import { gridMeasures, Z, type WidthClass } from "@/lib/cockpit-layout";
import { computeRenderedRect, pickActiveBatch } from "@/lib/overlay-geometry";
import { detectorNotice } from "@/lib/detection-lease";
import { renderProfile } from "@/lib/render-profile";
import { resolveSkillState } from "@/lib/skills";
import { lockTarget, releaseTarget, runTargetAction, skillForConfigKey } from "@/lib/target-actions";
import {
  designatedFor,
  isDesignated,
  isVideoLost,
  nextGhost,
  nextLastFollowRect,
  targetInputFor,
  targetMenuEntries,
} from "@/lib/target-overlay";
import { cn, isDemoMode } from "@/lib/utils";
import { BoxEaser } from "@/lib/box-easer";
import type { SmoothBox } from "@/shared/hud/box-smoothing";
import { cycleFocus, dragAccepted, dragFrameBox, dragScreenRect, dragStarted, DOUBLE_CLICK_MS, focusOrder } from "@/shared/hud/target-interaction";
import { layoutTargets, tagFontPx, type GhostSample, type GlyphLayout, type Point } from "@/shared/hud/target-layout";
import {
  resolveTargets,
  type LastFollowRect,
  type TargetKind,
  type TargetDetection,
} from "@/shared/hud/target-state";
import type { VideoFeedState } from "@/shared/video-transport";
import { batchStage, DETECTION_DIM_ALPHA, useDetectionsStore } from "@/stores/detections-store";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";
import { useLayerChipsStore } from "@/stores/layer-chips-store";
import { specText } from "@/shared/hud/spec-text";

/** Box smoothing time constant (13-hud-v2 §5.4). */
const SMOOTH_TAU_MS = 120;
/** A box within this of its target has converged. */
const SMOOTH_EPS_PX = 0.5;
/** Touch long-press before a drag-to-designate starts. */
const LONG_PRESS_MS = 500;
const RAD_TO_DEG = 180 / Math.PI;

/** Kinds that ease toward each batch (detection-backed boxes). */
const SMOOTHED: Partial<Record<TargetKind, true>> = {
  CANDIDATE: true,
  TRACKED: true,
  LOCKED: true,
  ACQUIRING: true,
  FOLLOWING: true,
  HOLDING: true,
};

interface MenuState {
  anchor: { left: number; top: number; bottom: number };
  detection: TargetDetection;
}

export interface TargetLayerProps {
  /** The camera this video shows (roster id), or null for the primary leg. */
  cameraId: string | null;
  multiStream: boolean;
  videoState: VideoFeedState;
  /** The stream's decoded size (the letterbox source), when known. */
  videoWidth: number | null;
  videoHeight: number | null;
  widthClass: WidthClass;
  /** Mount yaw of this camera (0 for a forward camera); null for a
   *  down-facing camera, which never shows the off-frame arrow. */
  cameraYawOffsetDeg: number | null;
  /** The PiP: 9 px tags, candidate tags hidden, no keyboard focus cycle. */
  pip?: boolean;
  /** Hold this node's detection lease on the camera while mounted. */
  holdDetection?: boolean;
}

export function TargetLayer({
  cameraId,
  multiStream,
  videoState,
  videoWidth,
  videoHeight,
  widthClass,
  cameraYawOffsetDeg,
  pip = false,
  holdDetection = false,
}: TargetLayerProps) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const size = useElementSize(containerRef);
  const latest = useDetectionsStore((s) => s.latest);
  const byCamera = useDetectionsStore((s) => s.byCamera);
  const follow = useNavStreamStore((s) => s.follow);
  const event = useNavStreamStore((s) => s.event);
  const designated = useNavStreamStore((s) => s.designated);
  const yawRad = useFlightStore((s) => s.telemetry?.attitude?.yaw ?? null);
  const extensions = useExtensionsStore((s) => s.extensions);
  const ctx = useSkillContext();
  const demo = isDemoMode();
  const minimal = renderProfile() === "minimal";

  const picked = pickActiveBatch(multiStream, latest, byCamera, cameraId);
  // A 100 ms clock while anything time-based is on screen (staleness, LOST
  // fade, re-acquired snap); idle otherwise.
  const ticking = picked !== null || follow !== null || event !== null;
  const now = useNow(ticking ? 100 : null);
  // The batch ages like every layer: full, dimmed past 500 ms, hidden past 2 s.
  const stage = batchStage(picked?.receivedAt, now);
  const fresh = stage !== "hidden" ? picked : null;
  const input = targetInputFor(multiStream, cameraId, fresh);
  const camera = input.cameraId ?? cameraId ?? "main";
  const videoLost = isVideoLost(videoState, demo);
  const detector = useDetectionLease(cameraId ?? "main", holdDetection);
  const notice = videoLost ? null : detectorNotice(detector);
  const unavailable = showTargetsUnavailable(stage, detector, picked !== null);
  // The main layer raises "Targets unavailable" in the Feed's chip stack.
  useEffect(() => {
    if (!pip) useLayerChipsStore.getState().setTargetsUnavailable(unavailable);
  }, [pip, unavailable]);
  useEffect(() => (pip ? undefined : () => useLayerChipsStore.getState().setTargetsUnavailable(false)), [pip]);
  const yawDeg = yawRad != null && Number.isFinite(yawRad) ? yawRad * RAD_TO_DEG : null;

  // The followed track's last rect (for the LOST fade) and the ghost path,
  // advanced once per input change (the render-time state-adjustment pattern).
  const memoKey = `${fresh?.frameId ?? "-"}|${follow?.at ?? "-"}|${event?.at ?? "-"}|${now}|${camera}`;
  const [memory, setMemory] = useState<{ key: string; last: LastFollowRect | null; ghost: GhostSample[] }>({
    key: "",
    last: null,
    ghost: [],
  });

  const resolved = useMemo(
    () =>
      resolveTargets({
        batch: input.batch,
        cameraId: camera,
        designated,
        follow: follow?.data ?? null,
        followAgeMs: follow ? now - follow.at : 0,
        event: event ? { data: event.data, ageMs: now - event.at } : null,
        nowMs: now,
        videoLost,
        lastFollowRect: memory.last,
        vehicleYawDeg: yawDeg,
        cameraYawOffsetDeg,
        opts: { pip },
      }),
    [input.batch, camera, designated, follow, event, now, videoLost, memory.last, yawDeg, cameraYawOffsetDeg, pip],
  );
  if (memory.key !== memoKey) {
    setMemory({
      key: memoKey,
      last: nextLastFollowRect(memory.last, resolved, now),
      ghost: nextGhost(memory.ghost, resolved, now),
    });
  }

  // Box smoothing: the glyphs are laid out on each batch's boxes, and the
  // easer moves them from where they were shown by writing a transform onto
  // their elements each frame, so a moving box renders no React frame.
  const reduced = useReducedMotion();

  const srcW = videoWidth ?? resolved.frameW;
  const srcH = videoHeight ?? resolved.frameH;
  const rendered = computeRenderedRect(size.w, size.h, srcW, srcH);
  const [focusKey, setFocusKey] = useState<string | null>(null);
  const layouts = useMemo(
    () =>
      layoutTargets(resolved, {
        rendered,
        fontPx: tagFontPx(widthClass, pip),
        widthClass,
        revealKeys: focusKey ? new Set([focusKey]) : undefined,
        ghost: minimal ? undefined : memory.ghost,
      }),
    // `rendered` is a fresh object each render; its four numbers are the deps.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [resolved, rendered.left, rendered.top, rendered.width, rendered.height, widthClass, pip, focusKey, minimal, memory.ghost],
  );
  const easerRef = useRef<BoxEaser | null>(null);
  // The laid-out boxes and the glyph elements of the latest commit.
  const easeRef = useRef({ targets: new Map<string, SmoothBox>(), marks: new Map<string, Element>(), tags: new Map<string, HTMLElement>() });
  useLayoutEffect(() => {
    const root = containerRef.current;
    if (!root) return;
    const e = { targets: new Map<string, SmoothBox>(), marks: new Map<string, Element>(), tags: new Map<string, HTMLElement>() };
    for (const g of layouts) {
      if (g.box && SMOOTHED[g.kind]) e.targets.set(g.key, { x: g.box.x, y: g.box.y, width: g.box.w, height: g.box.h });
    }
    for (const el of root.querySelectorAll<HTMLElement | SVGGElement>("[data-ease-key]")) {
      const key = el.dataset.easeKey!;
      if (el instanceof HTMLElement) e.tags.set(key, el);
      else e.marks.set(key, el);
      // A glyph that stopped easing (its kind changed) drops its last transform.
      if (!e.targets.has(key)) {
        if (el instanceof HTMLElement) el.style.transform = "";
        else el.removeAttribute("transform");
      }
    }
    easeRef.current = e;
    easerRef.current ??= new BoxEaser((key, t) => {
      const { targets, marks, tags } = easeRef.current;
      const target = targets.get(key);
      marks.get(key)?.setAttribute("transform", `matrix(${t.sx} 0 0 ${t.sy} ${t.tx} ${t.ty})`);
      // Tags follow the box's corner without scaling, so their type stays crisp.
      const tag = tags.get(key);
      if (tag && target) {
        tag.style.transform = `translate(${t.tx + (t.sx - 1) * target.x}px, ${t.ty + (t.sy - 1) * target.y}px)`;
      }
    }, SMOOTH_TAU_MS, SMOOTH_EPS_PX);
    easerRef.current.setTargets(e.targets, reduced);
  }, [layouts, reduced]);
  useEffect(() => () => easerRef.current?.stop(), []);

  // ── interaction ──
  const [menu, setMenu] = useState<MenuState | null>(null);
  const tapTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const openMenuFor = (g: GlyphLayout, el: HTMLElement) => {
    const d = g.item.detection;
    if (!d) return;
    const r = el.getBoundingClientRect();
    setMenu({ anchor: { left: r.left, top: r.top, bottom: r.bottom }, detection: d });
  };

  const order = focusOrder(layouts);
  const orderKey = order.join(",");
  const focusRef = useRef<{ order: string[]; key: string | null; layouts: GlyphLayout[] }>({ order, key: focusKey, layouts });
  useEffect(() => {
    focusRef.current = { order, key: focusKey, layouts };
  });
  useEffect(() => {
    if (pip) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.ctrlKey || e.metaKey || e.altKey) return;
      const target = e.target as HTMLElement | null;
      if (target && (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.closest("[role=dialog]"))) return;
      const f = focusRef.current;
      if (e.key === "[" || e.key === "]") {
        setFocusKey(cycleFocus(f.order, f.key, e.key === "]" ? 1 : -1));
        e.preventDefault();
      } else if (e.key === "Enter" && f.key) {
        const el = containerRef.current?.querySelector<HTMLElement>(`[data-target-key="${CSS.escape(f.key)}"]`);
        const g = f.layouts.find((l) => l.key === f.key);
        if (el && g) openMenuFor(g, el);
        e.preventDefault();
      } else if (e.key === "Escape" && f.key) {
        setFocusKey(null);
        e.stopPropagation();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [pip]);
  // A focus whose target left the frame is dropped.
  const focusShown = focusKey !== null && orderKey.split(",").includes(focusKey) ? focusKey : null;

  // Drag-to-designate on empty video.
  const [drag, setDrag] = useState<{ a: Point; b: Point; live: boolean; touch: boolean } | null>(null);
  const pressTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const pointers = useRef(new Set<number>());
  // Layout pixels: the digital zoom scales this layer on screen.
  const localPoint = (e: ReactPointerEvent): Point => layoutPoint(e.clientX, e.clientY, containerRef.current!);
  const cancelDrag = () => {
    clearTimeout(pressTimer.current ?? undefined);
    setDrag(null);
  };
  useEffect(() => {
    if (!drag) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") cancelDrag();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [drag]);

  const frameW = resolved.frameW || srcW;
  const frameH = resolved.frameH || srcH;
  const finishDrag = () => {
    if (drag?.live && frameW > 0 && frameH > 0) {
      const box = dragFrameBox(dragScreenRect(drag.a, drag.b), rendered, frameW, frameH);
      if (dragAccepted(box, frameW, frameH)) void lockTarget(camera, box);
    }
    cancelDrag();
  };

  const designatedDet = menu ? isDesignated(designated, camera, menu.detection) : false;
  const menuItems: MenuItem[] = menu
    ? targetMenuEntries(menu.detection, designatedDet, extensions, (pluginId, configKey) => {
        const skill = skillForConfigKey(pluginId, configKey);
        return skill ? resolveSkillState(skill, ctx).enabled : true;
      }).map((entry): MenuItem => {
        const d = menu.detection;
        const ref = designatedFor(camera, d);
        if (entry.kind === "lock") {
          return {
            label: specText("target.lock"),
            icon: Lock,
            enabled: ref !== null,
            onSelect: () => {
              if (ref) void lockTarget(camera, ref.bbox, { classLabel: d.classLabel, confidence: d.confidence, trackId: ref.trackId });
            },
          };
        }
        if (entry.kind === "release") {
          return { label: specText("target.release"), icon: LockOpen, onSelect: () => void releaseTarget(ctx) };
        }
        const a = entry.action;
        return {
          label: a.label,
          icon: a.designate ? Crosshair : Puzzle,
          enabled: ref !== null,
          onSelect: () => {
            if (ref) {
              void runTargetAction(
                a,
                { cameraId: camera, bbox: ref.bbox, classLabel: d.classLabel, confidence: d.confidence, trackId: ref.trackId },
                ctx,
              );
            }
          },
        };
      })
    : [];

  const selection = drag?.live ? dragScreenRect(drag.a, drag.b) : null;
  const glyphAlpha = stage === "dim" ? DETECTION_DIM_ALPHA : 1;

  return (
    <div
      ref={containerRef}
      className={cn("absolute inset-0 touch-none select-none", Z.marks)}
      onPointerDown={(e) => {
        pointers.current.add(e.pointerId);
        // A second finger cancels a drag in progress.
        if (pointers.current.size > 1) {
          cancelDrag();
          return;
        }
        if (e.target !== e.currentTarget || (e.pointerType === "mouse" && e.button !== 0)) return;
        const p = localPoint(e);
        const touch = e.pointerType === "touch" || e.pointerType === "pen";
        setDrag({ a: p, b: p, live: false, touch });
        if (touch) {
          pressTimer.current = setTimeout(() => setDrag((d) => (d ? { ...d, live: true } : d)), LONG_PRESS_MS);
        }
        e.currentTarget.setPointerCapture?.(e.pointerId);
      }}
      onPointerMove={(e) => {
        if (!drag) return;
        const b = localPoint(e);
        if (drag.touch && !drag.live) {
          // Moving before the long-press fires is a pan, not a selection.
          if (dragStarted(drag.a, b)) cancelDrag();
          return;
        }
        setDrag({ ...drag, b, live: drag.live || dragStarted(drag.a, b) });
      }}
      onPointerUp={(e) => {
        pointers.current.delete(e.pointerId);
        finishDrag();
      }}
      onPointerCancel={(e) => {
        pointers.current.delete(e.pointerId);
        cancelDrag();
      }}
    >
      <svg className="pointer-events-none absolute inset-0 h-full w-full overflow-visible" aria-hidden>
        <g opacity={glyphAlpha} data-testid="target-glyphs">
          {layouts.map((g) => (
            <GlyphMarks key={g.key} g={g} minimal={minimal} />
          ))}
        </g>
        {selection ? (
          <rect
            x={selection.x}
            y={selection.y}
            width={selection.w}
            height={selection.h}
            fill="var(--hud-ink)"
            fillOpacity={0.12}
            stroke="var(--hud-ink)"
            strokeWidth={1}
            strokeDasharray="4 3"
          />
        ) : null}
      </svg>
      <div className="pointer-events-none absolute inset-0" style={{ opacity: glyphAlpha }} aria-hidden>
        {layouts.map((g) => (
          <GlyphTags key={g.key} g={g} />
        ))}
      </div>
      {notice ? (
        <span
          role="status"
          className={cn(
            "pointer-events-none absolute left-1/2 -translate-x-1/2 whitespace-nowrap rounded-[4px] bg-hud-glass font-mono font-semibold uppercase tracking-hud-tag text-hud-muted backdrop-blur-hud",
            pip ? "bottom-[4px] px-[5px] text-[9px]" : "bottom-[12px] px-[8px]",
          )}
          style={pip ? undefined : { fontSize: CHROME_TOKENS.type.label_px * gridMeasures(widthClass, false).scale }}
        >
          {notice}
        </span>
      ) : null}
      {layouts.map((g) =>
        g.hit ? (
          <button
            key={g.key}
            type="button"
            data-target-key={g.key}
            aria-label={spokenState(g)}
            className={cn(
              "absolute cursor-pointer rounded-sm bg-transparent outline-none",
              focusShown === g.key && "outline-dashed outline-2 outline-offset-[3px] outline-hud-ink",
            )}
            style={{ left: g.hit.x, top: g.hit.y, width: g.hit.w, height: g.hit.h }}
            onFocus={() => setFocusKey(g.key)}
            onClick={(e) => {
              const el = e.currentTarget;
              clearTimeout(tapTimer.current ?? undefined);
              // A double tap is reserved; a single tap waits it out.
              if (e.detail > 1) return;
              tapTimer.current = setTimeout(() => openMenuFor(g, el), DOUBLE_CLICK_MS);
            }}
          />
        ) : null,
      )}
      {menu ? <Menu anchor={menu.anchor} items={menuItems} onClose={() => setMenu(null)} /> : null}
    </div>
  );
}

function useReducedMotion(): boolean {
  const [reduced, setReduced] = useState(
    () => typeof window !== "undefined" && window.matchMedia?.("(prefers-reduced-motion: reduce)").matches === true,
  );
  useEffect(() => {
    const mq = window.matchMedia?.("(prefers-reduced-motion: reduce)");
    if (!mq) return;
    const on = () => setReduced(mq.matches);
    mq.addEventListener("change", on);
    return () => mq.removeEventListener("change", on);
  }, []);
  return reduced;
}

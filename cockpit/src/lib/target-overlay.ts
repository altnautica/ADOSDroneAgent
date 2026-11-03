// Pure glue between the cockpit's stores and the shared HUD v2 target logic
// (`@/shared/hud/target-state`): which batch and camera a video's target
// layer resolves, when a video counts as lost, the rect the LOST mark fades
// at, the ghost-path samples, and what the target-action menu offers for a
// picked box. Kept DOM-free so every rule is unit-tested.

import type { InstalledExtension, TargetAction } from "@/lib/extensions";
import { pushGhostSample, type GhostSample } from "@/shared/hud/target-layout";
import {
  followedRect,
  type DesignatedRef,
  type LastFollowRect,
  type ResolvedTargets,
  type TargetBatch,
  type TargetDetection,
} from "@/shared/hud/target-state";
import type { VideoFeedState } from "@/shared/video-transport";
import type { CockpitDetectionBatch } from "@/stores/detections-store";

/** The resolver input for one video: the batch picked for it (already matched
 *  to its camera by `pickActiveBatch`) and the camera id the resolver checks
 *  against. A single-stream node draws its newest batch whatever camera id the
 *  engine stamped, so the resolver compares against the batch's own id. */
export function targetInputFor(
  multiStream: boolean,
  cameraId: string | null,
  batch: CockpitDetectionBatch | null,
): { cameraId: string | null; batch: TargetBatch | null } {
  const camera = multiStream ? cameraId : (batch?.cameraId ?? cameraId);
  if (!batch) return { cameraId: camera, batch: null };
  return {
    cameraId: camera,
    batch: {
      cameraId: batch.cameraId,
      frameWidth: batch.frameWidth,
      frameHeight: batch.frameHeight,
      detections: batch.detections,
    },
  };
}

/** A video is lost when it is not playing frames (connecting, frozen or
 *  failed): every target is removed then, never pinned over a stale frame.
 *  The demo node has no video at all, so it never gates. */
export function isVideoLost(state: VideoFeedState, demo: boolean): boolean {
  return !demo && state !== "live";
}

/** The rect the LOST mark fades at: refreshed while the followed track is
 *  drawn, kept otherwise. */
export function nextLastFollowRect(
  prev: LastFollowRect | null,
  resolved: ResolvedTargets,
  now: number,
): LastFollowRect | null {
  const f = followedRect(resolved);
  if (!f) return prev;
  return { trackId: f.trackId, rect: f.rect, frameW: resolved.frameW, frameH: resolved.frameH, at: now };
}

/** Ghost-path samples: the FOLLOWING box's bottom-centre over the last second,
 *  kept unchanged while coasting so the path ends where the subject was. */
export function nextGhost(prev: readonly GhostSample[], resolved: ResolvedTargets, now: number): GhostSample[] {
  const following = resolved.items.find((i) => i.kind === "FOLLOWING" && i.rect);
  if (!following?.rect) return prev as GhostSample[];
  const r = following.rect;
  return pushGhostSample(prev, { t: now, x: r.x + r.w / 2, y: r.y + r.h });
}

/** The designation a picked box makes. */
export function designatedFor(cameraId: string, d: TargetDetection): DesignatedRef | null {
  if (!d.bbox) return null;
  return { cameraId, trackId: d.trackId ?? null, bbox: d.bbox };
}

/** Whether `ref` designates the detection `d` on `cameraId`. */
export function isDesignated(ref: DesignatedRef | null, cameraId: string, d: TargetDetection): boolean {
  if (!ref || ref.cameraId !== cameraId || !d.bbox) return false;
  if (ref.trackId != null) return d.trackId === ref.trackId;
  return (
    d.trackId == null &&
    d.bbox.x === ref.bbox.x &&
    d.bbox.y === ref.bbox.y &&
    d.bbox.width === ref.bbox.width &&
    d.bbox.height === ref.bbox.height
  );
}

/** One entry of the target-action menu. */
export type TargetMenuEntry =
  | { kind: "lock" }
  | { kind: "release" }
  | { kind: "plugin"; action: TargetAction };

/** What the menu offers for detection `d`: Lock (or Release when it is the
 *  designated target), then every installed plugin's target actions that
 *  apply to its class. A Follow-type action (one that designates) is offered
 *  only while the plugin's skill is drivable, so a disabled follow skill
 *  never appears as a live choice. */
export function targetMenuEntries(
  d: TargetDetection,
  designated: boolean,
  extensions: readonly InstalledExtension[],
  skillEnabled: (pluginId: string, configKey: string | null) => boolean,
): TargetMenuEntry[] {
  const out: TargetMenuEntry[] = [designated ? { kind: "release" } : { kind: "lock" }];
  const cls = d.classLabel.toLowerCase();
  for (const ext of extensions) {
    for (const action of ext.targetActions) {
      if (action.appliesToClass && action.appliesToClass.toLowerCase() !== cls) continue;
      if (action.designate && !skillEnabled(action.pluginId, action.configKey)) continue;
      out.push({ kind: "plugin", action });
    }
  }
  return out;
}

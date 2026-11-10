// The cockpit's camera actions (shutter, zoom, gimbal recentre) and where
// each goes. Every surface tries the same chain:
//   zoom  — an installed camera extension's zoom skill, else a digital zoom
//           of the shown video, 1× / 2× / 4×, labelled "Digital ×N";
//   photo — the extension's capture skill, else a still of the shown video
//           grabbed in the browser and downloaded, with a "Photo saved" toast;
//   recentre — the extension's recentre skill.
// A vehicle camera driven over MAVLink sits between the two in the chain on
// surfaces that see the vehicle's camera list; this node's API exposes no
// camera list, so the agent cockpit goes from the extension straight to the
// local fallback. Video recording is this node's own recorder (a ground
// station's recorder on a ground station). Without video every camera button
// is unavailable and says so.

import { startNodeRecording, startRecording, stopNodeRecording, stopRecording } from "@/lib/api";
import { registerCockpitActions, runCockpitAction } from "@/lib/cockpit-actions";
import type { InstalledExtension } from "@/lib/extensions";
import { toast } from "@/shared/api/toast";
import { specText } from "@/shared/hud/spec-text";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useFeedStore } from "@/stores/feed-store";
import { useStatusStore } from "@/stores/status-store";

export type CameraAction = "photo" | "zoom_in" | "zoom_out" | "recenter";

/** Skill local ids that implement each action, in preference order. */
export const CAMERA_SKILL_IDS: Readonly<Record<CameraAction, readonly string[]>> = {
  photo: ["capture", "photo", "take-photo"],
  zoom_in: ["zoom-in"],
  zoom_out: ["zoom-out"],
  recenter: ["recenter", "center"],
};

/** The `<pluginId>:<localId>` of the installed skill for an action, or null. */
export function cameraSkillFor(action: CameraAction, extensions: readonly InstalledExtension[]): string | null {
  for (const localId of CAMERA_SKILL_IDS[action]) {
    for (const ext of extensions) {
      const id = `${ext.pluginId}:${localId}`;
      if (ext.skills.some((s) => s.id === id)) return id;
    }
  }
  return null;
}

/** The digital zoom levels of the shown video. */
export const DIGITAL_ZOOM_STEPS = [1, 2, 4] as const;
export type DigitalZoom = (typeof DIGITAL_ZOOM_STEPS)[number];

/** One step in or out from `z`, held at the ends. */
export function stepDigitalZoom(z: DigitalZoom, dir: 1 | -1): DigitalZoom {
  const i = DIGITAL_ZOOM_STEPS.indexOf(z);
  return DIGITAL_ZOOM_STEPS[Math.max(0, Math.min(DIGITAL_ZOOM_STEPS.length - 1, i + dir))];
}

/** Where a camera action goes now: an extension skill, the local fallback
 *  (digital zoom, frame grab), or nowhere with the reason. */
export type CameraRoute =
  | { kind: "skill"; skill: string }
  | { kind: "local" }
  | { kind: "unavailable"; reason: string };

export function cameraRoute(action: CameraAction, extensions: readonly InstalledExtension[], videoLive: boolean): CameraRoute {
  // Recentring moves the gimbal, which needs no picture.
  if (!videoLive && action !== "recenter") return { kind: "unavailable", reason: specText("camera.no_video") };
  const skill = cameraSkillFor(action, extensions);
  if (skill) return { kind: "skill", skill };
  if (action === "recenter") return { kind: "unavailable", reason: specText("cockpit.camera.no_extension") };
  return { kind: "local" };
}

/** The recorder's route: unavailable without video. */
export function recordRoute(videoLive: boolean): { reason: string | null } {
  return { reason: videoLive ? null : specText("camera.no_video") };
}

/** The main video element the pilot sees (the stage, or the bottom-left card
 *  in the map view). */
function mainVideo(): HTMLVideoElement | null {
  return document.querySelector<HTMLVideoElement>("[data-main-video] video");
}

/** A still of the shown video as a PNG download. */
async function grabFrame(): Promise<void> {
  const video = mainVideo();
  if (!video || video.videoWidth === 0) {
    toast.err(specText("camera.no_video"));
    return;
  }
  const canvas = document.createElement("canvas");
  canvas.width = video.videoWidth;
  canvas.height = video.videoHeight;
  canvas.getContext("2d")?.drawImage(video, 0, 0);
  const blob = await new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, "image/png"));
  if (!blob) {
    toast.err(specText("camera.no_video"));
    return;
  }
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = `ados-photo-${Date.now()}.png`;
  a.click();
  URL.revokeObjectURL(url);
  toast.ok(specText("camera.saved"));
}

/** Run a camera action through its chain; an unavailable one shows why. */
export function runCameraAction(action: CameraAction): void {
  const videoLive = useFeedStore.getState().video.state === "live";
  const route = cameraRoute(action, useExtensionsStore.getState().extensions, videoLive);
  if (route.kind === "unavailable") {
    toast.info(route.reason);
    return;
  }
  if (route.kind === "skill") {
    runCockpitAction(`skill:${route.skill}`);
    return;
  }
  if (action === "photo") {
    void grabFrame();
    return;
  }
  const view = useCockpitViewStore.getState();
  view.setDigitalZoom(stepDigitalZoom(view.digitalZoom, action === "zoom_in" ? 1 : -1));
}

let recordBusy = false;

/** Toggle the recorder: the ground station's on a ground station, this
 *  node's own elsewhere. */
export async function toggleRecording(groundStation: boolean): Promise<void> {
  const { reason } = recordRoute(useFeedStore.getState().video.state === "live");
  if (reason) {
    toast.info(reason);
    return;
  }
  if (recordBusy) return;
  recordBusy = true;
  const s = useStatusStore.getState().status;
  const recording = s?.recording === true || s?.video?.recording === true;
  try {
    if (groundStation) await (recording ? stopRecording() : startRecording());
    else await (recording ? stopNodeRecording() : startNodeRecording());
  } catch {
    // The next status poll keeps the shutter honest (the agent's real state).
  } finally {
    recordBusy = false;
  }
}

/** Register the camera actions while the Feed is mounted. */
export function registerCameraActions(groundStation: boolean): () => void {
  return registerCockpitActions([
    { id: "camera.photo", label: "Photo", group: "camera", run: (p) => p === "press" && runCameraAction("photo") },
    {
      id: "camera.record",
      label: "Record",
      group: "camera",
      run: (p) => {
        if (p === "press") void toggleRecording(groundStation);
      },
    },
    { id: "camera.zoom_in", label: "Zoom in", group: "camera", run: (p) => p === "press" && runCameraAction("zoom_in") },
    { id: "camera.zoom_out", label: "Zoom out", group: "camera", run: (p) => p === "press" && runCameraAction("zoom_out") },
    { id: "gimbal.recenter", label: "Recentre gimbal", group: "camera", run: (p) => p === "press" && runCameraAction("recenter") },
  ]);
}

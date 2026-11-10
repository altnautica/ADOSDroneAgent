// The cockpit's camera actions (shutter, zoom, gimbal recentre) and where
// each goes. Every surface tries the same chain:
//   zoom  — an installed camera extension's zoom skill, else the vehicle's
//           own camera over MAVLink when it lists basic zoom, else a digital
//           zoom of the shown video, 1× / 2× / 4×, labelled "Digital ×N";
//   photo — the extension's capture skill, else the vehicle's camera over
//           MAVLink when it lists image capture, else a still of the shown
//           video grabbed in the browser and downloaded ("Photo saved");
//   recentre — the extension's recentre skill.
// The vehicle's cameras come from `GET /api/v1/camera/mavlink`; demo mode has
// none. Video recording is this node's own recorder (a ground station's
// recorder on a ground station). Without video every camera button except a
// vehicle camera's is unavailable and says so.

import { startNodeRecording, startRecording, stopNodeRecording, stopRecording } from "@/lib/api";
import { LEASE_OWNER } from "@/lib/lease-owner";
import { isDemoMode } from "@/lib/utils";
import { ApiError, apiFetch, errorDetail } from "@/shared/api-fetch";
import { registerCockpitActions, runCockpitAction } from "@/lib/cockpit-actions";
import type { InstalledExtension } from "@/lib/extensions";
import { toast } from "@/shared/api/toast";
import { hasSpecText, specText } from "@/shared/hud/spec-text";
import { create } from "zustand";
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

/** A camera the vehicle lists over MAVLink (`GET /api/v1/camera/mavlink`). */
export interface MavCamera {
  component_id: number;
  can_capture_image: boolean;
  can_zoom: boolean;
}

/** Where a camera action goes now: an extension skill, the vehicle's camera
 *  over MAVLink, the local fallback (digital zoom, frame grab), or nowhere
 *  with the reason. */
export type CameraRoute =
  | { kind: "skill"; skill: string }
  | { kind: "mavlink"; componentId: number }
  | { kind: "local" }
  | { kind: "unavailable"; reason: string };

export function cameraRoute(
  action: CameraAction,
  extensions: readonly InstalledExtension[],
  videoLive: boolean,
  mavCameras: readonly MavCamera[] = [],
): CameraRoute {
  // Recentring moves the gimbal, which needs no picture; nor does the
  // vehicle's own camera.
  if (!videoLive && action !== "recenter") {
    const cam = mavCameraFor(action, mavCameras);
    if (cam) return { kind: "mavlink", componentId: cam.component_id };
    return { kind: "unavailable", reason: specText("camera.no_video") };
  }
  const skill = cameraSkillFor(action, extensions);
  if (skill) return { kind: "skill", skill };
  if (action === "recenter") return { kind: "unavailable", reason: specText("cockpit.camera.no_extension") };
  const cam = mavCameraFor(action, mavCameras);
  if (cam) return { kind: "mavlink", componentId: cam.component_id };
  return { kind: "local" };
}

function mavCameraFor(action: CameraAction, cams: readonly MavCamera[]): MavCamera | null {
  if (action === "photo") return cams.find((c) => c.can_capture_image) ?? null;
  if (action === "zoom_in" || action === "zoom_out") return cams.find((c) => c.can_zoom) ?? null;
  return null;
}

/** The vehicle's MAVLink cameras; none in demo mode or when the list fails. */
export async function fetchMavCameras(): Promise<MavCamera[]> {
  if (isDemoMode()) return [];
  try {
    const r = await apiFetch<{ cameras?: MavCamera[] }>("/api/v1/camera/mavlink");
    return Array.isArray(r.cameras) ? r.cameras : [];
  } catch {
    return [];
  }
}

/** The last camera list asked for, so the zones know a vehicle camera can
 *  take a photo or zoom without video. */
export const useMavCameraStore = create<{ cameras: MavCamera[]; refresh: () => Promise<MavCamera[]> }>((set) => ({
  cameras: [],
  refresh: async () => {
    const cameras = await fetchMavCameras();
    set({ cameras });
    return cameras;
  },
}));

/** The agent's reply to a MAVLink camera write. */
export interface MavCameraReply {
  ok: boolean;
  result: string;
  statustext?: string;
}

/** A MAVLink result's spec name (`mav_result.*`), else the agent's token. */
export function mavResultText(result: string): string {
  const key = `mav_result.${result}`;
  return hasSpecText(key) ? specText(key) : result;
}

/** Send one MAVLink camera command. The photo's result shows as a toast; a
 *  refusal or error shows `camera.capture_failed` / `camera.zoom_failed`
 *  with the vehicle's text, else the result's name. */
export async function runMavCamera(action: CameraAction, componentId: number): Promise<MavCameraReply | null> {
  const photo = action === "photo";
  const failKey = photo ? "camera.capture_failed" : "camera.zoom_failed";
  const body = photo ? { component_id: componentId } : { component_id: componentId, type: "step", value: action === "zoom_in" ? 1 : -1 };
  try {
    const r = await apiFetch<MavCameraReply>(`/api/v1/camera/mavlink/${photo ? "capture" : "zoom"}`, { method: "POST", body });
    if (!r.ok) toast.err(specText(failKey, { reason: r.statustext || mavResultText(r.result) }));
    else if (photo) toast.ok(mavResultText(r.result));
    return r;
  } catch (e) {
    const reason = e instanceof ApiError ? errorDetail(e.body, e.message) : String(e);
    toast.err(specText(failKey, { reason }));
    return null;
  }
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
export async function runCameraAction(action: CameraAction): Promise<void> {
  const videoLive = useFeedStore.getState().video.state === "live";
  const extensions = useExtensionsStore.getState().extensions;
  // The vehicle's camera list is asked only when no extension takes the action.
  const skip = action === "recenter" || (videoLive && cameraSkillFor(action, extensions) !== null);
  const mav = skip ? [] : await useMavCameraStore.getState().refresh();
  const route = cameraRoute(action, extensions, videoLive, mav);
  if (route.kind === "unavailable") {
    toast.info(route.reason);
    return;
  }
  if (route.kind === "skill") {
    runCockpitAction(`skill:${route.skill}`);
    return;
  }
  if (route.kind === "mavlink") {
    await runMavCamera(action, route.componentId);
    return;
  }
  if (action === "photo") {
    await grabFrame();
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
    { id: "camera.photo", label: "Photo", group: "camera", run: (p) => void (p === "press" && runCameraAction("photo")) },
    {
      id: "camera.record",
      label: "Record",
      group: "camera",
      run: (p) => {
        if (p === "press") void toggleRecording(groundStation);
      },
    },
    { id: "camera.zoom_in", label: "Zoom in", group: "camera", run: (p) => void (p === "press" && runCameraAction("zoom_in")) },
    { id: "camera.zoom_out", label: "Zoom out", group: "camera", run: (p) => void (p === "press" && runCameraAction("zoom_out")) },
    { id: "gimbal.recenter", label: "Recentre gimbal", group: "camera", run: (p) => void (p === "press" && runCameraAction("recenter")) },
  ]);
}

// Stream quality: the drone's attention profile (`/api/video/profile`). On
// Auto the drone streams at the profile the ground station assigned; on High
// this window holds a hero lease, renewed while it stays chosen, so the drone
// streams full rate for this pilot without moving the fleet's assignment. A
// node without the route (a ground station, no video pipeline) has no field.

export const VIDEO_PROFILE_PATH = "/api/video/profile";
export const STREAM_QUALITIES = ["auto", "high"] as const;
export type StreamQuality = (typeof STREAM_QUALITIES)[number];
/** The hero lease asked for, and how often it is renewed (well inside it). */
export const QUALITY_LEASE_S = 30;
export const QUALITY_RENEW_MS = 10_000;

interface VideoProfileState {
  leases?: { holder: string }[];
}

/** The quality this window has chosen, read from the profile state: High
 *  while it holds a lease. */
export function streamQualityOf(state: VideoProfileState, holder: string): StreamQuality {
  return state.leases?.some((l) => l.holder === holder) ? "high" : "auto";
}

/** The request that applies a quality for `holder`. */
export function streamQualityRequest(q: StreamQuality, holder: string): { path: string; method: "POST" | "DELETE"; body?: unknown } {
  return q === "high"
    ? { path: VIDEO_PROFILE_PATH, method: "POST", body: { profile: "hero", holder, lease_s: QUALITY_LEASE_S } }
    : { path: `${VIDEO_PROFILE_PATH}?holder=${encodeURIComponent(holder)}`, method: "DELETE" };
}

let renewTimer: number | undefined;

async function applyQuality(q: StreamQuality): Promise<VideoProfileState> {
  const r = streamQualityRequest(q, LEASE_OWNER);
  return apiFetch<VideoProfileState>(r.path, { method: r.method, body: r.body });
}

/** The stream quality field's state: null until known, and while the node has
 *  no profile route. */
export const useStreamQualityStore = create<{
  quality: StreamQuality | null;
  load: () => Promise<void>;
  set: (q: StreamQuality) => Promise<void>;
}>((set) => ({
  quality: null,
  load: async () => {
    if (isDemoMode()) {
      set({ quality: "auto" });
      return;
    }
    try {
      set({ quality: streamQualityOf(await apiFetch<VideoProfileState>(VIDEO_PROFILE_PATH), LEASE_OWNER) });
    } catch {
      set({ quality: null });
    }
  },
  set: async (q) => {
    window.clearInterval(renewTimer);
    renewTimer = undefined;
    if (isDemoMode()) {
      set({ quality: q });
      return;
    }
    try {
      set({ quality: streamQualityOf(await applyQuality(q), LEASE_OWNER) });
      if (q === "high") renewTimer = window.setInterval(() => void applyQuality("high").catch(() => undefined), QUALITY_RENEW_MS);
    } catch (e) {
      toast.err(e instanceof ApiError ? errorDetail(e.body, e.message) : String(e));
    }
  },
}));

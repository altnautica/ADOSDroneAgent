// The cockpit's camera actions (shutter, zoom, gimbal recentre) and where
// each goes. Every surface tries the same chain:
//   zoom  — an installed camera extension's zoom skill, else the vehicle's
//           own camera over MAVLink when it lists basic zoom (a continuous
//           zoom from the press to its release), else a digital zoom of the
//           shown video, 1× / 2× / 4×, labelled "Digital ×N";
//   photo — the extension's capture skill, else the vehicle's camera over
//           MAVLink when it lists image capture, else a still of the shown
//           video grabbed in the browser and downloaded ("Photo saved");
//   recentre — the extension's recentre skill.
// The vehicle's cameras come from `GET /api/v1/camera/mavlink`; demo mode has
// none. Video recording is this node's own recorder (a ground station's
// recorder on a ground station). Without video every camera button except a
// vehicle camera's is unavailable and says so.

import { startNodeRecording, startRecording, stopNodeRecording, stopRecording } from "@/lib/api";
import { isDemoMode } from "@/lib/utils";
import { ApiError, apiFetch, errorDetail } from "@/shared/api-fetch";
import { registerCockpitActions, runCockpitAction } from "@/lib/cockpit-actions";
import type { InstalledExtension } from "@/lib/extensions";
import { toast } from "@/shared/api/toast";
import { refusalText } from "@/shared/hud/refusal";
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

/** How long the camera list may take before it reads as unanswered. The
 *  agent's own probe takes about 1 s. */
export const CAMERA_LIST_TIMEOUT_MS = 4000;

/** The agent's longest wait on one camera write: a probe for a camera it has
 *  not registered (1 s), the first send and two resends at 1.2 s each, then
 *  the extended wait after an `IN_PROGRESS` answer (8 s). */
export const AGENT_CAMERA_WRITE_BUDGET_MS = 1000 + 1200 * 3 + 8000;

/** How long a camera write may take before the press reads as unanswered:
 *  past the agent's own wait, with margin for the link, so a capture the
 *  camera is still taking never reads as a failure. */
export const CAMERA_WRITE_TIMEOUT_MS = AGENT_CAMERA_WRITE_BUDGET_MS + 3400;

/** `apiFetch` cut off after `timeoutMs`; a cut-off request rejects with a
 *  `CameraTimeout`. */
class CameraTimeout extends Error {}

async function cameraFetch<T>(path: string, timeoutMs: number, opts: { method?: "POST"; body?: unknown } = {}): Promise<T> {
  const ac = new AbortController();
  const timer = setTimeout(() => ac.abort(new CameraTimeout()), timeoutMs);
  try {
    return await apiFetch<T>(path, { ...opts, signal: ac.signal });
  } catch (e) {
    throw ac.signal.aborted ? new CameraTimeout() : e;
  } finally {
    clearTimeout(timer);
  }
}

/** The vehicle's MAVLink cameras, or null when the list fails or times out;
 *  none in demo mode. */
async function listMavCameras(): Promise<MavCamera[] | null> {
  if (isDemoMode()) return [];
  try {
    const r = await cameraFetch<{ cameras?: MavCamera[] }>("/api/v1/camera/mavlink", CAMERA_LIST_TIMEOUT_MS);
    return Array.isArray(r.cameras) ? r.cameras : [];
  } catch {
    return null;
  }
}

/** The vehicle's MAVLink cameras; none in demo mode or when the list fails. */
export async function fetchMavCameras(): Promise<MavCamera[]> {
  return (await listMavCameras()) ?? [];
}

/** The last camera list the agent gave, so the zones know a vehicle camera can
 *  take a photo or zoom without video, and a press routes without waiting on
 *  the agent's probe. A failed refresh keeps the last list. `loaded` turns true
 *  once a list has been asked for. */
let refreshing: Promise<MavCamera[]> | null = null;

export const useMavCameraStore = create<{ cameras: MavCamera[]; loaded: boolean; refresh: () => Promise<MavCamera[]> }>((set, get) => ({
  cameras: [],
  loaded: false,
  refresh: () => {
    // One list in flight at a time: presses during it share its answer.
    refreshing ??= listMavCameras()
      .then((listed) => {
        const cameras = listed ?? get().cameras;
        set({ cameras, loaded: true });
        return cameras;
      })
      .finally(() => {
        refreshing = null;
      });
    return refreshing;
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

/** Send one MAVLink camera write, once: the agent resends where that is safe
 *  (a resent capture would shoot twice). A refusal or error shows `failKey`
 *  (`camera.capture_failed` / `camera.zoom_failed`) with the vehicle's text,
 *  else the result's name; a null `failKey` shows nothing. Never rejects. */
async function mavCameraWrite(route: "capture" | "zoom", body: object, failKey: string | null): Promise<MavCameraReply | null> {
  try {
    const r = await cameraFetch<MavCameraReply>(`/api/v1/camera/mavlink/${route}`, CAMERA_WRITE_TIMEOUT_MS, {
      method: "POST",
      body,
    });
    // IN_PROGRESS is the camera still working on an accepted command, never
    // a failure, whatever `ok` says.
    if (failKey && !r.ok && r.result !== "IN_PROGRESS") toast.err(specText(failKey, { reason: r.statustext || mavResultText(r.result) }));
    return r;
  } catch (e) {
    const reason = e instanceof ApiError ? errorDetail(e.body, e.message) : refusalText("no response");
    if (failKey) toast.err(specText(failKey, { reason }));
    return null;
  }
}

/** True when the camera answered and refused: it is not acting on the command. */
function refused(r: MavCameraReply | null): boolean {
  return r !== null && !r.ok && r.result !== "IN_PROGRESS" && r.result !== "TIMEOUT";
}

/** One photo on the vehicle's camera; a taken photo shows as a toast. */
export async function captureMavPhoto(componentId: number): Promise<MavCameraReply | null> {
  const r = await mavCameraWrite("capture", { component_id: componentId }, "camera.capture_failed");
  if (r !== null && (r.ok || r.result === "IN_PROGRESS")) toast.ok(specText("camera.capture_sent"));
  return r;
}

/** A tap zooms at least this long, so a quick tap still moves the lens. */
export const ZOOM_TAP_MS = 300;
/** A held zoom whose release never comes (a lost pointer-up) stops here. */
export const ZOOM_HOLD_MAX_MS = 10_000;
/** A held zoom tells the agent it is still held this often (its deadman stops a quiet zoom after ~1 s). */
export const ZOOM_KEEPALIVE_MS = 300;
/** A failed stop is sent again this long after, `ZOOM_STOP_TRIES` sends in all. */
export const ZOOM_STOP_RETRY_MS = 250;
export const ZOOM_STOP_TRIES = 3;

/** Set the vehicle camera's continuous zoom rate: -1 wide … 1 tele, 0 stops (ArduPilot refuses a step zoom). */
export function setMavZoomRate(componentId: number, rate: number, failKey: string | null = "camera.zoom_failed"): Promise<MavCameraReply | null> {
  return mavCameraWrite("zoom", { component_id: componentId, type: "continuous", value: rate }, failKey);
}

/** Stop the vehicle camera's zoom, resent until the camera takes it, up to `ZOOM_STOP_TRIES`
 *  sends. A refusal shows at once, no answer on the last send; one failure shows at most. */
export async function stopMavZoom(componentId: number): Promise<MavCameraReply | null> {
  for (let i = 1, shown = false; ; i++) {
    const r = await setMavZoomRate(componentId, 0, null);
    if (r !== null && (r.ok || r.result === "IN_PROGRESS")) return r;
    if (!shown && (i >= ZOOM_STOP_TRIES || refused(r))) {
      shown = true;
      toast.err(specText("camera.zoom_failed", { reason: r ? r.statustext || mavResultText(r.result) : refusalText("no response") }));
    }
    if (i >= ZOOM_STOP_TRIES) return r;
    await new Promise((done) => setTimeout(done, ZOOM_STOP_RETRY_MS));
  }
}

/** The releases of the vehicle camera zooms being held. */
const heldZooms = new Set<() => void>();

/** Release every held vehicle camera zoom (the page hides or the camera
 *  controls go away). */
export function releaseMavZooms(): void {
  for (const release of [...heldZooms]) release();
}

/** Zoom the vehicle camera at full rate (`dir` 1 tele, -1 wide) until the
 *  returned release runs, at least `ZOOM_TAP_MS` and at most
 *  `ZOOM_HOLD_MAX_MS` after the press. The stop goes then, whether or not
 *  the rate has its answer yet, and once more if the rate's answer comes
 *  after it (the camera may act on a late rate). While held, a keepalive
 *  goes every `ZOOM_KEEPALIVE_MS` so the agent's own deadman stops the zoom
 *  if this page vanishes. A camera that refused the rate gets no stop. */
export function holdMavZoom(componentId: number, dir: 1 | -1): () => void {
  const pressedAt = Date.now();
  let released = false;
  let refusedStart = false;
  let stopSent = false;
  void setMavZoomRate(componentId, dir).then((reply) => {
    if (refused(reply)) {
      refusedStart = true;
      clearInterval(keepalive);
    } else if (stopSent) {
      void stopMavZoom(componentId);
    }
  });
  const keepalive = setInterval(() => {
    // A 409 (the zoom already stopped) or a lost keepalive needs nothing:
    // the release's stop and the agent's deadman cover both.
    void cameraFetch(`/api/v1/camera/mavlink/zoom`, CAMERA_WRITE_TIMEOUT_MS, {
      method: "POST",
      body: { component_id: componentId, type: "continuous", value: dir, keepalive: true },
    }).catch(() => undefined);
  }, ZOOM_KEEPALIVE_MS);
  const release = () => {
    if (released) return;
    released = true;
    heldZooms.delete(release);
    clearTimeout(cap);
    clearInterval(keepalive);
    setTimeout(() => {
      if (refusedStart) return;
      stopSent = true;
      void stopMavZoom(componentId);
    }, Math.max(0, ZOOM_TAP_MS - (Date.now() - pressedAt)));
  };
  const cap = setTimeout(release, ZOOM_HOLD_MAX_MS);
  heldZooms.add(release);
  return release;
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

/** The part of a `vw`×`vh` frame the pilot sees: the frame is fitted inside
 *  a `boxW`×`boxH` box (`object-contain`, letterboxed), scaled by the
 *  digital zoom about the box centre, and clipped to the box. A letterboxed
 *  side stays whole until the zoom pushes it past the box edge. */
export function frameCrop(
  vw: number,
  vh: number,
  zoom: number,
  boxW: number = vw,
  boxH: number = vh,
): { sx: number; sy: number; sw: number; sh: number } {
  const fit = Math.min(boxW / vw, boxH / vh) * zoom;
  const rw = vw * fit;
  const rh = vh * fit;
  const rx = (boxW - rw) / 2;
  const ry = (boxH - rh) / 2;
  const x0 = Math.max(0, rx);
  const y0 = Math.max(0, ry);
  return {
    sx: (x0 - rx) / fit,
    sy: (y0 - ry) / fit,
    sw: (Math.min(boxW, rx + rw) - x0) / fit,
    sh: (Math.min(boxH, ry + rh) - y0) / fit,
  };
}

/** A still of the shown video, as the pilot sees it (the rendered picture
 *  under the digital zoom, letterbox excluded), as a PNG download. */
async function grabFrame(): Promise<void> {
  const video = mainVideo();
  if (!video || video.videoWidth === 0) {
    toast.err(specText("camera.no_video"));
    return;
  }
  // The box the picture is fitted in: the video element's own layout size
  // (the zoom is a transform on its wrapper, so it does not change this).
  const boxW = video.clientWidth || video.videoWidth;
  const boxH = video.clientHeight || video.videoHeight;
  const { sx, sy, sw, sh } = frameCrop(video.videoWidth, video.videoHeight, useCockpitViewStore.getState().digitalZoom, boxW, boxH);
  const canvas = document.createElement("canvas");
  canvas.width = Math.round(sw);
  canvas.height = Math.round(sh);
  canvas.getContext("2d")?.drawImage(video, sx, sy, sw, sh, 0, 0, canvas.width, canvas.height);
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

/** A zoom press: its action, and the release of the vehicle camera zoom it
 *  started (null until it starts, or on another route). */
interface ZoomPress {
  action: CameraAction;
  release: (() => void) | null;
}

/** The zoom press in progress. */
let zoomPress: ZoomPress | null = null;

/** Run a camera action through its chain; an unavailable one shows why.
 *  A vehicle camera zooms from the zoom press to its release; every other
 *  route acts on the press, and a release is a no-op. */
export async function runCameraAction(action: CameraAction, phase: "press" | "release" = "press"): Promise<void> {
  if (phase === "release") {
    if (zoomPress?.action === action) {
      zoomPress.release?.();
      zoomPress = null;
    }
    return;
  }
  const press: ZoomPress | null = action === "zoom_in" || action === "zoom_out" ? { action, release: null } : null;
  if (press) {
    zoomPress?.release?.();
    zoomPress = press;
  }
  const videoLive = useFeedStore.getState().video.state === "live";
  const extensions = useExtensionsStore.getState().extensions;
  // The press routes on the last camera list and refreshes it in the
  // background (the agent probes the vehicle for about a second); only the
  // first press, before any list, waits for one. No list is asked when an
  // extension takes the action.
  const skip = action === "recenter" || (videoLive && cameraSkillFor(action, extensions) !== null);
  const store = useMavCameraStore.getState();
  let mav: readonly MavCamera[] = [];
  if (!skip && store.loaded) {
    mav = store.cameras;
    void store.refresh();
  } else if (!skip) {
    mav = await store.refresh();
  }
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
    if (!press) {
      await captureMavPhoto(route.componentId);
      return;
    }
    const release = holdMavZoom(route.componentId, action === "zoom_in" ? 1 : -1);
    // A release that came while the camera list loaded ends the zoom as a tap.
    if (zoomPress === press) press.release = release;
    else release();
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

/** End every vehicle camera zoom in progress: its stop goes now. */
export function endZoomPresses(): void {
  zoomPress = null;
  releaseMavZooms();
}

/** Register the camera actions while the Feed is mounted. A held zoom ends
 *  when the page hides or the Feed goes away. */
export function registerCameraActions(groundStation: boolean): () => void {
  const onHidden = () => {
    if (document.visibilityState === "hidden") endZoomPresses();
  };
  document.addEventListener("visibilitychange", onHidden);
  window.addEventListener("pagehide", endZoomPresses);
  const unregister = registerCockpitActions([
    { id: "camera.photo", label: "Photo", group: "camera", run: (p) => void (p === "press" && runCameraAction("photo")) },
    {
      id: "camera.record",
      label: "Record",
      group: "camera",
      run: (p) => {
        if (p === "press") void toggleRecording(groundStation);
      },
    },
    { id: "camera.zoom_in", label: "Zoom in", group: "camera", run: (p) => void (p !== "hold" && runCameraAction("zoom_in", p)) },
    { id: "camera.zoom_out", label: "Zoom out", group: "camera", run: (p) => void (p !== "hold" && runCameraAction("zoom_out", p)) },
    { id: "gimbal.recenter", label: "Recentre gimbal", group: "camera", run: (p) => void (p === "press" && runCameraAction("recenter")) },
  ]);
  return () => {
    unregister();
    document.removeEventListener("visibilitychange", onHidden);
    window.removeEventListener("pagehide", endZoomPresses);
    endZoomPresses();
  };
}

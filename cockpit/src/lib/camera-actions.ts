// The cockpit's camera actions (shutter, zoom, gimbal recentre) and where
// each goes: still photos, zoom and the gimbal belong to the installed camera
// extension, so each action fires the first installed camera skill whose local
// id names it (siyi-pod `photo` / `zoom-in`, gimbal-v2 `recenter`, …); video
// recording is this node's own recorder (a ground station's recorder on a
// ground station).

import { startNodeRecording, startRecording, stopNodeRecording, stopRecording } from "@/lib/api";
import { registerCockpitActions, runCockpitAction } from "@/lib/cockpit-actions";
import type { InstalledExtension } from "@/lib/extensions";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useStatusStore } from "@/stores/status-store";

export type CameraAction = "photo" | "zoom_in" | "zoom_out" | "recenter";

/** Skill local ids that implement each action, in preference order. */
export const CAMERA_SKILL_IDS: Readonly<Record<CameraAction, readonly string[]>> = {
  photo: ["photo"],
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

function fireCamera(action: CameraAction): void {
  const id = cameraSkillFor(action, useExtensionsStore.getState().extensions);
  if (id) runCockpitAction(`skill:${id}`);
}

let recordBusy = false;

/** Toggle the recorder: the ground station's on a ground station, this
 *  node's own elsewhere. */
export async function toggleRecording(groundStation: boolean): Promise<void> {
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
    { id: "camera.photo", label: "Photo", group: "camera", run: (p) => p === "press" && fireCamera("photo") },
    {
      id: "camera.record",
      label: "Record",
      group: "camera",
      run: (p) => {
        if (p === "press") void toggleRecording(groundStation);
      },
    },
    { id: "camera.zoom_in", label: "Zoom in", group: "camera", run: (p) => p === "press" && fireCamera("zoom_in") },
    { id: "camera.zoom_out", label: "Zoom out", group: "camera", run: (p) => p === "press" && fireCamera("zoom_out") },
    { id: "gimbal.recenter", label: "Recentre gimbal", group: "camera", run: (p) => p === "press" && fireCamera("recenter") },
  ]);
}

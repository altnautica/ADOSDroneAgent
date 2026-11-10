// Keeping the vehicle's MAVLink camera list current, so Photo and Zoom know a
// vehicle camera can shoot without video. A node that boots before its flight
// controller connects lists none at first, so the list is asked for again when
// the controller connects and on a slow timer.

import type { StoreApi } from "zustand";

export const MAV_CAMERA_REFRESH_MS = 30_000;

type StatusStore = Pick<StoreApi<{ status: { fc_connected?: boolean | null } | null }>, "getState" | "subscribe">;

/** Lists the cameras now, on each FC-connected edge and every
 *  `MAV_CAMERA_REFRESH_MS`; returns the stop. */
export function watchMavCameras(refresh: () => void, status: StatusStore): () => void {
  refresh();
  let connected = status.getState().status?.fc_connected === true;
  const unsubscribe = status.subscribe((s) => {
    const now = s.status?.fc_connected === true;
    if (now && !connected) refresh();
    connected = now;
  });
  const timer = setInterval(refresh, MAV_CAMERA_REFRESH_MS);
  return () => {
    unsubscribe();
    clearInterval(timer);
  };
}

/** A press refused for want of video asks for the camera list again, so a
 *  vehicle camera found since routes the next press. */
export function reprobeOnNoVideo(reason: string | null, noVideo: string, refresh: () => void): boolean {
  if (reason !== noVideo) return false;
  refresh();
  return true;
}

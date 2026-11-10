// Why a flight-screen camera control is unavailable now.

import { cameraRoute, useMavCameraStore, type CameraAction } from "@/lib/camera-actions";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useFeedStore } from "@/stores/feed-store";

export function useVideoLive(): boolean {
  return useFeedStore((s) => s.video.state === "live");
}

/** Why a camera action is unavailable now, or null. A vehicle camera listed
 *  over MAVLink takes a photo or zooms without video. */
export function useCameraReason(action: CameraAction): string | null {
  const extensions = useExtensionsStore((s) => s.extensions);
  const mav = useMavCameraStore((s) => s.cameras);
  const route = cameraRoute(action, extensions, useVideoLive(), mav);
  return route.kind === "unavailable" ? route.reason : null;
}

// The dashboard video panel's layer lease: the Route-level base layers plus
// the depth and surfaces layers when toggled on (`layers.json` `lease: true`,
// off by default). The view lease only says what to serve: the vision engine
// runs depth and surfaces only under a `/api/vision/demand` lease of that
// kind, so a toggled-on layer also holds one (`holdLayerDemand`), as the
// cockpit does.

import { LEASE_OWNER } from "@/lib/lease-owner";
import { holdDetectionLease, type VisibilitySource } from "@/lib/video-detections";
import layersJson from "@/shared/cockpit-spec/layers.json";

/** The primary camera's served id: the default for detections and layers. */
export const DEFAULT_CAMERA_ID = "main";

/** The camera ids the detections and layers can follow: the primary first,
 *  then every other leg of the camera roster (`GET /api/video/roster`). */
export function cameraOptions(roster: { cameras?: readonly { id?: unknown }[] } | null | undefined): string[] {
  const ids = (roster?.cameras ?? []).map((c) => c.id).filter((id): id is string => typeof id === "string" && id !== "");
  return [DEFAULT_CAMERA_ID, ...ids.filter((id) => id !== DEFAULT_CAMERA_ID)];
}
const BASE_LAYERS = ["ar_route", "ar_motion", "obstacles"] as const;

/** The leased layers toggled on their own (`layers.json` `lease: true`), with
 *  their spec label and default. */
export const LEASED_LAYERS: readonly { id: string; label: string; default: boolean }[] = layersJson.layers
  .filter((l) => l.lease)
  .map((l) => ({ id: l.id, label: l.label, default: l.default }));

/** The leased layers on by default. */
export function defaultLeasedLayers(): Record<string, boolean> {
  return Object.fromEntries(LEASED_LAYERS.map((l) => [l.id, l.default]));
}

/** The view query for the base layers plus the toggled-on leased ones. */
export function layersViewQuery(toggles: Readonly<Record<string, boolean>>, cameraId: string = DEFAULT_CAMERA_ID): string {
  const on = LEASED_LAYERS.filter((l) => toggles[l.id]).map((l) => l.id);
  return `${cameraId}:${[...BASE_LAYERS, ...on].join(",")}`;
}

/** A demand lease's hold, and how often it is renewed while held. */
export const DEMAND_HOLD_S = 6;
export const DEMAND_RENEW_MS = 2000;

/** The vision engine kinds a layer toggle demands. */
export type DemandKind = "depth" | "surfaces";

export interface VisionDemandBody {
  kind: DemandKind;
  camera_id: string;
  hold_s: number;
  owner: string;
}

/** Hold a `kind` demand lease on `cameraId` while the page is
 *  visible: taken now, renewed every `DEMAND_RENEW_MS`, released with
 *  `hold_s: 0` when the page hides or the returned function runs. */
export function holdLayerDemand(
  kind: DemandKind,
  post: (body: VisionDemandBody) => void,
  doc: VisibilitySource,
  cameraId: string = DEFAULT_CAMERA_ID,
): () => void {
  return holdDetectionLease(
    (holdS) => post({ kind, camera_id: cameraId, hold_s: holdS, owner: LEASE_OWNER }),
    doc,
    DEMAND_HOLD_S,
    DEMAND_RENEW_MS,
  );
}

// Which video legs the Feed offers and where each one is fetched from.

import type { RosterCamera, StatusStream } from "@/lib/types";
import { whepUrlFor } from "@/shared/whep";

/** The relative WHEP + HLS endpoints for a leg, same-origin against whatever
 *  host the operator reached the agent on. The primary leg is `/whep` and
 *  `/hls/main/index.m3u8`; another leg is `/whep?camera=<id>` and
 *  `/hls/<id>/index.m3u8` unless the agent advertises its own URLs. */
export function resolveLegVideoUrls(
  active: Pick<RosterCamera, "id" | "whep_url" | "hls_url"> | null,
): { whepUrl: string; hlsUrl: string } {
  const secondary = active?.id && active.id !== "main" ? active.id : null;
  return {
    whepUrl: active?.whep_url ?? whepUrlFor("/whep", secondary),
    hlsUrl: active?.hls_url ?? (secondary ? `/hls/${secondary}/index.m3u8` : "/hls/main/index.m3u8"),
  };
}

/** The legs the tabs offer: the concurrent legs the status body advertises
 *  when there is more than one, else the camera roster. */
export function feedCameras(streams: StatusStream[] | undefined, roster: RosterCamera[]): RosterCamera[] {
  if (streams && streams.length > 1) {
    return streams.map((s) => ({
      id: s.id,
      role: s.role,
      live: s.live ?? undefined,
      whep_url: s.whepUrl,
      hls_url: s.hls,
    }));
  }
  return roster;
}

/** Labels for the camera roles the agent defines, for a leg with no name.
 *  `down` is the downward-facing (belly) camera. */
const ROLE_LABEL: Readonly<Record<string, string>> = { down: "Belly" };

/** The operator-facing name of a leg: its label or name, else its role word,
 *  else its id. */
export function cameraLabel(cam: Pick<RosterCamera, "id" | "label" | "name" | "role">): string {
  const roleLabel = cam.role ? (ROLE_LABEL[cam.role] ?? cam.role) : undefined;
  return cam.label ?? cam.name ?? roleLabel ?? cam.id;
}

/**
 * The leg the picture-in-picture inset shows, or null with fewer than two
 * legs. The operator's last choice (`pipId`, set by a swap) wins while it is
 * still offered and is not the main leg; otherwise the belly (`down`) leg,
 * then the first other leg.
 */
export function pickPipCamera(
  cameras: readonly RosterCamera[],
  mainId: string | null,
  pipId: string | null,
): RosterCamera | null {
  if (cameras.length < 2) return null;
  const main = mainId ?? cameras[0].id;
  const others = cameras.filter((c) => c.id !== main);
  return others.find((c) => c.id === pipId) ?? others.find((c) => c.role === "down") ?? others[0] ?? null;
}

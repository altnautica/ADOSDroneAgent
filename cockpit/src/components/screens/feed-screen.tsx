// The Feed / HUD screen (the default) — the immersive, video-forward flying
// view. It is a full-bleed screen: the WHEP video fills the panel (L0), the
// flight-instrument HUD floats over it (L2), multi-stream tabs appear top-left
// when the node has more than one camera, and the pilot's action bar sits at the
// bottom; the shell floats its own chrome (status strip, menu, utility bar)
// translucently on top. Flight telemetry is provided here so its 5 Hz poll runs
// only while the Feed is on screen.

import { useEffect, useMemo } from "react";

import { DetectionOverlay } from "@/components/feed/detection-overlay";
import { FeedActionBar } from "@/components/feed/feed-action-bar";
import { FeedHud } from "@/components/feed/feed-hud";
import { MiniMap } from "@/components/feed/mini-map";
import { ProximityRadar } from "@/components/feed/proximity-radar";
import { SkillBar } from "@/components/feed/skill-bar";
import { StreamTabs } from "@/components/feed/stream-tabs";
import { VideoLayer } from "@/components/shell/video-layer";
import { FlightTelemetryProvider } from "@/hooks/flight-telemetry-context";
import { resolveActiveCameraId } from "@/lib/overlay-geometry";
import { useGroundDetectionPoll } from "@/hooks/use-ground-detection-poll";
import { useProfile } from "@/hooks/use-profile";
import { useRoster } from "@/hooks/use-roster";
import { useVisionDetections } from "@/hooks/use-vision-detections";
import { useFeedStore } from "@/stores/feed-store";
import type { RosterCamera } from "@/lib/types";

// Resolve the relative WHEP + HLS endpoints for the active leg, same-origin
// against whatever host the operator reached the agent on (never an absolute
// host:port — ICE won't traverse the remote path, so the cockpit needs the HLS
// fallback to resolve against the same origin). The primary `main` stream (and
// the single-camera default) is `/hls/main/index.m3u8`; a secondary leg is
// served at `/hls/<id>/index.m3u8`. The roster may expose either URL explicitly
// (`whep_url` / `hls_url`); otherwise they are derived from the leg id.
export function resolveLegVideoUrls(
  active: Pick<RosterCamera, "id" | "whep_url" | "hls_url"> | null,
): { whepUrl: string; hlsUrl: string } {
  const primaryHls = "/hls/main/index.m3u8";
  const hls =
    active?.hls_url ??
    (active?.id && active.id !== "main"
      ? `/hls/${active.id}/index.m3u8`
      : primaryHls);
  return { whepUrl: active?.whep_url ?? "/whep", hlsUrl: hls };
}

export function FeedScreen() {
  const cameras = useRoster();
  const profile = useProfile();
  const activeCameraId = useFeedStore((s) => s.activeCameraId);
  const streamNonce = useFeedStore((s) => s.streamNonce);
  const setActiveStreamLabel = useFeedStore((s) => s.setActiveStreamLabel);

  // The vision engine lives on a companion node (a drone or a workstation/compute
  // box), not a ground station — so the on-box LOCAL detection feed runs only
  // there, over this node's own socket. A ground station has no local engine: it
  // must RECEIVE the linked drone's detections over the relay, which this screen
  // drives through the ground poll path instead. Both produce boxes through the
  // same overlay, so a ground cockpit and a drone cockpit render identically.
  const visionCapable =
    profile === "drone" ||
    profile === "workstation" ||
    profile === "compute" ||
    profile === "ground_station";
  const localVisionCapable =
    profile === "drone" || profile === "workstation" || profile === "compute";
  useVisionDetections(localVisionCapable);
  useGroundDetectionPoll();

  // Flight-nav aids (minimap / radar) show only where a flying vehicle's state is
  // present: a drone reports its own, a ground station republishes the received
  // drone's. A workstation/compute node has no vehicle, so they stay hidden there.
  const flightNavCapable = profile === "drone" || profile === "ground_station";

  const { whepUrl, hlsUrl, reconnectKey, activeLabel } = useMemo(() => {
    const active =
      cameras.find((c) => c.id === activeCameraId) ?? cameras[0] ?? null;
    const { whepUrl, hlsUrl } = resolveLegVideoUrls(active);
    return {
      whepUrl,
      hlsUrl,
      reconnectKey: `${active?.id ?? "primary"}:${whepUrl}:${streamNonce}`,
      activeLabel: active?.label ?? active?.name ?? active?.role ?? null,
    };
  }, [cameras, activeCameraId, streamNonce]);

  // Publish the selected stream's label to the shared feed state so the top
  // bar's video zone can name what is on screen (a ground station has an empty
  // roster, so this is null there and the zone falls back to a generic label).
  useEffect(() => {
    setActiveStreamLabel(activeLabel);
  }, [activeLabel, setActiveStreamLabel]);

  return (
    <FlightTelemetryProvider>
      <div className="absolute inset-0">
        <VideoLayer
          whepUrl={whepUrl}
          hlsUrl={hlsUrl}
          reconnectKey={reconnectKey}
        />
        {visionCapable ? (
          <DetectionOverlay
            // Follow the SAME leg the video shows — the resolved leg id, not the
            // raw selection — so boxes draw on the headline leg with no click.
            activeCameraId={resolveActiveCameraId(activeCameraId, cameras)}
            multiStream={cameras.length > 1}
          />
        ) : null}
        <FeedHud />
        {cameras.length > 1 ? <StreamTabs cameras={cameras} /> : null}
        {flightNavCapable ? <MiniMap /> : null}
        {flightNavCapable ? <ProximityRadar /> : null}
        <SkillBar />
        <FeedActionBar />
      </div>
    </FlightTelemetryProvider>
  );
}

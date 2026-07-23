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
import { useProfile } from "@/hooks/use-profile";
import { useRoster } from "@/hooks/use-roster";
import { useVisionDetections } from "@/hooks/use-vision-detections";
import { useFeedStore } from "@/stores/feed-store";

export function FeedScreen() {
  const cameras = useRoster();
  const profile = useProfile();
  const activeCameraId = useFeedStore((s) => s.activeCameraId);
  const streamNonce = useFeedStore((s) => s.streamNonce);
  const setActiveStreamLabel = useFeedStore((s) => s.setActiveStreamLabel);

  // The vision engine lives on a companion node (a drone or a workstation/compute
  // box), not a ground station — so the on-box detection feed runs only there. A
  // ground-station cockpit shows received video but its detections would arrive
  // over a different path, not this local socket.
  const visionCapable =
    profile === "drone" ||
    profile === "workstation" ||
    profile === "compute";
  useVisionDetections(visionCapable);

  // Flight-nav aids (minimap / radar) show only where a flying vehicle's state is
  // present: a drone reports its own, a ground station republishes the received
  // drone's. A workstation/compute node has no vehicle, so they stay hidden there.
  const flightNavCapable = profile === "drone" || profile === "ground_station";

  const { whepUrl, reconnectKey, activeLabel } = useMemo(() => {
    const active =
      cameras.find((c) => c.id === activeCameraId) ?? cameras[0] ?? null;
    const url = active?.whep_url ?? "/whep";
    return {
      whepUrl: url,
      reconnectKey: `${active?.id ?? "primary"}:${url}:${streamNonce}`,
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
        <VideoLayer whepUrl={whepUrl} reconnectKey={reconnectKey} />
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

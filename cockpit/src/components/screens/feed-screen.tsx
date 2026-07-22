// The Feed / HUD screen (the default) — the immersive, video-forward flying
// view. It is a full-bleed screen: the WHEP video fills the panel (L0), the
// flight-instrument HUD floats over it (L2), multi-stream tabs appear top-left
// when the node has more than one camera, and the pilot's action bar sits at the
// bottom; the shell floats its own chrome (status strip, menu, utility bar)
// translucently on top. Flight telemetry is provided here so its 5 Hz poll runs
// only while the Feed is on screen.

import { useEffect, useMemo } from "react";

import { FeedActionBar } from "@/components/feed/feed-action-bar";
import { FeedHud } from "@/components/feed/feed-hud";
import { StreamTabs } from "@/components/feed/stream-tabs";
import { VideoLayer } from "@/components/shell/video-layer";
import { FlightTelemetryProvider } from "@/hooks/flight-telemetry-context";
import { useRoster } from "@/hooks/use-roster";
import { useFeedStore } from "@/stores/feed-store";

export function FeedScreen() {
  const cameras = useRoster();
  const activeCameraId = useFeedStore((s) => s.activeCameraId);
  const streamNonce = useFeedStore((s) => s.streamNonce);
  const setActiveStreamLabel = useFeedStore((s) => s.setActiveStreamLabel);

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
        <FeedHud />
        {cameras.length > 1 ? <StreamTabs cameras={cameras} /> : null}
        <FeedActionBar />
      </div>
    </FlightTelemetryProvider>
  );
}

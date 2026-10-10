// The Feed / HUD screen (the default): full-bleed video (L0), extension video
// overlays (L1), the instrument HUD (L2), widget corners, stream tabs, the
// alert stack, the Skill Bar and utility controls, and the confirm sheet. The
// flight-telemetry poll and the extension-state poll run only while the Feed is
// mounted.
//
// Feed request budget with no extensions enabled (drone): /api/telemetry 5/s,
// /api/status/full 1/s (shell), /api/v1/battery 0.2/s (shell), /api/wfb 0.1/s
// (shell), /api/video/roster 1/30 s — about 6.3/s.

import { useEffect, useMemo } from "react";

import { AlertStack } from "@/components/feed/alert-stack";
import { DetectionOverlay } from "@/components/feed/detection-overlay";
import { FeedActionBar } from "@/components/feed/feed-action-bar";
import { HudCanvas } from "@/components/feed/hud-canvas";
import { SafetyBand } from "@/components/feed/safety-band";
import { MiniMap } from "@/components/feed/mini-map";
import { ProximityRadar } from "@/components/feed/proximity-radar";
import { SkillBar } from "@/components/feed/skill-bar";
import { SkillConfirmSheet } from "@/components/feed/skill-confirm-sheet";
import { StreamTabs } from "@/components/feed/stream-tabs";
import { ExtensionFeedMounts } from "@/components/plugins/extension-feed-mounts";
import { VideoLayer } from "@/components/shell/video-layer";
import { useExtensionStatePoll } from "@/hooks/use-extension-state-poll";
import { useFlightTelemetryPoll } from "@/hooks/use-flight-telemetry";
import { useGroundDetectionPoll } from "@/hooks/use-ground-detection-poll";
import { useRoster } from "@/hooks/use-roster";
import { useSkillKeys } from "@/hooks/use-skill-keys";
import { useVisionDetections } from "@/hooks/use-vision-detections";
import { resolveActiveCameraId } from "@/lib/overlay-geometry";
import type { RosterCamera, StatusStream } from "@/lib/types";
import { useProfile } from "@/shared/use-profile";
import { whepUrlFor } from "@/shared/whep";
import { cn } from "@/lib/utils";
import { useFeedStore } from "@/stores/feed-store";
import { useNavStore } from "@/stores/nav-store";
import { useStatusStore } from "@/stores/status-store";

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

export function FeedScreen() {
  const profile = useProfile();
  const isDrone = profile === "drone";
  const roster = useRoster(isDrone);
  const streams = useStatusStore((s) => s.status?.video?.streams);
  const cameras = useMemo(() => feedCameras(streams, roster), [streams, roster]);
  const activeCameraId = useFeedStore((s) => s.activeCameraId);
  const streamNonce = useFeedStore((s) => s.streamNonce);
  const setActiveStreamLabel = useFeedStore((s) => s.setActiveStreamLabel);
  const menuCollapsed = useNavStore((s) => s.menuCollapsed);

  useFlightTelemetryPoll();
  useExtensionStatePoll();
  useSkillKeys();

  // A companion node runs a local vision engine; a ground station receives the
  // linked drone's detections over the relay instead. Both feed one overlay.
  // A drone opens its detection socket only when it reports a working vision
  // path: with none, the route accepts and closes, and dialling it is pure churn.
  const perceptionTier = useStatusStore((s) => s.status?.perception_tier ?? null);
  const droneVision = isDrone && perceptionTier !== null && perceptionTier !== "none";
  const localVisionCapable = droneVision || profile === "workstation" || profile === "compute";
  const visionCapable = localVisionCapable || profile === "ground_station";
  useVisionDetections(localVisionCapable);
  useGroundDetectionPoll();
  const flightNavCapable = isDrone || profile === "ground_station";

  const { whepUrl, hlsUrl, reconnectKey, activeLabel } = useMemo(() => {
    const active = cameras.find((c) => c.id === activeCameraId) ?? cameras[0] ?? null;
    const urls = resolveLegVideoUrls(active);
    return {
      ...urls,
      reconnectKey: `${active?.id ?? "primary"}:${urls.whepUrl}:${streamNonce}`,
      activeLabel: active?.label ?? active?.name ?? active?.role ?? null,
    };
  }, [cameras, activeCameraId, streamNonce]);

  useEffect(() => {
    setActiveStreamLabel(activeLabel);
  }, [activeLabel, setActiveStreamLabel]);

  return (
    <div className="absolute inset-0">
      <VideoLayer whepUrl={whepUrl} hlsUrl={hlsUrl} reconnectKey={reconnectKey} />
      {visionCapable ? (
        <DetectionOverlay
          activeCameraId={resolveActiveCameraId(activeCameraId, cameras)}
          multiStream={cameras.length > 1}
        />
      ) : null}
      <ExtensionFeedMounts />
      {/* Everything the pilot reads or touches sits inside the region the
          shell's chrome leaves free (status strip above, utility bar below,
          menu rail to the left), stacked so nothing overlaps at any size. */}
      <div
        className={cn(
          "pointer-events-none absolute bottom-[3rem] right-[0.4rem] top-[3.1rem] z-30 flex flex-col gap-[0.3rem] portrait:bottom-[6.6rem]",
          menuCollapsed ? "left-[0.4rem]" : "left-[0.4rem] landscape:left-[7rem]",
        )}
      >
        <SafetyBand />
        <div className="flex min-h-0 flex-1 gap-[0.3rem]">
          <div className="relative min-w-0 flex-1">
            <HudCanvas />
            <div className="absolute inset-x-0 top-0 z-40 flex flex-col items-center gap-[0.3rem]">
              <AlertStack />
            </div>
            {cameras.length > 1 ? (
              <div className="absolute left-0 top-0 z-20">
                <StreamTabs cameras={cameras} />
              </div>
            ) : null}
          </div>
          {flightNavCapable ? (
            <div className="z-20 flex shrink-0 flex-col justify-end gap-[0.3rem]">
              <ProximityRadar />
              <MiniMap />
            </div>
          ) : null}
        </div>
        <div className="flex shrink-0 items-end justify-between gap-[0.6rem]">
          <SkillBar />
          <FeedActionBar />
        </div>
      </div>
      <SkillConfirmSheet />
    </div>
  );
}

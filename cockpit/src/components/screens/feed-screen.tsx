// The Feed / HUD screen (the default): video (L0), extension video overlays
// (L1), the host target layer, the instrument HUD (L2), widget zones, stream
// tabs, the alert stack, the Skill Bar and utility controls, and the confirm
// sheet, arranged by the window's width class and orientation (compact, split
// or portrait; see feed-layouts). The flight-telemetry poll and the
// extension-state poll run only while the Feed is mounted.
//
// Feed request budget with no extensions enabled (drone): /api/telemetry 5/s,
// /api/status/full 1/s (shell), /api/v1/battery 0.2/s (shell), /api/wfb 0.1/s
// (shell), /api/video/roster 1/30 s — about 6.3/s. A node with a vision path
// adds POST /api/vision/demand 0.5/s per shown target overlay (main and PiP).

import { useEffect, useMemo, type CSSProperties } from "react";

import { AlertStack } from "@/components/feed/alert-stack";
import { TargetLayer } from "@/components/feed/target-layer";
import { FeedActionBar } from "@/components/feed/feed-action-bar";
import { CompactLayout, PortraitLayout, SplitLayout, type FeedSlots } from "@/components/feed/feed-layouts";
import { FollowChrome } from "@/components/feed/follow-chrome";
import { HudCanvas } from "@/components/feed/hud-canvas";
import { LayerStack } from "@/components/feed/layers/layer-stack";
import { MiniMap } from "@/components/feed/mini-map";
import { ProximityRadar } from "@/components/feed/proximity-radar";
import { PipInset } from "@/components/feed/pip-inset";
import { SafetyBand } from "@/components/feed/safety-band";
import { SkillBar } from "@/components/feed/skill-bar";
import { SkillConfirmSheet } from "@/components/feed/skill-confirm-sheet";
import { StreamTabs } from "@/components/feed/stream-tabs";
import { ExtensionVideoOverlays } from "@/components/plugins/extension-feed-mounts";
import { VideoLayer } from "@/components/shell/video-layer";
import { useExtensionStatePoll } from "@/hooks/use-extension-state-poll";
import { useFlightTelemetryPoll } from "@/hooks/use-flight-telemetry";
import { useGroundDetectionPoll } from "@/hooks/use-ground-detection-poll";
import { useNavStream } from "@/hooks/use-nav-stream";
import { useRoster } from "@/hooks/use-roster";
import { useSkillKeys } from "@/hooks/use-skill-keys";
import { useVisionDetections } from "@/hooks/use-vision-detections";
import { useWidthClass } from "@/hooks/use-width-class";
import { feedLayoutFor, skillSlotPx } from "@/lib/cockpit-layout";
import { cameraLabel, feedCameras, pickPipCamera, resolveLegVideoUrls } from "@/lib/feed-legs";
import { resolveActiveCameraId } from "@/lib/overlay-geometry";
import { useProfile } from "@/shared/use-profile";
import { useFeedStore } from "@/stores/feed-store";
import { useNavStore } from "@/stores/nav-store";
import { useSettingsStore } from "@/stores/settings-store";
import { useStatusStore } from "@/stores/status-store";

export function FeedScreen() {
  const profile = useProfile();
  const isDrone = profile === "drone";
  // A visor carries its own cameras like a drone, so it lists the same roster.
  const ownsCameras = isDrone || profile === "visor";
  const roster = useRoster(ownsCameras);
  const streams = useStatusStore((s) => s.status?.video?.streams);
  const cameras = useMemo(() => feedCameras(streams, roster), [streams, roster]);
  const activeCameraId = useFeedStore((s) => s.activeCameraId);
  const streamNonce = useFeedStore((s) => s.streamNonce);
  const setActiveStreamLabel = useFeedStore((s) => s.setActiveStreamLabel);
  const pipCameraId = useFeedStore((s) => s.pipCameraId);
  const syncLegs = useFeedStore((s) => s.syncLegs);
  const menuCollapsed = useNavStore((s) => s.menuCollapsed);
  const glove = useSettingsStore((s) => s.glove);
  const { widthClass, portrait } = useWidthClass();

  useFlightTelemetryPoll();
  useExtensionStatePoll();
  useSkillKeys();

  // A companion node runs a local vision engine; a ground station receives the
  // linked drone's detections over the relay instead. Both feed one overlay.
  // A camera node (drone or visor) opens its detection socket only when it
  // reports a working vision path: with none, the route accepts and closes,
  // and dialling it is pure churn.
  const perceptionTier = useStatusStore((s) => s.status?.perception_tier ?? null);
  const cameraVision = ownsCameras && perceptionTier !== null && perceptionTier !== "none";
  const localVisionCapable = cameraVision || profile === "workstation" || profile === "compute";
  const visionCapable = localVisionCapable || profile === "ground_station";
  useVisionDetections(localVisionCapable);
  useGroundDetectionPoll();
  const flightNavCapable = isDrone || profile === "ground_station";
  useNavStream(flightNavCapable || localVisionCapable);
  const video = useFeedStore((s) => s.video);

  const { whepUrl, hlsUrl, reconnectKey, activeLabel, mainId, pipCamera } = useMemo(() => {
    const active = cameras.find((c) => c.id === activeCameraId) ?? cameras[0] ?? null;
    const urls = resolveLegVideoUrls(active);
    return {
      ...urls,
      reconnectKey: `${active?.id ?? "primary"}:${urls.whepUrl}:${streamNonce}`,
      activeLabel: active ? cameraLabel(active) : null,
      mainId: active?.id ?? null,
      pipCamera: pickPipCamera(cameras, active?.id ?? null, pipCameraId),
    };
  }, [cameras, activeCameraId, streamNonce, pipCameraId]);

  useEffect(() => {
    setActiveStreamLabel(activeLabel);
  }, [activeLabel, setActiveStreamLabel]);
  const pipId = pipCamera?.id ?? null;
  useEffect(() => {
    syncLegs(mainId, pipId);
  }, [mainId, pipId, syncLegs]);
  useEffect(() => () => syncLegs(null, null), [syncLegs]);

  const slots: FeedSlots = {
    video: (
      <>
        <VideoLayer whepUrl={whepUrl} hlsUrl={hlsUrl} reconnectKey={reconnectKey} />
        <ExtensionVideoOverlays
          cameraId={resolveActiveCameraId(activeCameraId, cameras)}
          multiStream={cameras.length > 1}
          videoWidth={video.width}
          videoHeight={video.height}
        />
        {visionCapable ? (
          <TargetLayer
            cameraId={resolveActiveCameraId(activeCameraId, cameras)}
            multiStream={cameras.length > 1}
            videoState={video.state}
            videoWidth={video.width}
            videoHeight={video.height}
            widthClass={widthClass}
            cameraYawOffsetDeg={
              cameras.find((c) => c.id === resolveActiveCameraId(activeCameraId, cameras))?.role === "down" ? null : 0
            }
            holdDetection={localVisionCapable}
          />
        ) : null}
        <LayerStack
          cameraId={resolveActiveCameraId(activeCameraId, cameras)}
          videoWidth={video.width}
          videoHeight={video.height}
          videoState={video.state}
          widthClass={widthClass}
        />
      </>
    ),
    hud: (
      <>
        <HudCanvas />
        <FollowChrome
          widthClass={widthClass}
          cameras={cameras}
          multiStream={cameras.length > 1}
          hasRadar={flightNavCapable}
          skillClearance={
            feedLayoutFor(widthClass, portrait, flightNavCapable) === "portrait"
              ? "12px"
              : "calc(var(--skill-slot) + 2.25rem)"
          }
        />
      </>
    ),
    band: <SafetyBand />,
    alerts: <AlertStack />,
    tabs: cameras.length > 1 ? <StreamTabs cameras={cameras} /> : null,
    map: flightNavCapable ? (className) => <MiniMap className={className} /> : null,
    radar: flightNavCapable ? (className) => <ProximityRadar className={className} /> : null,
    pip: pipCamera ? (
      <PipInset
        key={pipCamera.id}
        camera={pipCamera}
        streamNonce={streamNonce}
        visionCapable={visionCapable}
        holdDetection={localVisionCapable}
        widthClass={widthClass}
      />
    ) : null,
    skillRow: (
      <>
        <SkillBar />
        <FeedActionBar />
      </>
    ),
    menuCollapsed,
  };

  const layout = feedLayoutFor(widthClass, portrait, flightNavCapable);
  const style = { "--skill-slot": `${skillSlotPx(widthClass, glove)}px` } as CSSProperties;

  return (
    <div className="absolute inset-0" style={style}>
      {layout === "split" ? (
        <SplitLayout slots={slots} />
      ) : layout === "portrait" ? (
        <PortraitLayout slots={slots} />
      ) : (
        <CompactLayout slots={slots} />
      )}
      <SkillConfirmSheet />
    </div>
  );
}

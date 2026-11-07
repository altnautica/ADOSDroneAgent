// The Feed / HUD screen (the default): the cockpit in the DJI grid (cockpit
// spec `layout.json`) for the view on screen (`views.json`): video (L0),
// extension video overlays (L1), the host target layer, the data layers
// (AR, obstacles, depth, surfaces), the instrument HUD (L2), the top bar with
// the flight-state chip, the left and right columns, the bottom-left card, the
// telemetry row, the skill picker, the flow overlays and the cockpit sheets.
// The flight-telemetry poll, the extension-state poll, the pilot link and the
// stick stream run only while the Feed is mounted.
//
// Feed request budget with no extensions enabled (drone): /api/telemetry 5/s,
// /api/status/full 1/s (shell), /api/v1/battery 0.2/s (shell), /api/wfb 0.1/s
// (shell), /api/video/roster 1/30 s, /api/plugins 0.2/s — about 6.5/s, plus
// the pilot link (`WS /api/v1/pilot/stream`) and the nav stream. A node with a
// vision path adds POST /api/vision/demand 0.5/s per shown target overlay.

import { useEffect, useMemo, type CSSProperties } from "react";

import { AlertStack } from "@/components/feed/alert-stack";
import { BottomRow, LeftColumn, RightColumn } from "@/components/feed/cockpit/cockpit-zones";
import { FlightOverlays } from "@/components/feed/cockpit/flight-overlays";
import { LaunchSheet } from "@/components/feed/cockpit/launch-sheet";
import { ModeSheet } from "@/components/feed/cockpit/mode-sheet";
import { PreflightSheet } from "@/components/feed/cockpit/preflight-sheet";
import { AttitudeRadarCard, ObstacleRadarCard } from "@/components/feed/cockpit/radar-cards";
import { TopBar } from "@/components/feed/cockpit/top-bar";
import { LandscapeGrid, PortraitGrid, type FeedSlots } from "@/components/feed/feed-layouts";
import { FollowChrome } from "@/components/feed/follow-chrome";
import { HudCanvas } from "@/components/feed/hud-canvas";
import { LayerStack } from "@/components/feed/layers/layer-stack";
import { VisionPanel } from "@/components/feed/layers/vision-panel";
import { MiniMap } from "@/components/feed/mini-map";
import { OnscreenSticks } from "@/components/feed/onscreen-sticks";
import { PipInset } from "@/components/feed/pip-inset";
import { SkillConfirmSheet } from "@/components/feed/skill-confirm-sheet";
import { SkillAckLine, SkillPicker } from "@/components/feed/skill-picker/skill-picker";
import { StreamTabs } from "@/components/feed/stream-tabs";
import { TargetLayer } from "@/components/feed/target-layer";
import { ExtensionVideoOverlays } from "@/components/plugins/extension-feed-mounts";
import { VideoLayer } from "@/components/shell/video-layer";
import { useExtensionStatePoll } from "@/hooks/use-extension-state-poll";
import { useFollowSkill } from "@/hooks/use-follow-skill";
import { useFlightTelemetryPoll } from "@/hooks/use-flight-telemetry";
import { useGroundDetectionPoll } from "@/hooks/use-ground-detection-poll";
import { useNavStream } from "@/hooks/use-nav-stream";
import { usePilotInput } from "@/hooks/use-pilot-input";
import { useRoster } from "@/hooks/use-roster";
import { useSkillKeys } from "@/hooks/use-skill-keys";
import { useVisionDetections } from "@/hooks/use-vision-detections";
import { useWidthClass } from "@/hooks/use-width-class";
import { registerCameraActions } from "@/lib/camera-actions";
import { registerFeedActions } from "@/lib/feed-actions";
import { gridMeasures } from "@/lib/cockpit-layout";
import { effectiveLayers } from "@/lib/cockpit-view";
import { cameraLabel, feedCameras, pickPipCamera, resolveLegVideoUrls } from "@/lib/feed-legs";
import { isKiosk } from "@/lib/kiosk";
import { resolveActiveCameraId } from "@/lib/overlay-geometry";
import { connectPilotLink } from "@/lib/pilot-link";
import { useProfile } from "@/shared/use-profile";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";
import { useFeedStore } from "@/stores/feed-store";
import { useNavStore } from "@/stores/nav-store";
import { useSettingsStore } from "@/stores/settings-store";
import { useStatusStore } from "@/stores/status-store";

function CockpitSheets() {
  const sheet = useCockpitUiStore((s) => s.sheet);
  if (sheet === "mode") return <ModeSheet />;
  if (sheet === "launch") return <LaunchSheet />;
  if (sheet === "preflight") return <PreflightSheet />;
  return null;
}

export function FeedScreen() {
  const profile = useProfile();
  const isDrone = profile === "drone";
  const groundStation = profile === "ground_station";
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
  const m = gridMeasures(widthClass, glove);
  const vehicleId = useStatusStore((s) => (groundStation ? s.status?.paired_drone?.device_id : null) ?? "local");
  const bindVehicle = useCockpitViewStore((s) => s.bindVehicle);
  const view = useCockpitViewStore((s) => s.view);
  const level = useCockpitViewStore((s) => s.level);
  const layerFlags = useCockpitViewStore((s) => s.layers);
  const followSkill = useFollowSkill();

  useEffect(() => {
    bindVehicle(vehicleId, m.defaultLevel);
  }, [bindVehicle, vehicleId, m.defaultLevel]);

  useFlightTelemetryPoll();
  useExtensionStatePoll();
  useSkillKeys();

  // A companion node runs a local vision engine; a ground station receives the
  // linked drone's detections over the relay instead. Both feed one overlay.
  const perceptionTier = useStatusStore((s) => s.status?.perception_tier ?? null);
  const cameraVision = ownsCameras && perceptionTier !== null && perceptionTier !== "none";
  const localVisionCapable = cameraVision || profile === "workstation" || profile === "compute";
  const visionCapable = localVisionCapable || groundStation;
  useVisionDetections(localVisionCapable);
  useGroundDetectionPoll();
  const flightNavCapable = isDrone || groundStation;
  useNavStream(flightNavCapable || localVisionCapable);
  const video = useFeedStore((s) => s.video);

  // The operator link to the pilot and the stick stream (C1, C4), only on a
  // node that flies; the actions every binding fires through.
  useEffect(() => (flightNavCapable ? connectPilotLink(isKiosk() && groundStation ? "kiosk" : "cockpit") : undefined), [
    flightNavCapable,
    groundStation,
  ]);
  usePilotInput();
  useEffect(() => registerCameraActions(groundStation), [groundStation]);
  useEffect(() => registerFeedActions(), []);

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

  const cameraId = resolveActiveCameraId(activeCameraId, cameras);
  const layers = effectiveLayers(level, layerFlags, view);

  const slots: FeedSlots = {
    video: (
      <>
        <VideoLayer whepUrl={whepUrl} hlsUrl={hlsUrl} reconnectKey={reconnectKey} />
        <ExtensionVideoOverlays cameraId={cameraId} multiStream={cameras.length > 1} videoWidth={video.width} videoHeight={video.height} />
        {visionCapable && layers.has("targets") ? (
          <TargetLayer
            cameraId={cameraId}
            multiStream={cameras.length > 1}
            videoState={video.state}
            videoWidth={video.width}
            videoHeight={video.height}
            widthClass={widthClass}
            cameraYawOffsetDeg={cameras.find((c) => c.id === cameraId)?.role === "down" ? null : 0}
            holdDetection={localVisionCapable}
          />
        ) : null}
        <LayerStack cameraId={cameraId} videoWidth={video.width} videoHeight={video.height} videoState={video.state} widthClass={widthClass} />
      </>
    ),
    hud: (
      <>
        {layers.has("hud") ? <HudCanvas /> : null}
        <FollowChrome
          widthClass={widthClass}
          cameras={cameras}
          multiStream={cameras.length > 1}
          hasRadar={false}
          skillClearance={portrait ? "12px" : "calc(var(--cockpit-col) + 3.5rem)"}
        />
      </>
    ),
    topBar: <TopBar compact={widthClass === "compact"} />,
    alerts: <AlertStack />,
    overlays: <FlightOverlays />,
    tabs: cameras.length > 1 ? <StreamTabs cameras={cameras} /> : null,
    map: flightNavCapable ? (className) => <MiniMap className={className} /> : null,
    cards: flightNavCapable
      ? {
          map: (className) => <MiniMap className={className} />,
          attitude_radar: (className) => <AttitudeRadarCard className={className} />,
          obstacle_radar: (className) => <ObstacleRadarCard className={className} />,
        }
      : {},
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
    leftColumn: (row) => (flightNavCapable ? <LeftColumn row={row} /> : null),
    rightColumn: <RightColumn compact={widthClass === "compact"} />,
    bottomRow: flightNavCapable ? <BottomRow distanceOnly={m.distanceOnly} /> : null,
    skillPicker: <SkillPicker />,
    ack: <SkillAckLine />,
    visionPanel: <VisionPanel cameraId={cameraId} />,
    onscreenSticks: <OnscreenSticks />,
    railPresent: followSkill !== null,
    menuCollapsed,
  };

  const style = {
    "--cockpit-col": `${m.colPx}px`,
    "--cockpit-shutter": `${m.shutterPx}px`,
  } as CSSProperties;

  return (
    <div className="absolute inset-0" style={style} data-focus-rings={m.focusRingsAlways ? "always" : undefined} data-width-class={widthClass}>
      {portrait ? <PortraitGrid slots={slots} /> : <LandscapeGrid slots={slots} m={m} view={view} />}
      <SkillConfirmSheet />
      <CockpitSheets />
    </div>
  );
}

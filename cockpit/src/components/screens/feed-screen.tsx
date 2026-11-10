// The Feed / HUD screen (the default), the whole viewport: the cockpit in the
// cockpit grid (cockpit spec `layout.json`) for the view on screen
// (`views.json`): video (L0), extension video overlays (L1), the host target
// layer, the data layers (AR, obstacles, depth, surfaces), the centre marks
// and instruments (L2), the top bar with the flight-state chip and STOP, the
// left and right columns, the bottom-left card, the telemetry row, the skill
// picker, the follow strip, the flow overlays, the cockpit sheets and the
// contextual tips (`tip-layer.tsx`). No shell
// chrome is drawn around it; every other screen is behind the top bar's Menu.
// The flight-telemetry poll, the extension-state poll, the pilot link, the
// stick stream and the gimbal driver run only while the Feed is mounted.
//
// Feed request budget with no extensions enabled (drone): /api/telemetry 5/s,
// /api/status/full 1/s (shell), /api/v1/battery 0.2/s (shell), /api/wfb 0.1/s
// (shell), /api/video/roster 1/30 s, /api/plugins 0.2/s — about 6.5/s, plus
// the pilot link (`WS /api/v1/pilot/stream`) and the nav stream. A node with a
// vision path adds POST /api/vision/demand 0.5/s per shown target overlay.

import { useEffect, useMemo, useRef, type CSSProperties } from "react";

import { AlertStack } from "@/components/feed/alert-stack";
import { BottomRow, LeftColumn, RightColumn } from "@/components/feed/cockpit/cockpit-zones";
import { FlightOverlays } from "@/components/feed/cockpit/flight-overlays";
import { LaunchSheet } from "@/components/feed/cockpit/launch-sheet";
import { ModeSheet } from "@/components/feed/cockpit/mode-sheet";
import { PreflightSheet } from "@/components/feed/cockpit/preflight-sheet";
import { AttitudeRadarCard, ObstacleRadarCard } from "@/components/feed/cockpit/radar-cards";
import { TopBar } from "@/components/feed/cockpit/top-bar";
import { LandscapeGrid, PortraitGrid, type FeedSlots } from "@/components/feed/feed-layouts";
import { FollowStrip } from "@/components/feed/follow-chrome";
import { useFollowStripState } from "@/hooks/use-follow-strip";
import { HudCanvas } from "@/components/feed/hud-canvas";
import { LayerChips } from "@/components/feed/layers/layer-chips";
import { LayerStack } from "@/components/feed/layers/layer-stack";
import { VisionPanel } from "@/components/feed/layers/vision-panel";
import { MiniMap } from "@/components/feed/mini-map";
import { OnscreenSticks } from "@/components/feed/onscreen-sticks";
import { PipInset } from "@/components/feed/pip-inset";
import { SkillConfirmSheet } from "@/components/feed/skill-confirm-sheet";
import { SkillAckLine, SkillPicker } from "@/components/feed/skill-picker/skill-picker";
import { TargetLayer } from "@/components/feed/target-layer";
import { TipLayer } from "@/components/feed/tip-layer";
import { ExtensionVideoOverlays } from "@/components/plugins/extension-feed-mounts";
import { VideoLayer } from "@/components/shell/video-layer";
import { BannerHost } from "@/components/shell/banner-host";
import { ReachNotice } from "@/components/shell/reach-notice";
import { RebootBanner } from "@/components/shell/reboot-banner";
import { useExtensionStatePoll } from "@/hooks/use-extension-state-poll";
import { useElementSize } from "@/hooks/use-element-size";
import { useFlightTelemetryPoll } from "@/hooks/use-flight-telemetry";
import { useGimbalAvailable, useGimbalDriver } from "@/hooks/use-gimbal-rate";
import { useGroundDetectionPoll } from "@/hooks/use-ground-detection-poll";
import { useNavStream } from "@/hooks/use-nav-stream";
import { usePilotInput } from "@/hooks/use-pilot-input";
import { useRoster } from "@/hooks/use-roster";
import { useSkillKeys } from "@/hooks/use-skill-keys";
import { hasLocalVision, useVisionDetections } from "@/hooks/use-vision-detections";
import { useWidthClass } from "@/hooks/use-width-class";
import { registerCameraActions } from "@/lib/camera-actions";
import { pushCommandHandler } from "@/lib/command-stack";
import { feedNavCommand, registerFeedActions } from "@/lib/feed-actions";
import { chromeStyle } from "@/lib/chrome-tokens";
import { gridMeasures } from "@/lib/cockpit-layout";
import { effectiveLayers, pluginLayerId } from "@/lib/cockpit-view";
import { mountablePanels } from "@/lib/extensions";
import { cameraLabel, feedCameras, pickPipCamera, resolveLegVideoUrls } from "@/lib/feed-legs";
import { isKiosk } from "@/lib/kiosk";
import { resolveActiveCameraId } from "@/lib/overlay-geometry";
import { connectPilotLink } from "@/lib/pilot-link";
import { useProfile } from "@/shared/use-profile";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";
import { useFeedStore } from "@/stores/feed-store";
import { useExtensionsStore } from "@/stores/extensions-store";
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
  const rootRef = useRef<HTMLDivElement>(null);
  const size = useElementSize(rootRef);
  const glove = useSettingsStore((s) => s.glove);
  const { widthClass, portrait } = useWidthClass();
  const m = gridMeasures(widthClass, glove);
  const vehicleId = useStatusStore((s) => (groundStation ? s.status?.paired_drone?.device_id : null) ?? "local");
  const bindVehicle = useCockpitViewStore((s) => s.bindVehicle);
  const view = useCockpitViewStore((s) => s.view);
  const level = useCockpitViewStore((s) => s.level);
  const layerFlags = useCockpitViewStore((s) => s.layers);
  const strip = useFollowStripState();
  const extensions = useExtensionsStore((s) => s.extensions);
  const pluginOverlayIds = useMemo(
    () => extensions.flatMap((ext) => mountablePanels(ext, "video.overlay").map((p) => pluginLayerId(ext.pluginId, p.id))),
    [extensions],
  );

  useEffect(() => {
    bindVehicle(vehicleId, m.defaultLevel);
  }, [bindVehicle, vehicleId, m.defaultLevel]);

  useFlightTelemetryPoll();
  useExtensionStatePoll();
  useSkillKeys();

  // A companion node runs a local vision engine; a ground station receives the
  // linked drone's detections over the relay instead. Both feed one overlay.
  const perceptionTier = useStatusStore((s) => s.status?.perception_tier ?? null);
  const visionEngine = useStatusStore((s) => s.status?.vision_engine ?? null);
  const localVisionCapable = hasLocalVision({ ownsCameras, perceptionTier, visionEngine, profile });
  const visionCapable = localVisionCapable || groundStation;
  useVisionDetections(localVisionCapable);
  useGroundDetectionPoll();
  const flightNavCapable = isDrone || groundStation;
  useNavStream(flightNavCapable || localVisionCapable);
  const video = useFeedStore((s) => s.video);

  // The operator link to the pilot and the stick stream , only on a
  // node that flies; the actions every binding fires through.
  useEffect(() => (flightNavCapable ? connectPilotLink(isKiosk() && groundStation ? "kiosk" : "cockpit") : undefined), [
    flightNavCapable,
    groundStation,
  ]);
  usePilotInput();
  useEffect(() => registerCameraActions(groundStation), [groundStation]);
  useEffect(() => registerFeedActions(), []);
  useEffect(() => pushCommandHandler(feedNavCommand), []);
  const gimbal = useGimbalAvailable(flightNavCapable);
  useGimbalDriver(gimbal);

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
  const layers = effectiveLayers(level, layerFlags, view, pluginOverlayIds);

  const slots: FeedSlots = {
    video: <VideoLayer whepUrl={whepUrl} hlsUrl={hlsUrl} reconnectKey={reconnectKey} />,
    layers: (
      <>
        <ExtensionVideoOverlays
          cameraId={cameraId}
          multiStream={cameras.length > 1}
          videoWidth={video.width}
          videoHeight={video.height}
          layers={layers}
        />
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
        <LayerStack cameraId={cameraId} videoWidth={video.width} videoHeight={video.height} videoState={video.state} fpsCap={m.overlayFpsCap ?? 0} />
      </>
    ),
    hud: layers.has("centre_marks") ? <HudCanvas instruments={layers.has("instruments")} /> : null,
    followStrip:
      strip.designated || strip.following
        ? (bandHeightPx) => (
            <FollowStrip
              widthClass={widthClass}
              cameras={cameras}
              multiStream={cameras.length > 1}
              following={strip.following}
              designated={strip.designated}
              bandHeightPx={bandHeightPx}
            />
          )
        : null,
    topBar: <TopBar compact={widthClass === "compact"} />,
    alerts: (
      <>
        <div className="pointer-events-auto flex w-full max-w-[560px] flex-col overflow-hidden rounded-[var(--fs-radius-sm)] empty:hidden" data-testid="feed-notices">
          <ReachNotice />
          <BannerHost fullBleed />
          <RebootBanner floating />
        </div>
        <div data-zone="alert_stack">
          <AlertStack />
        </div>
      </>
    ),
    overlays: (
      <>
        <FlightOverlays />
        <LayerChips />
      </>
    ),

    map: flightNavCapable ? (className) => <MiniMap className={className} /> : null,
    cards: flightNavCapable
      ? {
          map: (className) => <MiniMap className={className} />,
          attitude_radar: (className) => <AttitudeRadarCard className={className} />,
          obstacle_radar: (className) => <ObstacleRadarCard className={className} />,
        }
      : {},
    pip: pipCamera ? <PipInset key={pipCamera.id} camera={pipCamera} streamNonce={streamNonce} /> : null,
    leftColumn: (o) => (flightNavCapable ? <LeftColumn {...o} /> : null),
    rightColumn: (items) => <RightColumn items={items} cameras={cameras} />,
    bottomRow: flightNavCapable ? (short) => <BottomRow short={short} /> : null,
    skillPicker: <SkillPicker />,
    ack: <SkillAckLine />,
    visionPanel: <VisionPanel cameraId={cameraId} />,
    onscreenSticks: <OnscreenSticks />,
  };

  const style = {
    ...chromeStyle(),
    "--cockpit-col": `${m.colPx}px`,
    "--cockpit-shutter": `${m.shutterPx}px`,
  } as CSSProperties;

  return (
    <div
      ref={rootRef}
      className="absolute inset-0 overflow-hidden bg-black"
      style={style}
      data-focus-rings={m.focusRingsAlways ? "always" : undefined}
      data-width-class={widthClass}
      data-layer-level={level}
    >
      {portrait ? (
        <PortraitGrid slots={slots} />
      ) : size.w > 0 ? (
        <LandscapeGrid
          slots={slots}
          m={m}
          input={{
            width: size.w,
            height: size.h,
            m,
            widthClass,
            view,
            flies: flightNavCapable,
            strip: slots.followStrip !== null,
            gimbal,
          }}
        />
      ) : null}
      <SkillConfirmSheet />
      <CockpitSheets />
      <TipLayer gimbalAvailable={gimbal} scale={m.scale} />
    </div>
  );
}

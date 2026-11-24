// The ground station's video receive state on its Radio link screen, so a
// decode fault can be read next to the link that feeds it. Read-only; the
// view is shared with the dashboard (`@/shared/api/video-receive`).

import { useCallback } from "react";
import { useTranslation } from "react-i18next";

import { Dot, Row, SectionHeader } from "@/components/ui/data";
import { apiFetch } from "@/shared/api-fetch";
import {
  VIDEO_LATENCY_PATH,
  VIDEO_STATUS_PATH,
  videoReceiveView,
  type VideoLatencyWire,
  type VideoStatusWire,
} from "@/shared/api/video-receive";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";

export function VideoReceiveSection() {
  const { t } = useTranslation();
  const video = useResource<VideoStatusWire>(
    useCallback((s) => apiFetch<VideoStatusWire>(VIDEO_STATUS_PATH, { signal: s }), []),
    2000,
  );
  const latency = useResource<VideoLatencyWire>(
    useCallback((s) => apiFetch<VideoLatencyWire>(VIDEO_LATENCY_PATH, { signal: s }), []),
    2000,
  );
  const view = videoReceiveView(video.data, latency.data);

  return (
    <>
      <SectionHeader>{t("cockpit.link.video.title")}</SectionHeader>
      <Row label={t("cockpit.link.video.stream")} left={<Dot tone={view.tone} />} value={t(view.stateKey)} tone={view.tone} />
      <Row label={t("cockpit.link.video.tracks")} value={view.tracks} />
      <Row label={t("cockpit.link.video.viewers")} value={view.viewers} />
      <Row label={t("cockpit.link.video.latency")} value={view.latency} hint={t("cockpit.link.video.latencyHint")} />
    </>
  );
}

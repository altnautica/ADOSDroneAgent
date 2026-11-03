// The video pipeline on the System screen: its state, encoder settings,
// measured bitrate and glass-to-glass latency, why there is no picture when
// there is none (a ground station waits on the drone's WFB stream first), a
// still frame grabbed on demand from the live stream, and a jump to the Feed,
// which is the live player. Self-polling.

import { useCallback, useEffect, useState } from "react";
import { Image as ImageIcon, Play } from "lucide-react";
import { useTranslation } from "react-i18next";

import { ActionButton, EmptyNote, Row, SectionHeader } from "@/components/ui/data";
import { videoIdleReason } from "@/lib/home-panels";
import { apiFetch } from "@/shared/api-fetch";
import type { SetupStatus } from "@/shared/agent-api.generated";
import { capturedLabel, fetchSnapshot } from "@/shared/api/snapshot";
import { toastFromError } from "@/shared/api/toast";
import type { DashboardSnapshot } from "@/shared/api/types";
import { fmtBitrate, fmtNum } from "@/shared/format";
import "@/shared/i18n";
import { useProfile } from "@/shared/use-profile";
import { useResource } from "@/shared/use-resource";
import { useNavStore } from "@/stores/nav-store";

interface WfbLite {
  state?: string | null;
  packets_received?: number | null;
  actual_channel?: number | null;
}

export function VideoCard() {
  const { t } = useTranslation();
  const isGround = useProfile() === "ground_station";
  const goTab = useNavStore((s) => s.goTab);
  const snap = useResource(
    useCallback((s: AbortSignal) => apiFetch<DashboardSnapshot>("/api/v1/dashboard/snapshot", { signal: s }), []),
    2000,
  );
  const setup = useResource(
    useCallback((s: AbortSignal) => apiFetch<SetupStatus>("/api/v1/setup/status", { signal: s }), []),
    20_000,
  );
  const wfb = useResource(
    useCallback((s: AbortSignal) => apiFetch<WfbLite>("/api/wfb", { signal: s }), []),
    2000,
    { enabled: isGround },
  );
  const [still, setStill] = useState<{ url: string; at: Date | null } | null>(null);
  const [grabbing, setGrabbing] = useState(false);
  useEffect(() => () => {
    if (still) URL.revokeObjectURL(still.url);
  }, [still]);

  const v = snap.data?.video;
  const access = setup.data?.video;
  const haveUrl = Boolean(access?.hls_url || access?.public_whep_url || access?.public_hls_url);
  const reason = videoIdleReason({
    pipelineState: v?.state,
    isGround,
    wfbPackets: wfb.data?.packets_received,
    haveStreamUrl: haveUrl,
  });
  const wfbDetail = wfb.data?.actual_channel
    ? t("video.panel.wfbWaitDetailChannel", { state: wfb.data?.state ?? "unknown", channel: wfb.data.actual_channel })
    : t("video.panel.wfbWaitDetail", { state: wfb.data?.state ?? "unknown" });

  const grab = () => {
    if (grabbing) return;
    setGrabbing(true);
    fetchSnapshot()
      .then((shot) => setStill({ url: URL.createObjectURL(shot.blob), at: shot.capturedAt }))
      .catch((err: unknown) => toastFromError(err, t("video.panel.streamUnavailable")))
      .finally(() => setGrabbing(false));
  };

  return (
    <div className="flex flex-col gap-[0.15rem]">
      <SectionHeader>{t("video.panel.title")}</SectionHeader>
      {reason ? (
        <EmptyNote>
          {t(`video.panel.${reason}`)}
          {reason === "waitingForWfb" ? ` · ${wfbDetail}` : ""}
        </EmptyNote>
      ) : null}
      <Row label={t("video.fields.pipeline")} value={v?.state ?? access?.pipeline_state ?? "unknown"} />
      {v?.codec ? <Row label={t("video.fields.codec")} value={v.codec} /> : null}
      {v?.width && v?.height ? <Row label={t("video.fields.res")} value={`${v.width}×${v.height}`} /> : null}
      {v?.fps ? <Row label={t("video.fields.fps")} value={fmtNum(v.fps, 0)} /> : null}
      {v?.bitrate_kbps ? <Row label={t("video.fields.bitrate")} value={fmtBitrate(v.bitrate_kbps)} /> : null}
      {v?.glass_to_glass_ms != null ? <Row label={t("video.fields.g2g")} value={`${fmtNum(v.glass_to_glass_ms, 0)} ms`} /> : null}
      {still ? (
        <figure className="mt-[0.3rem] overflow-hidden rounded-md bg-letterbox">
          <img src={still.url} alt={t("video.panel.lastSnapshot")} className="aspect-video w-full object-contain" />
          <figcaption className="px-[0.4rem] py-[0.2rem] text-[0.75rem] text-muted-foreground">
            {t("video.panel.snapshotOnly")}
            {still.at ? ` · ${capturedLabel(still.at)}` : ""}
          </figcaption>
        </figure>
      ) : null}
      <div className="mt-[0.3rem] flex gap-[0.4rem]">
        <ActionButton label={t("video.panel.lastSnapshot")} icon={ImageIcon} onClick={grab} busy={grabbing} disabled={v?.state !== "running"} full />
        <ActionButton label={t("panels.cockpit.enter")} icon={Play} variant="primary" onClick={() => goTab("feed")} full />
      </div>
    </div>
  );
}

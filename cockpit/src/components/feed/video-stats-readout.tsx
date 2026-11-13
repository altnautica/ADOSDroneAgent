// The main video's receive rates as a debug readout (`?debug=video`): frames
// decoded against frames shown per second, drops, the inter-frame delay's
// mean and spread, the jitter buffer's hold, the bitrate and the decoder, from
// the transport's once-a-second `getStats()` sample. A decoded rate above the
// shown rate points at the renderer; a received rate falling with the bitrate
// points at the link.

import i18n from "@/shared/i18n";
import type { VideoStats } from "@/shared/video-stats";

const fmt = (v: number | null, digits = 0) => (v === null ? "–" : v.toFixed(digits));

function videoStatsLines(s: VideoStats): string[] {
  return [
    i18n.t("video.stats.rates", { decoded: fmt(s.decodedFps, 1), shown: fmt(s.shownFps, 1), received: fmt(s.receivedFps, 1) }),
    i18n.t("video.stats.timing", {
      dropped: fmt(s.droppedFrames),
      mean: fmt(s.interFrameMs, 1),
      sd: fmt(s.interFrameSdMs, 1),
      jitter: fmt(s.jitterBufferMs),
    }),
    i18n.t("video.stats.link", { kbps: fmt(s.kbps), decoder: s.decoder ?? "–" }),
  ];
}

export function VideoStatsReadout({ stats }: { stats: VideoStats | null }) {
  if (!stats) return null;
  return (
    <div
      className="pointer-events-none absolute bottom-[30%] left-1/2 -translate-x-1/2 rounded-md bg-hud-glass-strong px-[10px] py-[6px] font-mono text-[12px] leading-[16px] text-hud-ink"
      data-testid="video-stats"
    >
      {videoStatsLines(stats).map((line, i) => (
        <div key={i}>{line}</div>
      ))}
    </div>
  );
}

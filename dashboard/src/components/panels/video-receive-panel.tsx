// The ground station's video receive state on its Radio link page, so a
// decode fault can be read next to the link that feeds it. Read-only; the
// view is shared with the cockpit (`@/shared/api/video-receive`).

import { Video } from "lucide-react";
import { useTranslation } from "react-i18next";

import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { useResource } from "@/hooks/use-resource";
import {
  VIDEO_LATENCY_PATH,
  VIDEO_STATUS_PATH,
  videoReceiveView,
  type VideoLatencyWire,
  type VideoStatusWire,
} from "@/shared/api/video-receive";

const POLL_MS = 2_000;

const TONE_CLASS = { ok: "text-ok", warn: "text-warn", muted: "text-muted-foreground" } as const;

export function VideoReceivePanel() {
  const { t } = useTranslation();
  const video = useResource<VideoStatusWire>("video-status", VIDEO_STATUS_PATH, POLL_MS);
  const latency = useResource<VideoLatencyWire>("video-latency", VIDEO_LATENCY_PATH, POLL_MS);
  const view = videoReceiveView(video.data, latency.data);

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Video className="h-3.5 w-3.5" />
          {t("cockpit.link.video.title")}
        </CardTitle>
      </CardHeader>
      <CardContent>
        <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-sm">
          <div className="text-xs text-muted-foreground">{t("cockpit.link.video.stream")}</div>
          <div className={`text-xs ${TONE_CLASS[view.tone]}`}>{t(view.stateKey)}</div>
          <div className="text-xs text-muted-foreground">{t("cockpit.link.video.tracks")}</div>
          <div className="font-mono text-xs">{view.tracks}</div>
          <div className="text-xs text-muted-foreground">{t("cockpit.link.video.viewers")}</div>
          <div className="font-mono text-xs">{view.viewers}</div>
          <div className="text-xs text-muted-foreground">{t("cockpit.link.video.latency")}</div>
          <div className="font-mono text-xs">{view.latency}</div>
        </div>
      </CardContent>
    </Card>
  );
}

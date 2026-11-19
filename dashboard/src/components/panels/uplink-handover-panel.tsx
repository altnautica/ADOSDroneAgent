// The radio uplink on a ground station's Receive page: whether this box
// transmits to the fleet or a phone holds the uplink
// (`GET /api/v1/ground-station/uplink`). While a phone holds it the box only
// receives; the operator takes it back after a confirm. A null block (the
// radio service is not reporting) reads as unknown, never as transmitting; a
// node without the route (not a ground station, an older agent) shows nothing.

import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { RadioTower } from "lucide-react";
import { useTranslation } from "react-i18next";

import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { useNow } from "@/hooks/use-now";
import { useResource } from "@/hooks/use-resource";
import { ApiError } from "@/shared/api-fetch";
import { toast, toastFromError } from "@/shared/api/toast";
import { UPLINK_PATH, takeBackUplink, uplinkView } from "@/shared/api/uplink-handover";
import { fmtRelativeTime } from "@/shared/format";

const UPLINK_KEY = "gs-uplink";
const UPLINK_POLL_MS = 2_000;

export function UplinkHandoverPanel() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const read = useResource<{ uplink?: unknown }>(UPLINK_KEY, UPLINK_PATH, UPLINK_POLL_MS);
  const [confirming, setConfirming] = useState(false);
  // "Last heard" ages with the clock between polls.
  useNow(1_000);

  if (read.error instanceof ApiError && read.error.status === 404) return null;
  if (read.data === undefined) return null;

  const view = uplinkView(read.data.uplink);
  const holder = view?.holder ?? t("wfb.uplink.aPhone");
  const line =
    view === null
      ? t("wfb.uplink.unknown")
      : view.state === "transmitting"
        ? t("wfb.uplink.transmitting")
        : view.sinceMs === null
          ? t("wfb.uplink.standby", { holder })
          : t("wfb.uplink.standbySince", {
              holder,
              since: new Date(view.sinceMs).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }),
            });

  const takeBack = async () => {
    try {
      await takeBackUplink();
      toast.ok(t("wfb.uplink.takenBack"));
      await queryClient.invalidateQueries({ queryKey: [UPLINK_KEY] });
    } catch (err) {
      toastFromError(err, t("wfb.uplink.takeBackFailed"));
      throw err;
    }
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <RadioTower className="h-3.5 w-3.5" />
          {t("wfb.uplink.title")}
          {view?.state === "standby" ? (
            <Badge variant="warning" className="ml-auto">
              {holder}
            </Badge>
          ) : null}
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-2 text-sm">
        <p className={view?.state === "standby" ? "text-warn" : undefined}>{line}</p>
        {view === null ? (
          <p className="text-xs text-muted-foreground">{t("wfb.uplink.unknownHint")}</p>
        ) : view.state === "standby" ? (
          <>
            <p className="text-xs text-muted-foreground">{t("wfb.uplink.standbyHint", { holder })}</p>
            <div className="grid grid-cols-2 gap-x-4 text-sm">
              <div className="text-xs text-muted-foreground">{t("wfb.uplink.lastHeard")}</div>
              <div className="font-mono">
                {view.lastBeaconMs === null ? t("wfb.uplink.notHeard") : fmtRelativeTime(view.lastBeaconMs)}
              </div>
            </div>
            <Button variant="destructive" size="sm" onClick={() => setConfirming(true)}>
              {t("wfb.uplink.takeBack")}
            </Button>
          </>
        ) : null}
      </CardContent>
      <ConfirmDialog
        open={confirming}
        onOpenChange={setConfirming}
        title={t("wfb.uplink.confirmTitle")}
        description={t("wfb.uplink.confirmBody", { holder })}
        confirmLabel={t("wfb.uplink.takeBack")}
        destructive
        onConfirm={takeBack}
      />
    </Card>
  );
}

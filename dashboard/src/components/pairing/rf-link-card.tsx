import { Radio, Unlink } from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { ReadErrorCard } from "@/components/read-error-card";
import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { useResource } from "@/hooks/use-resource";
import { apiFetch } from "@/shared/api-fetch";
import { withArmedRetry } from "@/shared/api/armed-retry";
import { toast, toastFromError } from "@/shared/api/toast";
import { useProfile } from "@/shared/use-profile";

interface WfbPair {
  paired?: boolean;
  paired_with_device_id?: string | null;
  fingerprint?: string | null;
  auto_pair_enabled?: boolean;
  role?: string | null;
}

/** The radio (RF) link binding, from the same endpoints the cockpit's Pair
 *  screen reads. Unbinding wipes the radio key; the agent refuses it while
 *  armed unless forced, and the force is a second, explicit question. */
export function RfLinkCard() {
  const { t } = useTranslation();
  // The far end of the radio link: a ground station binds a drone, every
  // other node binds with a ground station.
  const peer = t(useProfile() === "ground_station" ? "pairing.subject.drone" : "pairing.subject.groundStation");
  const rf = useResource<WfbPair>("wfb-pair", "/api/wfb/pair", 5_000);
  const [confirm, setConfirm] = useState(false);
  const [armedPrompt, setArmedPrompt] = useState<{ resolve: (ok: boolean) => void } | null>(null);
  const known = rf.data != null && !rf.isError;
  const paired = rf.data?.paired === true;

  async function unbind() {
    try {
      await withArmedRetry(
        (force) => apiFetch(force ? "/api/wfb/pair/unpair?force=1" : "/api/wfb/pair/unpair", { method: "POST", body: {} }),
        () => {
          setConfirm(false);
          return new Promise<boolean>((resolve) => setArmedPrompt({ resolve }));
        },
      );
      toast.ok(t("cockpit.pair.rfUnpaired", { peer }));
    } catch (err) {
      toastFromError(err, t("pairing.page.unpairFailed"));
    } finally {
      void rf.refetch();
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center justify-between">
          <span className="flex items-center gap-2">
            <Radio className="h-3.5 w-3.5" />
            {t("cockpit.pair.rfLink")}
          </span>
          {known && paired && (
            <Button variant="outline" size="sm" onClick={() => setConfirm(true)}>
              <Unlink className="h-3.5 w-3.5" />
              {t("cockpit.pair.unpairDrone", { peer })}
            </Button>
          )}
        </CardTitle>
      </CardHeader>
      <CardContent>
        {rf.isError ? (
          <ReadErrorCard message={t("cockpit.readFailed.wfbPair")} onRetry={() => void rf.refetch()} />
        ) : !known ? (
          <p className="text-xs text-muted-foreground">{t("common.loading")}</p>
        ) : paired ? (
          <div className="grid grid-cols-[130px_1fr] gap-y-1 text-sm">
            <span className="text-xs text-muted-foreground">{t("cockpit.pair.pairedDrone", { peer })}</span>
            <span className="font-mono truncate">{rf.data?.paired_with_device_id ?? t("cockpit.pair.linked")}</span>
            {rf.data?.fingerprint && (
              <>
                <span className="text-xs text-muted-foreground">{t("cockpit.pair.fingerprint")}</span>
                <span className="font-mono truncate">{rf.data.fingerprint}</span>
              </>
            )}
            <span className="text-xs text-muted-foreground">{t("cockpit.pair.autoPair")}</span>
            <span>
              {rf.data?.auto_pair_enabled == null
                ? "—"
                : rf.data.auto_pair_enabled
                  ? t("cockpit.pair.on")
                  : t("cockpit.pair.off")}
            </span>
          </div>
        ) : (
          <Badge variant="neutral">{t("cockpit.pair.noDrone", { peer })}</Badge>
        )}
      </CardContent>
      <ConfirmDialog
        open={confirm}
        onOpenChange={setConfirm}
        title={t("cockpit.pair.unpairDrone", { peer })}
        description={
          <>
            <p>{t("cockpit.pair.rfUnpairNote", { peer })}</p>
            <p className="mt-2 text-warn">{t("pairing.confirm.flightWarning")}</p>
          </>
        }
        confirmLabel={t("cockpit.pair.unpairDrone", { peer })}
        destructive
        onConfirm={unbind}
      />
      <ConfirmDialog
        open={armedPrompt !== null}
        onOpenChange={(open) => {
          if (!open) {
            armedPrompt?.resolve(false);
            setArmedPrompt(null);
          }
        }}
        title={t("settings.profile.armedTitle")}
        description={t("settings.shared.armedOverride")}
        confirmLabel={t("cockpit.pair.unpairDrone", { peer })}
        destructive
        onConfirm={() => armedPrompt?.resolve(true)}
      />
    </Card>
  );
}

// The radio uplink on a ground station's Link screen: whether this box
// transmits to the fleet or a phone holds the uplink, read from the status
// poll's `uplink` block. While a phone holds it the box only receives, so the
// screen says so and offers the take-back behind the guarded (tap, then hold)
// gesture. A missing block reads as unknown, never as transmitting.

import { useState } from "react";
import { useTranslation } from "react-i18next";
import { RadioTower } from "lucide-react";

import { Dot, Row, SectionHeader } from "@/components/ui/data";
import { GuardedButton } from "@/components/ui/guarded-button";
import { useNow } from "@/hooks/use-now";
import { takeBackUplink, uplinkView } from "@/shared/api/uplink-handover";
import { toast, toastFromError } from "@/shared/api/toast";
import { fmtRelativeTime } from "@/shared/format";
import "@/shared/i18n";
import { useStatusStore } from "@/stores/status-store";

export function UplinkHandoverSection() {
  const { t } = useTranslation();
  const view = uplinkView(useStatusStore((s) => s.status?.uplink));
  const [busy, setBusy] = useState(false);
  // "Last heard" ages with the clock between polls.
  useNow(1000);

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
    setBusy(true);
    try {
      await takeBackUplink();
      toast.ok(t("wfb.uplink.takenBack"));
    } catch (err) {
      toastFromError(err, t("wfb.uplink.takeBackFailed"));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <SectionHeader>{t("wfb.uplink.title")}</SectionHeader>
      <Row
        label={line}
        left={<Dot tone={view === null ? "muted" : view.state === "standby" ? "warn" : "ok"} />}
        hint={
          view === null
            ? t("wfb.uplink.unknownHint")
            : view.state === "standby"
              ? t("wfb.uplink.standbyHint", { holder })
              : undefined
        }
        tone={view?.state === "standby" ? "warn" : undefined}
        mono={false}
      />
      {view?.state === "standby" ? (
        <>
          <Row
            label={t("wfb.uplink.lastHeard")}
            value={view.lastBeaconMs === null ? t("wfb.uplink.notHeard") : fmtRelativeTime(view.lastBeaconMs)}
          />
          <GuardedButton
            label={t("wfb.uplink.takeBack")}
            guardLabel={t("wfb.uplink.holdToTakeBack")}
            icon={RadioTower}
            disabled={busy}
            onFired={() => void takeBack()}
          />
        </>
      ) : null}
    </>
  );
}

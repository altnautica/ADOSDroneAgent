// IO — the ground station's controllers, HDMI display and Bluetooth. Making a
// controller primary moves RC input to it, so it fires from a guarded button;
// Bluetooth discovery and pairing are plain actions.

import { getArmedReading, radioWriteBlock } from "@/lib/radio-gate";
import { useCallback, useState } from "react";
import { useTranslation } from "react-i18next";
import { Bluetooth, Gamepad2, Monitor, RefreshCw } from "lucide-react";

import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { ActionButton, EmptyNote, ErrorNote, Row, StaleBadge } from "@/components/ui/data";
import { readFailed } from "@/lib/read-failed";
import { GuardedButton } from "@/components/ui/guarded-button";
import { ListItem } from "@/components/ui/list-item";
import {
  BT_SCAN_SECONDS,
  canMakePrimary,
  forgetBt,
  getDisplay,
  getGamepads,
  getPairedBt,
  pairBt,
  scanBt,
  setPrimaryGamepad,
  unpairedFound,
  type BtDevice,
} from "@/lib/io-model";
import { toast, toastFromError } from "@/shared/api/toast";
import { DASH } from "@/shared/format";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";

export function IoScreen({ initialTab = 0 }: { initialTab?: number } = {}) {
  const { t } = useTranslation();
  const [tab, setTab] = useState(initialTab);
  const [busy, setBusy] = useState(false);
  const [scanning, setScanning] = useState(false);
  const [found, setFound] = useState<BtDevice[] | null>(null);

  const gamepads = useResource(useCallback((s: AbortSignal) => getGamepads(s), []), 5_000, { enabled: tab === 0 });
  const display = useResource(useCallback((s: AbortSignal) => getDisplay(s), []), 10_000, { enabled: tab === 1 });
  const paired = useResource(useCallback((s: AbortSignal) => getPairedBt(s), []), 15_000, { enabled: tab === 2 });
  const armed = useResource(useCallback((s: AbortSignal) => getArmedReading(s), []), 1_000, { enabled: tab === 0 || tab === 2 });
  // Making a pad primary, or forgetting a paired controller, moves or drops
  // RC input: never while armed or unknown.
  const armedBlock = radioWriteBlock(armed.stale ? null : armed.data);

  const forget = (mac: string) => {
    setBusy(true);
    forgetBt(mac)
      .then(() => {
        toast.ok(t("cockpit.io.forgotten"));
        paired.refresh();
      })
      .catch((err: unknown) => toastFromError(err, t("cockpit.io.forgetFailed")))
      .finally(() => setBusy(false));
  };

  const makePrimary = (deviceId: string) => {
    setBusy(true);
    setPrimaryGamepad(deviceId)
      .then(() => {
        toast.ok(t("cockpit.io.primarySet"));
        gamepads.refresh();
      })
      .catch((err: unknown) => toastFromError(err, t("io.gamepads.setPrimaryFailed")))
      .finally(() => setBusy(false));
  };

  const scan = () => {
    setScanning(true);
    scanBt()
      .then(setFound)
      .catch((err: unknown) => toastFromError(err, t("io.bluetooth.scanFailed")))
      .finally(() => setScanning(false));
  };

  const pair = (mac: string) => {
    setBusy(true);
    pairBt(mac)
      .then(({ outcome, error }) => {
        if (outcome === "connected") toast.ok(t("io.bluetooth.pairedConnected"));
        else if (outcome === "pairedNotConnected") toast.err(t("io.bluetooth.pairedNotConnected"), error ?? undefined);
        else toast.err(t("io.bluetooth.pairingFailed"), error ?? undefined);
        paired.refresh();
      })
      .catch((err: unknown) => toastFromError(err, t("io.bluetooth.pairingFailed")))
      .finally(() => setBusy(false));
  };

  let body;
  if (tab === 0) {
    const pads = gamepads.data;
    body =
      !gamepads.ready ? (
        <EmptyNote>{t("common.loading")}</EmptyNote>
      ) : !pads ? (
        <EmptyNote>{t("io.gamepads.readFailed")}</EmptyNote>
      ) : pads.devices.length === 0 ? (
        <EmptyNote>{t("io.gamepads.empty")}</EmptyNote>
      ) : (
        <div className="flex flex-col gap-[0.3rem]">
          {armedBlock && pads.devices.length > 1 ? (
            <p className="text-[0.8rem] text-warn">{t(`cockpit.radio.${armedBlock}`)}</p>
          ) : null}
          {pads.devices.map((pad) => (
            <ListItem
              key={pad.deviceId}
              icon={Gamepad2}
              headline={pad.name ?? t("io.gamepads.unnamed")}
              supporting={pad.path ?? pad.deviceId}
              trailing={
                pad.deviceId === pads.primaryId ? (
                  <span className="text-[0.8rem] font-medium text-ok">{t("io.gamepads.primary")}</span>
                ) : (
                  <GuardedButton
                    label={t("io.gamepads.makePrimary")}
                    guardLabel={t("cockpit.io.primaryGuard")}
                    disabled={!canMakePrimary(pads, pad.deviceId, busy, armedBlock)}
                    onFired={() => makePrimary(pad.deviceId)}
                  />
                )
              }
            />
          ))}
        </div>
      );
  } else if (tab === 1) {
    const d = display.data;
    body = !display.ready ? (
      <EmptyNote>{t("io.display.loading")}</EmptyNote>
    ) : !d ? (
      <EmptyNote>{t("io.display.readFailed")}</EmptyNote>
    ) : (
      <div className="flex flex-col gap-[0.3rem]">
        <Row left={<Monitor className="h-[1rem] w-[1rem]" aria-hidden />} label={t("io.display.kiosk")} value={d.kioskEnabled ? t("io.display.on") : t("io.display.off")} />
        <Row label={t("io.display.resolution")} value={d.resolution ?? DASH} />
        <Row label={t("io.display.target")} value={d.kioskTargetUrl ?? t("io.display.default")} />
      </div>
    );
  } else {
    const list = paired.data ?? [];
    const nearby = found ? unpairedFound(found, list) : null;
    body = (
      <div className="flex flex-col gap-[0.3rem]">
        <ActionButton
          label={scanning ? t("io.bluetooth.scanning", { seconds: BT_SCAN_SECONDS }) : t("io.bluetooth.scan")}
          icon={Bluetooth}
          onClick={scan}
          busy={scanning}
        />
        <h2 className="pt-[0.4rem] text-[0.75rem] font-medium uppercase tracking-wider text-muted-foreground">
          {t("io.bluetooth.pairedHeading")}
        </h2>
        {readFailed(paired) ? (
          <ErrorNote onRetry={paired.refresh}>{t("cockpit.readFailed.btPaired")}</ErrorNote>
        ) : list.length === 0 ? (
          <EmptyNote>{t("io.bluetooth.noPaired")}</EmptyNote>
        ) : (
          <>
            {armedBlock ? <p className="text-[0.8rem] text-warn">{t(`cockpit.radio.${armedBlock}`)}</p> : null}
            {list.map((d) => (
              <ListItem
                key={d.mac}
                headline={d.name ?? d.mac}
                supporting={[d.mac, d.type].filter(Boolean).join(" · ")}
                trailing={
                  <span className="flex items-center gap-[0.4rem]">
                    <span className={d.connected ? "text-[0.8rem] text-ok" : "text-[0.8rem] text-muted-foreground"}>
                      {d.connected ? t("io.bluetooth.connected") : t("io.bluetooth.paired")}
                    </span>
                    <GuardedButton
                      label={t("cockpit.io.forget")}
                      guardLabel={t("cockpit.io.forgetGuard")}
                      disabled={busy || armedBlock !== null}
                      onFired={() => forget(d.mac)}
                    />
                  </span>
                }
              />
            ))}
          </>
        )}
        {nearby ? (
          <>
            <h2 className="pt-[0.4rem] text-[0.75rem] font-medium uppercase tracking-wider text-muted-foreground">
              {t("io.bluetooth.foundNearby")}
            </h2>
            {nearby.length === 0 ? (
              <EmptyNote>{t("io.bluetooth.noUnpaired")}</EmptyNote>
            ) : (
              nearby.map((d) => (
                <ListItem
                  key={d.mac}
                  headline={d.name ?? d.mac}
                  supporting={d.mac}
                  trailing={<ActionButton label={t("io.bluetooth.pair")} onClick={() => pair(d.mac)} disabled={busy} />}
                />
              ))
            )}
          </>
        ) : null}
      </div>
    );
  }

  return (
    <ConsoleScaffold stale={gamepads.stale || display.stale || paired.stale}
      title={t("io.route.title")}
      tabs={[t("io.gamepads.title"), t("io.display.title"), t("io.bluetooth.title")]}
      selectedTab={tab}
      onSelectTab={setTab}
      actions={
        <div className="flex items-center gap-[0.4rem]">
          <StaleBadge stale={gamepads.stale || display.stale || paired.stale} />
          <ActionButton
            label={t("io.route.refresh")}
            icon={RefreshCw}
            onClick={() => {
              gamepads.refresh();
              display.refresh();
              paired.refresh();
            }}
          />
        </div>
      }
    >
      {body}
    </ConsoleScaffold>
  );
}

// Transmit — the drone's WFB transmitter: state, interface, channel, power,
// MCS, regulatory domain, bitrate. TX power is writable inside the region
// clamp the radio reports (`tx_power_max_dbm`); the write changes the radio, so
// it goes out through a slide and is held while the vehicle is armed.

import { useCallback, useState } from "react";
import { useTranslation } from "react-i18next";

import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { EmptyNote, Row, SectionHeader, StaleBadge, Tile, TileGrid } from "@/components/ui/data";
import { NumberSettingRow } from "@/components/ui/number-setting-row";
import { SlideToConfirm } from "@/components/ui/slide-to-confirm";
import { useDirtyGuard } from "@/hooks/use-dirty-guard";
import { armedRefusalText, getArmedReading, radioWriteBlock } from "@/lib/radio-gate";
import { txAdapterMissing, txPowerBlock, txPowerBounds } from "@/lib/transmit-model";
import { toast, toastFromError } from "@/shared/api/toast";
import { getWfbStatus, setWfbTxPower } from "@/shared/api/wfb";
import { DASH, fmtBitrate, fmtInt, fmtNum } from "@/shared/format";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";

const WFB_POLL_MS = 2_000;
const ARMED_POLL_MS = 1_000;

export function TransmitScreen() {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  const [draft, setDraft] = useState<number | null>(null);

  const wfb = useResource(useCallback(() => getWfbStatus(), []), WFB_POLL_MS);
  const armed = useResource(useCallback((s: AbortSignal) => getArmedReading(s), []), ARMED_POLL_MS);

  const w = wfb.data;
  const bounds = txPowerBounds(w);
  const current = typeof w?.tx_power_dbm === "number" ? w.tx_power_dbm : null;
  const value = draft ?? current ?? bounds?.min ?? 1;
  useDirtyGuard(draft !== null && draft !== current, "transmit-power");
  const block = txPowerBlock(value, current, bounds, radioWriteBlock(armed.stale ? null : armed.data));

  const apply = () => {
    if (block) return;
    setBusy(true);
    setWfbTxPower(value)
      .then((res) => {
        if (res.persisted === false) {
          toast.info(t("cockpit.transmit.notPersisted", { dbm: res.effective_dbm }), res.persist_error);
        } else {
          toast.ok(t("cockpit.transmit.applied", { dbm: res.effective_dbm }));
        }
        setDraft(null);
        wfb.refresh();
      })
      .catch((err: unknown) => {
        const refusal = armedRefusalText(err);
        if (refusal == null) return toastFromError(err, t("cockpit.transmit.failed"));
        toast.err(t("cockpit.radio.refused"), refusal);
        armed.refresh();
      })
      .finally(() => setBusy(false));
  };

  let body;
  if (!wfb.ready) {
    body = <EmptyNote>{t("common.loading")}</EmptyNote>;
  } else if (!w) {
    body = <EmptyNote>{t("cockpit.transmit.unavailable")}</EmptyNote>;
  } else if (txAdapterMissing(w)) {
    body = <EmptyNote>{t("cockpit.transmit.notRunning")}</EmptyNote>;
  } else {
    body = (
      <div className="flex flex-col gap-[0.5rem]">
        <TileGrid>
          <Tile label={t("cockpit.transmit.state")} value={w.state || DASH} />
          <Tile
            label={t("wfb.fields.txPower")}
            value={current != null ? t("cockpit.video.dbm", { dbm: fmtNum(current, 0) }) : DASH}
            hint={bounds ? t("cockpit.transmit.ceiling", { max: bounds.max }) : undefined}
          />
          <Tile label={t("wfb.fields.bitrate")} value={fmtBitrate(w.bitrate_kbps)} />
        </TileGrid>
        <SectionHeader>{t("wfb.tx.title")}</SectionHeader>
        <Row label={t("wfb.fields.interface")} value={w.interface || DASH} />
        <Row
          label={t("wfb.fields.channel")}
          value={
            w.actual_channel != null
              ? w.frequency_mhz
                ? t("cockpit.transmit.channelFreq", { ch: w.actual_channel, mhz: fmtNum(w.frequency_mhz, 0) })
                : String(w.actual_channel)
              : DASH
          }
        />
        {w.bandwidth_mhz ? (
          <Row label={t("wfb.fields.bandwidth")} value={t("cockpit.transmit.mhz", { mhz: fmtNum(w.bandwidth_mhz, 0) })} />
        ) : null}
        <Row label={t("wfb.fields.mcs")} value={w.mcs_index ?? DASH} />
        <Row label={t("wfb.fields.domain")} value={w.regulatory_domain || DASH} />
        <Row label={t("wfb.fields.packets")} value={fmtInt(w.packets_received)} />
        <Row label={t("wfb.fields.restarts")} value={fmtInt(w.restart_count)} />
        <SectionHeader>{t("cockpit.transmit.powerHeader")}</SectionHeader>
        {bounds ? (
          <>
            <NumberSettingRow
              title={t("wfb.fields.txPower")}
              summary={t("cockpit.transmit.range", { min: bounds.min, max: bounds.max })}
              value={value}
              min={bounds.min}
              max={bounds.max}
              step={1}
              format={(v) => t("cockpit.video.dbm", { dbm: v })}
              onChange={setDraft}
              enabled={!busy}
            />
            <div className="text-[0.8rem] text-muted-foreground">
              {block ? t(`cockpit.transmit.block.${block}`) : t("cockpit.transmit.ready", { dbm: value })}
            </div>
            <SlideToConfirm
              label={t("cockpit.transmit.slide", { dbm: value })}
              onConfirmed={apply}
              disabled={busy || block !== null}
            />
          </>
        ) : (
          <EmptyNote>{t("cockpit.transmit.block.noRadio")}</EmptyNote>
        )}
      </div>
    );
  }

  return (
    <ConsoleScaffold stale={wfb.stale} title={t("cockpit.transmit.title")} actions={<StaleBadge stale={wfb.stale} />}>
      {body}
    </ConsoleScaffold>
  );
}

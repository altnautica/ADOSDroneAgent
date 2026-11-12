// LoRa — the lane's region plan, TX governor (duty cycle, dwell, listen-before-
// talk, power), refusal counts, mismatch chips, nearby beacons, and the region
// picker. A region change switches the plan the lane transmits under, so it is
// applied with a slide and held while the vehicle is armed.

import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { TFunction } from "i18next";

import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { EmptyNote, ErrorNote, ProgressBar, Row, SectionHeader, StaleBadge } from "@/components/ui/data";
import { SelectSettingRow } from "@/components/ui/setting-row";
import { SlideToConfirm } from "@/components/ui/slide-to-confirm";
import { FOLLOW_COUNTRY, regionApplyBlock, regionBody } from "@/lib/lora-model";
import { getArmedReading, isArmedRefusal, radioWriteBlock } from "@/lib/radio-gate";
import type { Tone } from "@/lib/tone";
import type { AgentConfig } from "@/shared/api/types";
import { apiFetch } from "@/shared/api-fetch";
import {
  CRSF_STATUS_PATH,
  LORA_BEACONS_PATH,
  LORA_PATH,
  LORA_REGIONS_PATH,
  beaconAgeS,
  dutyFraction,
  formatCountdown,
  loraChips,
  loraTxState,
  meterTone,
  putLoraRegion,
  type CrsfModule,
  type LoraBeacon,
  type LoraChip,
  type LoraRegionRow,
  type LoraStatus,
  type MeterTone,
  type LoraRule,
} from "@/shared/api/lora";
import { regionFromConfig } from "@/shared/api/region";
import { toast, toastFromError } from "@/shared/api/toast";
import "@/shared/i18n";
import { useProfile } from "@/shared/use-profile";
import { useResource } from "@/shared/use-resource";

const METER_TONE: Record<MeterTone, Tone> = { neutral: "ok", warn: "warn", error: "err" };

function chipText(chip: LoraChip, t: TFunction): string {
  if (chip.kind === "module_band") return t("lora.chip.moduleBand", { module: chip.module, plan: chip.plan });
  if (chip.kind === "meshtastic") return t("lora.chip.meshtastic", { node: chip.node, expected: chip.expected });
  if (chip.kind === "airborne") return t("lora.chip.airborne", { code: chip.code });
  if (chip.kind === "elrs") {
    return t("lora.chip.elrs", { module: chip.module, domain: chip.domain, code: chip.code, expected: chip.expected });
  }
  return t("lora.chip.country", { lora: chip.lora, country: chip.country });
}

function TxLine({ status, elapsedMs }: { status: LoraStatus; elapsedMs: number }) {
  const { t } = useTranslation();
  const tx = loraTxState(status);
  if (tx.kind === "allowed") {
    const wait = tx.waitMs - elapsedMs;
    return (
      <Row
        label={t("lora.txAllowed")}
        tone="ok"
        value={wait > 0 ? t("lora.nextTxIn", { time: formatCountdown(wait) }) : t("lora.nextTxNow")}
      />
    );
  }
  if (tx.kind === "paused") {
    return (
      <Row
        label={t("lora.txPaused", { time: formatCountdown(tx.waitMs - elapsedMs), reason: t(`lora.reasonShort.${tx.reason}`) })}
        tone="warn"
      />
    );
  }
  if (tx.kind === "busy") return <Row label={t("lora.txBusy")} tone="warn" />;
  return (
    <Row
      label={t("lora.txBlocked", { reason: t(`lora.blocked.${tx.reason}`, { plan: status.region?.plan_id ?? "--" }) })}
      tone="err"
    />
  );
}

function RuleRow({ rule }: { rule: LoraRule }) {
  const { t } = useTranslation();
  if (rule.kind === "duty_cycle") {
    const fraction = dutyFraction(rule);
    const tone = METER_TONE[rule.exhausted ? "error" : meterTone(fraction)];
    return (
      <div className="flex flex-col gap-[0.2rem] px-[0.6rem]">
        <div className="flex justify-between text-[0.8rem]">
          <span className="text-muted-foreground">{t("lora.rule.duty", { band: rule.sub_band })}</span>
          <span className="font-mono">
            {t("lora.rule.dutyValue", {
              used: (rule.airtime_ms_window / 1000).toFixed(1),
              budget: (rule.budget_ms_window / 1000).toFixed(0),
            })}
          </span>
        </div>
        <ProgressBar value={fraction} max={1} tone={tone} />
      </div>
    );
  }
  const value =
    rule.kind === "dwell_time"
      ? t("lora.rule.dwellValue", { last: rule.last_tx_ms, limit: rule.limit_ms })
      : rule.kind === "lbt"
        ? t("lora.rule.lbtValue", { rssi: rule.last_rssi_dbm, threshold: rule.threshold_dbm, busy: rule.busy_count })
        : t("lora.rule.powerValue", { configured: rule.configured_mw, limit: rule.limit_mw });
  return <Row label={t(`lora.rule.${rule.kind}`)} value={value} />;
}

export function LoraScreen() {
  const { t } = useTranslation();
  const profile = useProfile();
  const [busy, setBusy] = useState(false);

  const lora = useResource(
    useCallback(
      (s: AbortSignal) => apiFetch<LoraStatus>(LORA_PATH, { signal: s }).then((status) => ({ status, at: Date.now() })),
      [],
    ),
    2_000,
  );
  const regions = useResource(
    useCallback((s: AbortSignal) => apiFetch<{ regions?: LoraRegionRow[] }>(LORA_REGIONS_PATH, { signal: s }), []),
    60_000,
  );
  const beacons = useResource(
    useCallback((s: AbortSignal) => apiFetch<{ beacons?: LoraBeacon[] }>(LORA_BEACONS_PATH, { signal: s }), []),
    5_000,
  );
  // The RC lane's modules exist only on a ground station; a drone has no route.
  const crsf = useResource(
    useCallback((s: AbortSignal) => apiFetch<{ modules?: CrsfModule[] }>(CRSF_STATUS_PATH, { signal: s }), []),
    5_000,
    { enabled: profile === "ground_station" },
  );
  const config = useResource(
    useCallback((s: AbortSignal) => apiFetch<AgentConfig>("/api/config", { signal: s }), []),
    30_000,
  );
  const armed = useResource(useCallback((s: AbortSignal) => getArmedReading(s), []), 1_000);

  // The governor's countdowns run down from the moment the status arrived.
  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    const id = window.setInterval(() => setNowMs(Date.now()), 1_000);
    return () => window.clearInterval(id);
  }, []);

  const status = lora.data?.status ?? null;
  const fetchedAt = lora.data?.at ?? nowMs;
  const current = status?.region?.code ?? FOLLOW_COUNTRY;
  // The operator's pick; null shows the live region.
  const [choiceEdit, setChoice] = useState<string | null>(null);
  const choice = choiceEdit ?? current;

  const regionRows = regions.data?.regions ?? [];
  const country = regionFromConfig(config.data?.network?.regulatory);
  const block = regionApplyBlock(choice, current, radioWriteBlock(armed.stale ? null : armed.data));

  const apply = () => {
    if (block) return;
    setBusy(true);
    putLoraRegion(regionBody(choice))
      .then(() => {
        toast.ok(t("lora.regionSaved"));
        setChoice(null);
        lora.refresh();
      })
      .catch((err: unknown) => {
        if (isArmedRefusal(err)) toast.err(t("lora.armedLocked"));
        else toastFromError(err, t("settings.shared.applyFailed"));
      })
      .finally(() => setBusy(false));
  };

  let body;
  if (!lora.ready) {
    body = <EmptyNote>{t("common.loading")}</EmptyNote>;
  } else if (!status) {
    body = <ErrorNote onRetry={lora.refresh}>{t("lora.loadFailed")}</ErrorNote>;
  } else if (!status.present) {
    body = <EmptyNote>{t("lora.absent")}</EmptyNote>;
  } else {
    const row = regionRows.find((r) => r.code === status.region?.code) ?? null;
    const chips = loraChips(status, country, row, crsf.data?.modules ?? []);
    const refused = status.tx?.refused_by_rule ?? {};
    const list = beacons.data?.beacons ?? [];
    body = (
      <div className="flex flex-col gap-[0.4rem]">
        <Row
          label={status.region?.plan_id ?? "--"}
          hint={status.region ? t("lora.source", { source: status.region.source }) : t("lora.regionUnset")}
          value={t("lora.module", { transport: status.transport ?? "--", band: status.module_band ?? "--" })}
        />
        {status.channel ? <Row label={t("lora.channel", { freq: status.channel.freq_mhz.toFixed(3) })} /> : null}
        {!status.region ? <Row label={t("lora.regionUnsetBanner")} tone="warn" /> : null}
        {chips.map((c, i) => (
          <Row key={`${c.kind}-${i}`} label={chipText(c, t)} tone="warn" value={t("cockpit.lora.mismatch")} />
        ))}
        <TxLine status={status} elapsedMs={Math.max(0, nowMs - fetchedAt)} />
        {(status.governor?.rules ?? []).map((r, i) => (
          <RuleRow key={`${r.kind}-${i}`} rule={r} />
        ))}
        <div className="px-[0.6rem] font-mono text-[0.75rem] text-muted-foreground">
          {t("lora.refused", {
            duty: refused.duty_cycle ?? 0,
            dwell: refused.dwell_time ?? 0,
            lbt: refused.lbt ?? 0,
            power: refused.max_power ?? 0,
            region: refused.region_unset ?? 0,
          })}
        </div>
        <SectionHeader>{t("lora.regionPicker")}</SectionHeader>
        <SelectSettingRow
          title={t("lora.regionPicker")}
          options={[FOLLOW_COUNTRY, ...regionRows.map((r) => r.code)]}
          value={choice}
          optionLabel={(code) => {
            if (code === FOLLOW_COUNTRY) return t("lora.followCountry", { country: country ?? "--" });
            const r = regionRows.find((x) => x.code === code);
            return r
              ? t("lora.regionOption", {
                  code: r.code,
                  plan: r.plan_id,
                  rules: r.rules.length ? r.rules.join(",") : t("lora.notVerified"),
                  module: r.supported_by_module ? "" : t("lora.moduleCannot"),
                })
              : code;
          }}
          onSelect={setChoice}
          enabled={!busy}
        />
        <div className="px-[0.6rem] text-[0.8rem] text-muted-foreground">
          {block === "unchanged"
            ? t("lora.adminNote")
            : block
              ? t(`cockpit.radio.${block}`)
              : t("lora.confirmBody", { from: current || "--", to: choice || t("lora.followShort") })}
        </div>
        <SlideToConfirm label={t("lora.applyRegion")} onConfirmed={apply} disabled={busy || block !== null} />
        <SectionHeader>{t("lora.beacons")}</SectionHeader>
        {list.length === 0 ? (
          <EmptyNote>{t("lora.noBeacons")}</EmptyNote>
        ) : (
          list.slice(0, 16).map((b) => (
            <Row
              key={b.node_id}
              label={t("lora.beacon", { id: b.node_id, age: beaconAgeS(b, nowMs), rssi: b.rssi_dbm, snr: b.snr_db.toFixed(1) })}
              value={b.battery_pct != null ? t("cockpit.lora.battery", { pct: b.battery_pct }) : undefined}
            />
          ))
        )}
      </div>
    );
  }

  return (
    <ConsoleScaffold stale={lora.stale} title={t("lora.title")} actions={<StaleBadge stale={lora.stale} />}>
      {body}
    </ConsoleScaffold>
  );
}

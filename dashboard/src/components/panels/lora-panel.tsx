import { useQuery, useQueryClient } from "@tanstack/react-query";
import type { TFunction } from "i18next";
import { RadioTower } from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Link } from "react-router-dom";

import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { resourceQueryOptions, useResource } from "@/hooks/use-resource";
import { useNodeProfile } from "@/hooks/use-node-profile";
import { useSnapshot } from "@/hooks/use-snapshot";
import { useConfig } from "@/hooks/use-config";
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
  type LoraRule,
  type LoraStatus,
} from "@/lib/lora";
import { regionFromConfig } from "@/lib/region";
import { toast, toastFromError } from "@/lib/toast";
import { ApiError } from "@/shared/api-fetch";

const FOLLOW_COUNTRY = "";

const METER_FILL = { neutral: "bg-primary", warn: "bg-warn", error: "bg-destructive" } as const;

function TxLine({ status, elapsedMs, t }: { status: LoraStatus; elapsedMs: number; t: TFunction }) {
  const tx = loraTxState(status);
  if (tx.kind === "allowed") {
    const wait = tx.waitMs - elapsedMs;
    return (
      <div className="flex items-center justify-between">
        <span className="font-mono text-ok">{t("lora.txAllowed")}</span>
        <span className="text-xs text-muted-foreground">
          {wait > 0 ? t("lora.nextTxIn", { time: formatCountdown(wait) }) : t("lora.nextTxNow")}
        </span>
      </div>
    );
  }
  if (tx.kind === "paused") {
    return (
      <div className="font-mono text-warn">
        {t("lora.txPaused", {
          time: formatCountdown(tx.waitMs - elapsedMs),
          reason: t(`lora.reasonShort.${tx.reason}`),
        })}
      </div>
    );
  }
  if (tx.kind === "busy") return <div className="font-mono text-warn">{t("lora.txBusy")}</div>;
  return (
    <div className="font-mono text-destructive">
      {t("lora.txBlocked", {
        reason: t(`lora.blocked.${tx.reason}`, { plan: status.region?.plan_id ?? "--" }),
      })}
    </div>
  );
}

function RuleRow({ rule, t }: { rule: LoraRule; t: TFunction }) {
  const label = "w-28 shrink-0 text-xs text-muted-foreground";
  if (rule.kind === "duty_cycle") {
    const fraction = dutyFraction(rule);
    const pct = Math.round(fraction * 100);
    const tone = rule.exhausted ? "error" : meterTone(fraction);
    return (
      <div className="flex items-center gap-3">
        <span className={label}>{t("lora.rule.duty", { band: rule.sub_band })}</span>
        <div className="h-2 flex-1 rounded bg-muted" role="meter" aria-valuenow={pct}>
          <div className={`h-2 rounded ${METER_FILL[tone]}`} data-tone={tone} style={{ width: `${pct}%` }} />
        </div>
        <span className="font-mono text-xs">
          {t("lora.rule.dutyValue", {
            used: (rule.airtime_ms_window / 1000).toFixed(1),
            budget: (rule.budget_ms_window / 1000).toFixed(0),
          })}
        </span>
      </div>
    );
  }
  const value =
    rule.kind === "dwell_time"
      ? t("lora.rule.dwellValue", { last: rule.last_tx_ms, limit: rule.limit_ms })
      : rule.kind === "lbt"
        ? t("lora.rule.lbtValue", { rssi: rule.last_rssi_dbm, threshold: rule.threshold_dbm, busy: rule.busy_count })
        : t("lora.rule.powerValue", { configured: rule.configured_mw, limit: rule.limit_mw });
  return (
    <div className="flex items-center gap-3">
      <span className={label}>{t(`lora.rule.${rule.kind}`)}</span>
      <span className="font-mono text-xs">{value}</span>
    </div>
  );
}

function chipText(chip: LoraChip, t: TFunction): string {
  if (chip.kind === "module_band") return t("lora.chip.moduleBand", { module: chip.module, plan: chip.plan });
  if (chip.kind === "meshtastic") return t("lora.chip.meshtastic", { node: chip.node, expected: chip.expected });
  if (chip.kind === "airborne") return t("lora.chip.airborne", { code: chip.code });
  if (chip.kind === "elrs") {
    return t("lora.chip.elrs", { module: chip.module, domain: chip.domain, code: chip.code, expected: chip.expected });
  }
  return t("lora.chip.country", { lora: chip.lora, country: chip.country });
}

export interface LoraViewProps {
  status: LoraStatus;
  regions: LoraRegionRow[];
  beacons: LoraBeacon[];
  country: string | null;
  nowMs: number;
  /** When `status` was fetched; the countdowns run down from it. */
  fetchedAtMs: number;
  /** RC modules reported by the CRSF lane; empty where none runs. */
  modules?: readonly CrsfModule[];
  /** The vehicle reports armed: region changes are locked. */
  armed?: boolean;
  onApplyRegion?: (region: string | null) => Promise<void>;
}

/** The compact `/lora` form: region line, TX line, one meter per governor
 *  rule, refusals, region picker, beacons and mismatch chips. */
export function LoraView({
  status,
  regions,
  beacons,
  country,
  nowMs,
  fetchedAtMs,
  modules = [],
  armed = false,
  onApplyRegion,
}: LoraViewProps) {
  const { t } = useTranslation();
  const current = status.region?.code ?? FOLLOW_COUNTRY;
  const [choice, setChoice] = useState(current);
  const [confirmOpen, setConfirmOpen] = useState(false);
  useEffect(() => setChoice(current), [current]);

  if (!status.present) {
    return <p className="text-sm text-muted-foreground">{t("lora.absent")}</p>;
  }
  const elapsedMs = Math.max(0, nowMs - fetchedAtMs);
  const refused = status.tx?.refused_by_rule ?? {};
  const row = regions.find((r) => r.code === status.region?.code) ?? null;
  const chips = loraChips(status, country, row, modules);

  return (
    <div className="space-y-3 text-sm">
      <div className="flex flex-wrap items-center gap-x-4 gap-y-1">
        <span className="font-mono">{status.region?.plan_id ?? "--"}</span>
        <span className="text-xs text-muted-foreground">
          {status.region ? t("lora.source", { source: status.region.source }) : t("lora.regionUnset")}
        </span>
        <span className="font-mono text-xs">
          {t("lora.module", { transport: status.transport ?? "—", band: status.module_band ?? "—" })}
        </span>
        {status.channel && (
          <span className="font-mono text-xs">{t("lora.channel", { freq: status.channel.freq_mhz.toFixed(3) })}</span>
        )}
      </div>
      {!status.region && (
        <div className="flex flex-wrap items-center gap-2 text-xs">
          <span className="text-warn">{t("lora.regionUnsetBanner")}</span>
          <Link to="/settings/region" className="text-primary underline">
            {t("lora.openCountry")}
          </Link>
        </div>
      )}
      {chips.map((c, i) => (
        <Badge key={`${c.kind}-${i}`} variant="warn" className="mr-2">
          {chipText(c, t)}
        </Badge>
      ))}
      <TxLine status={status} elapsedMs={elapsedMs} t={t} />
      <div className="space-y-1.5">
        {(status.governor?.rules ?? []).map((r, i) => (
          <RuleRow key={`${r.kind}-${i}`} rule={r} t={t} />
        ))}
      </div>
      <div className="font-mono text-xs text-muted-foreground">
        {t("lora.refused", {
          duty: refused.duty_cycle ?? 0,
          dwell: refused.dwell_time ?? 0,
          lbt: refused.lbt ?? 0,
          power: refused.max_power ?? 0,
          region: refused.region_unset ?? 0,
        })}
      </div>
      {onApplyRegion && (
        <div className="flex flex-wrap items-center gap-2">
          <select
            aria-label={t("lora.regionPicker")}
            className="h-8 rounded-md border border-input bg-background px-2 text-sm"
            value={choice}
            onChange={(e) => setChoice(e.target.value)}
          >
            <option value={FOLLOW_COUNTRY}>{t("lora.followCountry", { country: country ?? "--" })}</option>
            {regions.map((r) => (
              <option key={r.code} value={r.code}>
                {t("lora.regionOption", {
                  code: r.code,
                  plan: r.plan_id,
                  rules: r.rules.length ? r.rules.join(",") : t("lora.notVerified"),
                  module: r.supported_by_module ? "" : t("lora.moduleCannot"),
                })}
              </option>
            ))}
          </select>
          <Button
            size="sm"
            variant="outline"
            disabled={choice === current || armed}
            onClick={() => setConfirmOpen(true)}
          >
            {t("lora.applyRegion")}
          </Button>
          <span className="text-xs text-muted-foreground">{armed ? t("lora.armedLocked") : t("lora.adminNote")}</span>
          <ConfirmDialog
            open={confirmOpen}
            onOpenChange={setConfirmOpen}
            title={t("lora.confirmTitle")}
            description={t("lora.confirmBody", { from: current || "--", to: choice || t("lora.followShort") })}
            onConfirm={() => onApplyRegion(choice === FOLLOW_COUNTRY ? null : choice)}
          />
        </div>
      )}
      <div className="text-xs">
        <span className="text-muted-foreground">{t("lora.beacons")} </span>
        {beacons.length === 0 ? (
          <span className="text-muted-foreground">{t("lora.noBeacons")}</span>
        ) : (
          <span className="font-mono">
            {beacons
              .slice(0, 8)
              .map((b) =>
                t("lora.beacon", {
                  id: b.node_id,
                  age: beaconAgeS(b, nowMs),
                  rssi: b.rssi_dbm,
                  snr: b.snr_db.toFixed(1),
                }),
              )
              .join(" · ")}
          </span>
        )}
      </div>
    </div>
  );
}

export function LoraPanel() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const lora = useResource<LoraStatus>("lora", LORA_PATH, 2_000);
  const regions = useResource<{ regions: LoraRegionRow[] }>("lora-regions", LORA_REGIONS_PATH);
  const beacons = useResource<{ beacons: LoraBeacon[] }>("lora-beacons", LORA_BEACONS_PATH, 5_000);
  const config = useConfig();
  const snapshot = useSnapshot();
  const profile = useNodeProfile();
  // The RC lane's modules exist only on a ground station; a drone has no route.
  const crsf = useQuery({
    ...resourceQueryOptions<{ modules?: CrsfModule[] }>("crsf", CRSF_STATUS_PATH, 5_000),
    enabled: profile === "ground_station",
    retry: false,
  });
  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    const id = window.setInterval(() => setNowMs(Date.now()), 1_000);
    return () => window.clearInterval(id);
  }, []);

  async function applyRegion(region: string | null) {
    try {
      await putLoraRegion(region);
      toast.ok(t("lora.regionSaved"));
      await qc.invalidateQueries({ queryKey: ["lora"] });
    } catch (err) {
      if (err instanceof ApiError && err.code === "E_ARMED") toast.err(t("lora.armedLocked"));
      else toastFromError(err, t("settings.shared.applyFailed"));
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <RadioTower className="h-3.5 w-3.5" />
          {t("lora.title")}
        </CardTitle>
      </CardHeader>
      <CardContent>
        {lora.data ? (
          <LoraView
            status={lora.data}
            regions={regions.data?.regions ?? []}
            beacons={beacons.data?.beacons ?? []}
            country={regionFromConfig(config.data?.network?.regulatory)}
            modules={crsf.data?.modules ?? []}
            armed={snapshot.data?.fc?.armed === true}
            nowMs={nowMs}
            fetchedAtMs={lora.dataUpdatedAt}
            onApplyRegion={applyRegion}
          />
        ) : (
          <p className="text-sm text-muted-foreground">
            {lora.isError ? t("lora.loadFailed") : t("common.loading")}
          </p>
        )}
      </CardContent>
    </Card>
  );
}

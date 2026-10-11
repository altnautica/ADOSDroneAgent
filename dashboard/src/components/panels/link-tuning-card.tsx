// Operator radio link-tuning card. Mirrors the Mission Control tuning panel
// on the on-device dashboard: preset / FEC ratio / MCS rate / adaptive toggle.
// Each control applies to the live data plane via POST /api/video/config and
// then refetches the config so the displayed values reflect reality.

import { useState } from "react";
import { useTranslation } from "react-i18next";

import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { setVideoConfig, type VideoConfigPatch } from "@/lib/wfb";

const FEC_RATIOS: Array<[number, number]> = [
  [8, 16],
  [8, 14],
  [8, 12],
  [8, 10],
  [4, 12],
];

const PRESETS = ["conservative", "balanced", "aggressive"] as const;

const PRESET_LABEL_KEYS = {
  conservative: "wfb.tuning.presetConservative",
  balanced: "wfb.tuning.presetBalanced",
  aggressive: "wfb.tuning.presetAggressive",
} as const satisfies Record<(typeof PRESETS)[number], string>;

interface LinkTuningCardProps {
  mcsIndex?: number;
  fecK?: number;
  fecN?: number;
  preset?: string;
  adaptiveEnabled?: boolean;
  onChanged: () => void;
}

const selectClass =
  "w-full rounded border border-border bg-muted/40 px-2 py-1.5 text-sm font-mono " +
  "focus:border-primary focus:outline-none disabled:opacity-50";

export function LinkTuningCard({
  mcsIndex,
  fecK,
  fecN,
  preset,
  adaptiveEnabled,
  onChanged,
}: LinkTuningCardProps) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);

  const apply = async (patch: VideoConfigPatch) => {
    setBusy(true);
    setNote(null);
    try {
      const res = await setVideoConfig(patch);
      const warnings = res.warnings ?? [];
      setNote(
        warnings.length
          ? t("wfb.tuning.appliedWithWarnings", { warnings: warnings.join(", ") })
          : t("wfb.tuning.applied"),
      );
      onChanged();
    } catch (e) {
      setNote(e instanceof Error ? e.message : t("wfb.tuning.applyFailed"));
    } finally {
      setBusy(false);
    }
  };

  const fecValue = fecK != null && fecN != null ? `${fecK}/${fecN}` : "";

  return (
    <Card>
      <CardHeader>
        <CardTitle>{t("wfb.tuning.title")}</CardTitle>
      </CardHeader>
      <CardContent>
        <div className="space-y-3 text-sm">
          <label className="block">
            <span className="text-xs text-muted-foreground">{t("wfb.tuning.preset")}</span>
            <select
              className={selectClass}
              value={PRESETS.includes(preset as (typeof PRESETS)[number]) ? preset : ""}
              disabled={busy}
              onChange={(e) => {
                const v = e.target.value as (typeof PRESETS)[number];
                if (v) void apply({ preset: v });
              }}
            >
              <option value="" disabled>
                {t("wfb.tuning.selectPreset")}
              </option>
              {PRESETS.map((p) => (
                <option key={p} value={p}>
                  {t(PRESET_LABEL_KEYS[p])}
                </option>
              ))}
            </select>
          </label>

          <label className="block">
            <span className="text-xs text-muted-foreground">{t("wfb.tuning.fec")}</span>
            <select
              className={selectClass}
              value={fecValue}
              disabled={busy}
              onChange={(e) => {
                const [k, n] = e.target.value.split("/").map(Number);
                if (Number.isFinite(k) && Number.isFinite(n)) {
                  void apply({ fec_k: k, fec_n: n });
                }
              }}
            >
              {!FEC_RATIOS.some(([k, n]) => `${k}/${n}` === fecValue) && (
                <option value={fecValue} disabled>
                  {fecValue || "—"}
                </option>
              )}
              {FEC_RATIOS.map(([k, n]) => (
                <option key={`${k}/${n}`} value={`${k}/${n}`}>
                  {t("wfb.tuning.fecOption", {
                    k,
                    n,
                    redundancy: Math.round(((n - k) / k) * 100),
                  })}
                </option>
              ))}
            </select>
          </label>

          <label className="block">
            <span className="text-xs text-muted-foreground">{t("wfb.tuning.mcsRate")}</span>
            <select
              className={selectClass}
              value={mcsIndex != null ? String(mcsIndex) : ""}
              disabled={busy}
              onChange={(e) => {
                const mcs = Number(e.target.value);
                if (Number.isFinite(mcs)) void apply({ mcs });
              }}
            >
              <option value="" disabled>
                —
              </option>
              {Array.from({ length: 8 }, (_, i) => (
                <option key={i} value={String(i)}>
                  {i}
                </option>
              ))}
            </select>
          </label>

          <div className="flex items-center justify-between gap-2 pt-1">
            <span className="text-xs text-muted-foreground">
              {t("wfb.tuning.adaptive")}
            </span>
            <Switch
              checked={adaptiveEnabled ?? false}
              disabled={busy}
              onCheckedChange={(checked) => void apply({ auto: checked })}
            />
          </div>

          {note && <p className="text-xs text-muted-foreground">{note}</p>}
        </div>
      </CardContent>
    </Card>
  );
}

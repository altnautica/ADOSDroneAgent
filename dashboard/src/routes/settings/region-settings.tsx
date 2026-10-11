import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { RiskBadge } from "@/components/settings/risk-badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { RadioCardGroup } from "@/components/ui/radio-card-group";
import { useDirtyGuard } from "@/hooks/use-dirty-guard";
import { useStatus } from "@/hooks/use-status";
import {
  COMMON_REGIONS,
  modeFromStatus,
  normalizeRegion,
  regionFromStatus,
  regionLabel,
} from "@/lib/region";
import { postRegion } from "@/lib/setup-actions";
import { toast, toastFromError } from "@/lib/toast";
import type { RegulatoryMode } from "@/lib/types";

const MODE_OPTIONS = [
  {
    value: "unrestricted" as RegulatoryMode,
    labelKey: "settings.region.modeUnrestrictedLabel",
    badgeKey: "settings.region.modeDefaultBadge",
    descriptionKey: "settings.region.modeUnrestrictedDescription",
  },
  {
    value: "region" as RegulatoryMode,
    labelKey: "settings.region.modeRegionLabel",
    badgeKey: undefined,
    descriptionKey: "settings.region.modeRegionDescription",
  },
] as const;

const OTHER = "__other__";

export function RegionSettings() {
  const { t } = useTranslation();
  const status = useStatus();
  const qc = useQueryClient();

  const initialMode = modeFromStatus(status.data?.regulatory);
  const initialRegion = regionFromStatus(status.data?.regulatory);

  const [mode, setMode] = useState<RegulatoryMode>("unrestricted");
  const [choice, setChoice] = useState<string>(COMMON_REGIONS[0]!.code);
  const [otherCode, setOtherCode] = useState("");
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [busy, setBusy] = useState(false);

  // Seed the form from the agent's values, and re-seed only when THEY change
  // (not on every status poll, which would discard an unsaved choice).
  useEffect(() => {
    setMode(initialMode);
    if (initialRegion) {
      if (COMMON_REGIONS.some((c) => c.code === initialRegion)) {
        setChoice(initialRegion);
      } else {
        setChoice(OTHER);
        setOtherCode(initialRegion);
      }
    }
  }, [initialMode, initialRegion]);

  const region =
    choice === OTHER ? normalizeRegion(otherCode) : choice;
  const isValid = mode === "unrestricted" || region !== null;
  const effectiveRegion = mode === "region" ? region : null;

  const dirty =
    mode !== initialMode ||
    (mode === "region" && effectiveRegion !== initialRegion);
  useDirtyGuard(dirty || busy);

  async function handleApply() {
    setBusy(true);
    try {
      const res = await postRegion({
        mode,
        region: mode === "region" ? region : null,
      });
      const section = res.sections.regulatory;
      if (res.overall && section?.ok) {
        const restart = section.data?.restart_required === true;
        toast.ok(
          section.message || t("settings.region.saved"),
          restart
            ? t("settings.region.restartNote")
            : undefined,
        );
        await qc.invalidateQueries({ queryKey: ["setup-status"] });
      } else {
        toast.err(section?.message ?? t("settings.shared.applyFailed"));
      }
    } catch (err) {
      toastFromError(err, t("settings.shared.applyFailed"));
    } finally {
      setBusy(false);
    }
  }

  const countryOptions = [
    ...COMMON_REGIONS.map((r) => ({
      value: r.code,
      label: t("settings.region.countryWithCode", {
        label: t(r.labelKey),
        code: r.code,
      }),
    })),
    { value: OTHER, label: t("settings.region.otherIsoCode") },
  ];

  return (
    <div className="space-y-6">
      <Card>
        <CardContent className="pt-5 pb-5 space-y-1">
          <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-wider text-muted-foreground">
            {t("settings.shared.current")}
            <RiskBadge tone="manual" />
          </div>
          <div className="text-sm">
            {initialMode === "region" && initialRegion ? (
              <>
                <span className="font-mono">{t("settings.region.modeRegionValue")}</span>
                <span className="text-muted-foreground">{t("settings.region.enforcingSeparator")}</span>
                <span className="font-mono">{regionLabel(initialRegion)}</span>
              </>
            ) : (
              <span className="font-mono">{t("settings.region.modeUnrestrictedValue")}</span>
            )}
          </div>
        </CardContent>
      </Card>

      {mode === "unrestricted" && (
        <div className="rounded-md border border-amber-500/40 bg-amber-500/5 px-4 py-3 text-sm">
          <span className="font-medium text-amber-200">{t("settings.region.unrestrictedBannerTitle")}</span>{" "}
          <span className="text-muted-foreground">
            {t("settings.region.unrestrictedBannerBody")}
          </span>
        </div>
      )}

      <div>
        <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-wider text-muted-foreground mb-2">
          {t("settings.region.operatingRegion")}
          <RiskBadge tone="manual" />
        </div>
        <RadioCardGroup
          value={mode}
          onChange={(v) => setMode(v)}
          options={MODE_OPTIONS.map((o) => ({
            value: o.value,
            label: t(o.labelKey),
            description: t(o.descriptionKey),
            badge: o.badgeKey ? t(o.badgeKey) : undefined,
          }))}
          columns={2}
        />
      </div>

      {mode === "region" && (
        <div className="space-y-3">
          <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground">
            {t("settings.region.region")}
          </div>
          <RadioCardGroup
            value={choice}
            onChange={(v) => setChoice(v)}
            options={countryOptions}
            columns={2}
          />
          {choice === OTHER && (
            <div className="space-y-1.5 max-w-xs">
              <Label htmlFor="region-other">{t("settings.region.isoCodeLabel")}</Label>
              <Input
                id="region-other"
                value={otherCode}
                onChange={(e) => setOtherCode(e.target.value.toUpperCase())}
                placeholder={t("settings.region.isoCodePlaceholder")}
                maxLength={2}
                autoCapitalize="characters"
                spellCheck={false}
              />
              {otherCode.length > 0 && region === null && (
                <p className="text-xs text-destructive">
                  {t("settings.region.isoCodeInvalid")}
                </p>
              )}
            </div>
          )}
        </div>
      )}

      <div className="flex items-center justify-end gap-3">
        {dirty && (
          <span className="text-xs text-muted-foreground">{t("settings.shared.unsavedChanges")}</span>
        )}
        <Button
          variant="default"
          disabled={!dirty || !isValid || busy}
          onClick={() => setConfirmOpen(true)}
        >
          {t("settings.region.save")}
        </Button>
      </div>

      <ConfirmDialog
        open={confirmOpen}
        onOpenChange={setConfirmOpen}
        title={t("settings.region.confirmTitle")}
        description={
          <div className="space-y-2">
            {mode === "unrestricted" ? (
              <div>{t("settings.region.confirmUnrestricted")}</div>
            ) : (
              <div>
                <Trans
                  i18nKey="settings.region.confirmEnforce"
                  values={{
                    region: effectiveRegion
                      ? regionLabel(effectiveRegion)
                      : t("settings.region.theRegion"),
                  }}
                  components={{ code: <span className="font-mono font-medium" /> }}
                />
              </div>
            )}
            <div>{t("settings.region.restartNote")}</div>
          </div>
        }
        confirmLabel={t("settings.shared.apply")}
        destructive
        onConfirm={handleApply}
      />
    </div>
  );
}

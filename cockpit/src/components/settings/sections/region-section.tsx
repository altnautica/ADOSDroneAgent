// Region: the RF regulatory posture, read from `network.regulatory` in the
// config. Unrestricted is the default; pinning a region enforces that
// jurisdiction's channel and power limits. Apply posts the regulatory slice to
// the batch apply endpoint (residual Python) behind a slide.

import { useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { EmptyNote, Row } from "@/components/ui/data";
import { ListItem } from "@/components/ui/list-item";
import { TextSettingRow } from "@/components/ui/setting-row";
import { useDirtyGuard } from "@/hooks/use-dirty-guard";
import { CLOSED_GATE, openGate, sectionVerdict, settleGate, type ConfirmGate } from "@/lib/settings/apply-form";
import {
  OTHER_REGION,
  draftRegion,
  regionCurrent,
  regionDirty,
  regionPayload,
  regionSeed,
  regionValid,
  type RegionDraft,
} from "@/lib/settings/region";
import { useConfigStore } from "@/stores/config-store";
import { COMMON_REGIONS, regionLabel } from "@/shared/api/region";
import { postRegion } from "@/shared/api/setup-actions";
import { toast } from "@/shared/api/toast";
import type { RegulatoryInfo, RegulatoryMode } from "@/shared/api/types";
import "@/shared/i18n";
import { ApplyBar, SectionGroup, SlideConfirmDialog } from "./section-kit";
import { toastActionError, useAgentConfig } from "./section-hooks";

const MODE_OPTIONS = [
  {
    value: "unrestricted",
    labelKey: "settings.region.modeUnrestrictedLabel",
    descriptionKey: "settings.region.modeUnrestrictedDescription",
    badgeKey: "settings.region.modeDefaultBadge",
  },
  {
    value: "region",
    labelKey: "settings.region.modeRegionLabel",
    descriptionKey: "settings.region.modeRegionDescription",
    badgeKey: null,
  },
] as const satisfies readonly {
  value: RegulatoryMode;
  labelKey: string;
  descriptionKey: string;
  badgeKey: string | null;
}[];

export function RegionSection() {
  const { t } = useTranslation();
  const { ready, get } = useAgentConfig();
  const current = regionCurrent(get<RegulatoryInfo | null>("network.regulatory", null));

  const [draft, setDraft] = useState<RegionDraft>(() => regionSeed(current));
  const [gate, setGate] = useState<ConfirmGate<RegionDraft>>(CLOSED_GATE);
  const [busy, setBusy] = useState(false);

  // Re-seed only when the agent's own value changes, never on an unrelated poll.
  const seedKey = `${current.mode}|${current.region ?? ""}`;
  const [seededFrom, setSeededFrom] = useState(seedKey);
  if (seededFrom !== seedKey) {
    setSeededFrom(seedKey);
    setDraft(regionSeed(current));
  }

  const dirty = regionDirty(draft, current);
  const valid = regionValid(draft);
  useDirtyGuard(dirty || busy, "settings:region");

  const apply = async (snapshot: RegionDraft) => {
    setBusy(true);
    try {
      const res = await postRegion(regionPayload(snapshot));
      const verdict = sectionVerdict(res, "regulatory");
      if (verdict.ok) {
        toast.ok(
          verdict.message ?? t("settings.region.saved"),
          verdict.restartRequired ? t("settings.region.restartNote") : undefined,
        );
      } else {
        toast.err(verdict.message ?? t("settings.shared.applyFailed"));
      }
    } catch (err) {
      toastActionError(err, t("cockpit.settingsSections.residualMissing"));
    } finally {
      setBusy(false);
      void useConfigStore.getState().refresh();
    }
  };

  if (!ready) return <EmptyNote>{t("cockpit.settingsSections.loading")}</EmptyNote>;

  const otherInvalid = draft.choice === OTHER_REGION && draft.otherCode.length > 0 && draftRegion(draft) === null;
  const confirmRegion = gate.open ? draftRegion(gate.payload) : null;

  return (
    <div className="flex flex-col gap-[0.6rem]">
      <SectionGroup title={t("settings.shared.current")}>
        <Row
          label={t("settings.region.operatingRegion")}
          value={
            current.mode === "region" && current.region
              ? `${t("settings.region.modeRegionValue")}${t("settings.region.enforcingSeparator")}${regionLabel(current.region)}`
              : t("settings.region.modeUnrestrictedValue")
          }
          mono
        />
      </SectionGroup>

      {draft.mode === "unrestricted" ? (
        <div className="mx-[0.75rem] rounded-md border border-warn/40 bg-warn/5 px-[0.75rem] py-[0.5rem] text-[0.85rem]">
          <span className="font-medium text-warn">{t("settings.region.unrestrictedBannerTitle")}</span>{" "}
          <span className="text-muted-foreground">{t("settings.region.unrestrictedBannerBody")}</span>
        </div>
      ) : null}

      <SectionGroup title={t("settings.region.operatingRegion")}>
        {MODE_OPTIONS.map((o) => (
          <ListItem
            key={o.value}
            headline={t(o.labelKey)}
            supporting={t(o.descriptionKey)}
            selected={draft.mode === o.value}
            trailing={
              o.badgeKey ? (
                <span className="text-[0.7rem] uppercase tracking-wide text-muted-foreground">{t(o.badgeKey)}</span>
              ) : undefined
            }
            onClick={() => setDraft((d) => ({ ...d, mode: o.value }))}
          />
        ))}
      </SectionGroup>

      {draft.mode === "region" ? (
        <SectionGroup title={t("settings.region.region")}>
          {COMMON_REGIONS.map((r) => (
            <ListItem
              key={r.code}
              headline={t("settings.region.countryWithCode", { label: t(r.labelKey), code: r.code })}
              selected={draft.choice === r.code}
              onClick={() => setDraft((d) => ({ ...d, choice: r.code }))}
            />
          ))}
          <ListItem
            headline={t("settings.region.otherIsoCode")}
            selected={draft.choice === OTHER_REGION}
            onClick={() => setDraft((d) => ({ ...d, choice: OTHER_REGION }))}
          />
          {draft.choice === OTHER_REGION ? (
            <TextSettingRow
              title={t("settings.region.isoCodeLabel")}
              summary={
                otherInvalid ? <span className="text-err">{t("settings.region.isoCodeInvalid")}</span> : undefined
              }
              value={draft.otherCode}
              placeholder={t("settings.region.isoCodePlaceholder")}
              onChange={(v) => setDraft((d) => ({ ...d, otherCode: v.toUpperCase().slice(0, 2) }))}
            />
          ) : null}
        </SectionGroup>
      ) : null}

      <ApplyBar
        dirty={dirty}
        canApply={valid}
        busy={busy}
        onApply={() => setGate(openGate(draft))}
        onRevert={() => setDraft(regionSeed(current))}
      />

      {gate.open ? (
        <SlideConfirmDialog
          title={t("settings.region.confirmTitle")}
          slideLabel={t("cockpit.settingsSections.region.applySlide")}
          onDismiss={() => setGate(CLOSED_GATE)}
          onConfirmed={() => {
            const snapshot = settleGate(gate, true);
            setGate(CLOSED_GATE);
            if (snapshot) void apply(snapshot);
          }}
        >
          <div className="flex flex-col gap-[0.5rem]">
            {gate.payload.mode === "unrestricted" ? (
              <div>{t("settings.region.confirmUnrestricted")}</div>
            ) : (
              <div>
                <Trans
                  i18nKey="settings.region.confirmEnforce"
                  values={{ region: confirmRegion ? regionLabel(confirmRegion) : t("settings.region.theRegion") }}
                  components={{ code: <span className="font-mono font-medium" /> }}
                />
              </div>
            )}
            <div>{t("settings.region.restartNote")}</div>
          </div>
        </SlideConfirmDialog>
      ) : null}
    </div>
  );
}

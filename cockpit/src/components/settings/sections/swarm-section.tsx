// Swarm: the node's swarm participation, role, default formation and spacing.
// The onboard swarm runtime reads these live and commands the flight
// controller while enabled, so turning it on asks first. Plain config-leaf
// writes.

import { useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { EmptyNote } from "@/components/ui/data";
import { SelectSettingRow, SwitchSettingRow } from "@/components/ui/setting-row";
import {
  FORMATIONS,
  FORMATION_LABEL,
  ROLE_PLACEHOLDER,
  SPACING_BOUNDS,
  SWARM_KEYS,
  toFormation,
} from "@/lib/settings/swarm";
import type { AgentConfig } from "@/shared/api/types";

import { LeafNumberRow, LeafTextRow, SectionGroup, SlideConfirmDialog } from "./section-kit";
import { useAgentConfig, useLeafWriter } from "./section-hooks";

export function SwarmSection() {
  const { t } = useTranslation();
  const { config, ready, get } = useAgentConfig();
  const { save } = useLeafWriter();
  const [confirmEnable, setConfirmEnable] = useState(false);

  if (!ready) return <EmptyNote>{t("settingsMore.security.readingConfig")}</EmptyNote>;
  if (config == null) return <EmptyNote>{t("settingsMore.swarm.readError")}</EmptyNote>;
  const swarm = get<AgentConfig["swarm"] | null>("swarm", null);
  if (!swarm) return <EmptyNote>{t("settingsMore.swarm.unavailable")}</EmptyNote>;

  const formation = toFormation(swarm.default_formation);

  return (
    <div className="flex flex-col gap-[0.8rem]">
      <div className="rounded-md bg-input/40 px-[0.75rem] py-[0.5rem] text-[0.8rem] leading-relaxed text-muted-foreground">
        <Trans
          i18nKey="settingsMore.swarm.configOnlyBanner"
          components={{ strong: <span className="font-medium text-surface-foreground" /> }}
        />
      </div>

      <SwitchSettingRow
        title={t("settingsMore.swarm.participationLabel")}
        summary={t("settingsMore.swarm.participationHint")}
        checked={swarm.enabled === true}
        // Turning off is always safe; turning on hands the runtime the FC.
        onChange={(next) => (next ? setConfirmEnable(true) : void save(SWARM_KEYS.enabled, false))}
      />
      {confirmEnable ? (
        <SlideConfirmDialog
          title={t("settingsMore.swarm.enableTitle")}
          slideLabel={t("cockpit.settingsSections.swarm.enableSlide")}
          onConfirmed={() => {
            setConfirmEnable(false);
            void save(SWARM_KEYS.enabled, true);
          }}
          onDismiss={() => setConfirmEnable(false)}
        >
          {t("settingsMore.swarm.enableBody")}
        </SlideConfirmDialog>
      ) : null}

      <SectionGroup>
        <LeafTextRow
          title={t("settingsMore.swarm.roleLabel")}
          summary={t("settingsMore.swarm.roleHint")}
          value={swarm.role ?? ""}
          placeholder={ROLE_PLACEHOLDER}
          onCommit={(next) => save(SWARM_KEYS.role, next)}
        />
        <SelectSettingRow<string>
          title={t("settingsMore.swarm.formationTitle")}
          summary={formation ? t(FORMATION_LABEL[formation].descriptionKey) : t("settingsMore.swarm.formationHint")}
          options={FORMATIONS}
          value={formation ?? ""}
          optionLabel={(o) => {
            const f = toFormation(o);
            return f ? t(FORMATION_LABEL[f].labelKey) : t("settings.shared.unknown");
          }}
          onSelect={(o) => {
            if (o !== formation) void save(SWARM_KEYS.formation, o);
          }}
        />
        <LeafNumberRow
          title={t("settingsMore.swarm.spacingLabel")}
          summary={t("settingsMore.swarm.spacingHint")}
          value={swarm.default_spacing ?? SPACING_BOUNDS.min}
          enabled={swarm.default_spacing != null}
          min={SPACING_BOUNDS.min}
          max={SPACING_BOUNDS.max}
          onCommit={(next) => save(SWARM_KEYS.spacing, next)}
        />
      </SectionGroup>
    </div>
  );
}

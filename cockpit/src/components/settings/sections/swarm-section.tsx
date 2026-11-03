// Swarm: the node's swarm participation, role, default formation and spacing.
// These persist to config only (no runtime swarm consumer reads them yet),
// which the page says plainly. Plain config-leaf writes.

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

import { LeafNumberRow, LeafTextRow, SectionGroup } from "./section-kit";
import { useAgentConfig, useLeafWriter } from "./section-hooks";

export function SwarmSection() {
  const { t } = useTranslation();
  const { config, ready, get } = useAgentConfig();
  const { save } = useLeafWriter();

  if (!ready) return <EmptyNote>{t("settingsMore.security.readingConfig")}</EmptyNote>;
  if (config == null) return <EmptyNote>{t("settingsMore.swarm.readError")}</EmptyNote>;
  const swarm = get<AgentConfig["swarm"] | null>("swarm", null);
  if (!swarm) return <EmptyNote>{t("settingsMore.swarm.unavailable")}</EmptyNote>;

  const formation = toFormation(swarm.default_formation);

  return (
    <div className="flex flex-col gap-[0.8rem]">
      <div className="rounded-md bg-input/40 px-[0.75rem] py-[0.5rem] text-[0.8rem] leading-relaxed text-muted-foreground">
        <span className="mr-[0.4rem] text-[0.7rem] uppercase tracking-wide">{t("settingsMore.swarm.configOnlyBadge")}</span>
        <Trans
          i18nKey="settingsMore.swarm.configOnlyBanner"
          components={{ strong: <span className="font-medium text-surface-foreground" /> }}
        />
      </div>

      <SwitchSettingRow
        title={t("settingsMore.swarm.participationLabel")}
        summary={t("settingsMore.swarm.participationHint")}
        checked={swarm.enabled === true}
        onChange={(next) => void save(SWARM_KEYS.enabled, next)}
      />

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

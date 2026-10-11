import { useMemo } from "react";
import { Trans, useTranslation } from "react-i18next";

import {
  ConfigEnumField,
  ConfigNumberField,
  ConfigTextField,
  ConfigToggle,
} from "@/components/settings/config-fields";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { useConfig } from "@/hooks/use-config";

/** The built-in formation generators. A free-text name produced no
 * formation at all, so the model now rejects anything outside this set —
 * the field offers exactly what the agent accepts. */
const FORMATION_OPTIONS = [
  {
    value: "line" as const,
    labelKey: "settingsMore.swarm.formationLine",
    descriptionKey: "settingsMore.swarm.formationLineDesc",
  },
  {
    value: "column" as const,
    labelKey: "settingsMore.swarm.formationColumn",
    descriptionKey: "settingsMore.swarm.formationColumnDesc",
  },
  {
    value: "wedge" as const,
    labelKey: "settingsMore.swarm.formationWedge",
    descriptionKey: "settingsMore.swarm.formationWedgeDesc",
  },
  {
    value: "grid" as const,
    labelKey: "settingsMore.swarm.formationGrid",
    descriptionKey: "settingsMore.swarm.formationGridDesc",
  },
  {
    value: "circle" as const,
    labelKey: "settingsMore.swarm.formationCircle",
    descriptionKey: "settingsMore.swarm.formationCircleDesc",
  },
] as const;

/** Role placeholder is the config value itself, not copy. */
const ROLE_PLACEHOLDER = "auto";

function SwarmSection() {
  const { t } = useTranslation();
  const config = useConfig();
  const formationOptions = useMemo(
    () =>
      FORMATION_OPTIONS.map((o) => ({
        value: o.value,
        label: t(o.labelKey),
        description: t(o.descriptionKey),
      })),
    [t],
  );
  const swarm = config.data?.swarm;

  if (!swarm) {
    return (
      <Card>
        <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
          {t("settingsMore.swarm.unavailable")}
        </CardContent>
      </Card>
    );
  }

  return (
    <div className="space-y-6">
      <div className="rounded-md border border-info/40 bg-info/5 px-4 py-3 text-xs text-muted-foreground leading-relaxed">
        <Trans
          i18nKey="settingsMore.swarm.configOnlyBanner"
          components={{ strong: <span className="font-medium text-foreground" /> }}
        />
      </div>

      <Card>
        <CardContent className="pt-5 pb-5">
          <ConfigToggle
            configKey="swarm.enabled"
            label={t("settingsMore.swarm.participationLabel")}
            hint={t("settingsMore.swarm.participationHint")}
            value={swarm.enabled}
          />
        </CardContent>
      </Card>

      <Card>
        <CardContent className="pt-5 pb-5 space-y-5">
          <ConfigTextField
            configKey="swarm.role"
            id="swarm-role"
            label={t("settingsMore.swarm.roleLabel")}
            hint={t("settingsMore.swarm.roleHint")}
            placeholder={ROLE_PLACEHOLDER}
            value={swarm.role}
          />
          <div className="border-t border-border pt-5 space-y-2">
            <div className="text-sm font-medium">{t("settingsMore.swarm.formationTitle")}</div>
            <p className="text-xs text-muted-foreground">
              {t("settingsMore.swarm.formationHint")}
            </p>
            <ConfigEnumField
              configKey="swarm.default_formation"
              value={swarm.default_formation}
              options={formationOptions}
            />
          </div>
          <div className="border-t border-border pt-5">
            <ConfigNumberField
              configKey="swarm.default_spacing"
              id="swarm-spacing"
              label={t("settingsMore.swarm.spacingLabel")}
              hint={t("settingsMore.swarm.spacingHint")}
              value={swarm.default_spacing}
              integer
              min={0}
            />
          </div>
        </CardContent>
      </Card>
    </div>
  );
}

export function SwarmSettings() {
  const { t } = useTranslation();
  const config = useConfig();

  if (config.isLoading) {
    return <p className="text-[11px] text-muted-foreground/70">{t("settingsMore.security.readingConfig")}</p>;
  }
  if (config.isError) {
    return (
      <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
        {t("settingsMore.swarm.readError")}
      </div>
    );
  }

  return (
    <section className="space-y-3">
      <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-wider text-muted-foreground">
        {t("settingsMore.swarm.title")}
        <Badge variant="default" className="font-normal">
          {t("settingsMore.swarm.configOnlyBadge")}
        </Badge>
      </div>
      <SwarmSection />
    </section>
  );
}

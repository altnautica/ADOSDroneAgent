import { useMemo } from "react";
import { Trans, useTranslation } from "react-i18next";

import {
  ConfigEnumField,
  ConfigNumberField,
  ConfigTextField,
  ConfigToggle,
} from "@/components/settings/config-fields";
import { Card, CardContent } from "@/components/ui/card";
import { useConfig } from "@/hooks/use-config";
import {
  FLOCK_NEIGHBORS,
  FLOCK_RADIUS_M,
  GAIN_PCT,
  SEPARATION_M,
  SWARM_MODES,
  separationOrderOk,
  type SwarmBlock,
} from "@/lib/swarm-fields";

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
  const modeOptions = useMemo(
    () =>
      SWARM_MODES.map((value) => ({
        value,
        label: t(`settingsMore.swarm.mode.${value}`),
        description: t(`settingsMore.swarm.mode.${value}Desc`),
      })),
    [t],
  );
  const swarm = config.data?.swarm as SwarmBlock | undefined;

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
            confirm={(next) =>
              next ? { title: t("settingsMore.swarm.enableTitle"), description: t("settingsMore.swarm.enableBody") } : null
            }
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
            <div className="text-sm font-medium">{t("settingsMore.swarm.mode.title")}</div>
            <p className="text-xs text-muted-foreground">{t("settingsMore.swarm.mode.hint")}</p>
            <ConfigEnumField configKey="swarm.mode" value={swarm.mode} options={modeOptions} columns={3} />
          </div>
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
              min={1}
            />
          </div>
        </CardContent>
      </Card>

      <SeparationCard swarm={swarm} />
      <FlockCard swarm={swarm} />
    </div>
  );
}

/** The safety layer: both writes pass a danger confirm, and the repulsion
 *  radius stays above the hard floor. */
function SeparationCard({ swarm }: { swarm: SwarmBlock }) {
  const { t } = useTranslation();
  const radius = swarm.separation?.radius_m;
  const hard = swarm.separation?.hard_m;
  const confirm = {
    title: t("settingsMore.swarm.separation.confirmTitle"),
    description: t("settingsMore.swarm.separation.confirmBody"),
    confirmLabel: t("settingsMore.swarm.separation.confirmAction"),
  };
  const orderError = t("settingsMore.swarm.separation.order");
  return (
    <Card>
      <CardContent className="pt-5 pb-5 space-y-5">
        <div className="text-sm font-medium">{t("settingsMore.swarm.separation.title")}</div>
        <ConfigNumberField
          configKey="swarm.separation.radius_m"
          id="swarm-sep-radius"
          label={t("settingsMore.swarm.separation.radiusLabel")}
          hint={t("settingsMore.swarm.separation.radiusHint")}
          value={radius}
          min={SEPARATION_M.min}
          max={SEPARATION_M.max}
          check={(n) => (separationOrderOk(n, hard) ? null : orderError)}
          confirm={confirm}
        />
        <ConfigNumberField
          configKey="swarm.separation.hard_m"
          id="swarm-sep-hard"
          label={t("settingsMore.swarm.separation.hardLabel")}
          hint={t("settingsMore.swarm.separation.hardHint")}
          value={hard}
          min={SEPARATION_M.min}
          max={SEPARATION_M.max}
          check={(n) => (separationOrderOk(radius, n) ? null : orderError)}
          confirm={confirm}
        />
      </CardContent>
    </Card>
  );
}

/** The flocking law's gains (integer percent) and neighbourhood. */
function FlockCard({ swarm }: { swarm: SwarmBlock }) {
  const { t } = useTranslation();
  const f = swarm.flock ?? {};
  const gains = [
    ["cohesion", f.cohesion],
    ["alignment", f.alignment],
    ["separation_gain", f.separation_gain],
  ] as const;
  return (
    <Card>
      <CardContent className="pt-5 pb-5">
        <details>
          <summary className="cursor-pointer text-sm font-medium min-h-11 md:min-h-0">
            {t("settingsMore.swarm.flock.title")}
          </summary>
          <div className="space-y-5 pt-4">
            {gains.map(([key, value]) => (
              <ConfigNumberField
                key={key}
                configKey={`swarm.flock.${key}`}
                id={`swarm-flock-${key}`}
                label={t(`settingsMore.swarm.flock.${key}Label`)}
                hint={t(`settingsMore.swarm.flock.${key}Hint`)}
                value={value}
                min={GAIN_PCT.min}
                max={GAIN_PCT.max}
              />
            ))}
            <ConfigNumberField
              configKey="swarm.flock.radius_m"
              id="swarm-flock-radius"
              label={t("settingsMore.swarm.flock.radiusLabel")}
              hint={t("settingsMore.swarm.flock.radiusHint")}
              value={f.radius_m}
              min={FLOCK_RADIUS_M.min}
              max={FLOCK_RADIUS_M.max}
            />
            <ConfigNumberField
              configKey="swarm.flock.neighbors"
              id="swarm-flock-neighbors"
              label={t("settingsMore.swarm.flock.neighborsLabel")}
              hint={t("settingsMore.swarm.flock.neighborsHint")}
              value={f.neighbors}
              min={FLOCK_NEIGHBORS.min}
              max={FLOCK_NEIGHBORS.max}
            />
          </div>
        </details>
      </CardContent>
    </Card>
  );
}

export function SwarmSettings() {
  const { t } = useTranslation();
  const config = useConfig();

  if (config.isLoading) {
    return <p className="text-[11px] text-muted-foreground">{t("settingsMore.security.readingConfig")}</p>;
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
      </div>
      <SwarmSection />
    </section>
  );
}

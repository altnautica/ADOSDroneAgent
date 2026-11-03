// Advanced: the structured logger's level, applied on selection, and the HAL
// board override, applied behind a guard because a wrong slug can disable
// peripherals. Both go through the batch apply endpoint (residual Python).

import { useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { EmptyNote } from "@/components/ui/data";
import { GuardedButton } from "@/components/ui/guarded-button";
import { SelectSettingRow, TextSettingRow } from "@/components/ui/setting-row";
import { useDirtyGuard } from "@/hooks/use-dirty-guard";
import {
  LOG_LEVELS,
  boardOverrideFrom,
  canApplyOverride,
  logLevelChange,
  logLevelFrom,
  overrideRequest,
  type LogLevel,
} from "@/lib/settings/advanced";
import { sectionVerdict } from "@/lib/settings/apply-form";
import { useConfigStore } from "@/stores/config-store";
import { postApply } from "@/shared/api/apply-actions";
import { toast } from "@/shared/api/toast";
import "@/shared/i18n";
import { SectionGroup } from "./section-kit";
import { toastActionError, useAgentConfig } from "./section-hooks";

export function AdvancedSection() {
  const { t } = useTranslation();
  const { config, ready } = useAgentConfig();
  const residual = t("cockpit.settingsSections.residualMissing");

  const agentLevel = logLevelFrom(config);
  const agentOverride = boardOverrideFrom(config);

  const [level, setLevel] = useState<LogLevel>(agentLevel);
  const [override, setOverride] = useState(agentOverride);
  const [busy, setBusy] = useState(false);
  const [validationError, setValidationError] = useState<string | null>(null);

  // Follow the agent's values whenever they change.
  const [seededLevel, setSeededLevel] = useState(agentLevel);
  if (seededLevel !== agentLevel) {
    setSeededLevel(agentLevel);
    setLevel(agentLevel);
  }
  const [seededOverride, setSeededOverride] = useState(agentOverride);
  if (seededOverride !== agentOverride) {
    setSeededOverride(agentOverride);
    setOverride(agentOverride);
  }

  useDirtyGuard(override !== agentOverride || busy, "settings:advanced");

  const applyLevel = async (next: LogLevel) => {
    const change = logLevelChange(next, level);
    if (!change) return;
    const previous = level;
    setLevel(next);
    try {
      const res = await postApply({ advanced: change });
      const verdict = sectionVerdict(res, "advanced");
      if (verdict.ok) {
        toast.ok(verdict.message ?? t("settingsMore.advanced.logLevelSet", { level: next }));
      } else {
        setLevel(previous);
        toast.err(verdict.message ?? t("settingsMore.advanced.logLevelFailed"));
      }
    } catch (err) {
      setLevel(previous);
      toastActionError(err, residual);
    } finally {
      void useConfigStore.getState().refresh();
    }
  };

  const applyOverride = async () => {
    const req = overrideRequest(override);
    if (!req.ok) {
      setValidationError(req.error);
      return;
    }
    setBusy(true);
    try {
      const res = await postApply({ advanced: req.body });
      const verdict = sectionVerdict(res, "advanced");
      if (verdict.ok) toast.ok(verdict.message ?? t("settingsMore.advanced.overrideSaved"));
      else toast.err(verdict.message ?? t("settingsMore.cloud.applyFailed"));
    } catch (err) {
      toastActionError(err, residual);
    } finally {
      setBusy(false);
      void useConfigStore.getState().refresh();
    }
  };

  if (!ready) return <EmptyNote>{t("cockpit.settingsSections.loading")}</EmptyNote>;

  const draftCheck = overrideRequest(override);
  const shownError = validationError ?? (draftCheck.ok ? null : draftCheck.error);

  return (
    <div className="flex flex-col gap-[0.6rem]">
      <SectionGroup title={t("settingsMore.advanced.logLevel")}>
        <SelectSettingRow
          title={t("settingsMore.advanced.logLevel")}
          summary={t("settingsMore.advanced.logLevelHint")}
          options={LOG_LEVELS}
          value={level}
          onSelect={(v) => void applyLevel(v)}
        />
      </SectionGroup>

      <SectionGroup title={t("settingsMore.advanced.boardOverride")}>
        <p className="px-[0.75rem] text-[0.8rem] text-muted-foreground">
          {t("settingsMore.advanced.boardOverrideHint")}
        </p>
        <TextSettingRow
          title={t("settingsMore.advanced.slug")}
          value={override}
          placeholder={t("settingsMore.advanced.autoDetect")}
          onChange={(v) => {
            setOverride(v);
            setValidationError(null);
          }}
        />
        {shownError ? (
          <div className="mx-[0.75rem] rounded-md border border-err/40 bg-err/10 px-[0.75rem] py-[0.4rem] text-[0.8rem] text-err">
            {shownError}
          </div>
        ) : null}
        {override !== agentOverride && draftCheck.ok ? (
          <p className="px-[0.75rem] text-[0.8rem] text-warn">
            <Trans
              i18nKey="settingsMore.advanced.confirmBody"
              values={{ slug: override || t("settingsMore.advanced.autoDetect") }}
              components={{ slug: <span className="font-mono font-medium" /> }}
            />
          </p>
        ) : null}
        <div className="flex justify-end px-[0.75rem] pt-[0.3rem]">
          <GuardedButton
            label={t("settingsMore.advanced.applyOverride")}
            guardLabel={t("cockpit.settingsSections.advanced.overrideGuard")}
            disabled={!canApplyOverride(override, agentOverride, busy)}
            onFired={() => void applyOverride()}
          />
        </div>
      </SectionGroup>
    </div>
  );
}

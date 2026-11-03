// Cloud: the cloud posture (local only, the hosted relay, or self-hosted
// endpoints) and the remote-access tunnel token. The posture comes from setup
// status, the self-hosted endpoints from the config. Apply posts the cloud
// slice to the batch apply endpoint (residual Python) behind a slide. The
// tunnel token is set-only: masked while typed, cleared after submit, never
// read back, and submitted behind a slide because it exposes the node
// remotely.

import { useCallback, useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { EmptyNote } from "@/components/ui/data";
import { ListItem } from "@/components/ui/list-item";
import { SettingRow, TextSettingRow } from "@/components/ui/setting-row";
import { SlideToConfirm } from "@/components/ui/slide-to-confirm";
import { useDirtyGuard } from "@/hooks/use-dirty-guard";
import { CLOSED_GATE, openGate, sectionVerdict, settleGate, type ConfirmGate } from "@/lib/settings/apply-form";
import {
  CLOUD_MODE_OPTIONS,
  cloudDirty,
  cloudModeFrom,
  cloudSeed,
  tunnelTokenBody,
  validateCloud,
  type CloudDraft,
} from "@/lib/settings/cloud";
import { useConfigStore } from "@/stores/config-store";
import { apiFetch } from "@/shared/api-fetch";
import type { SetupStatus } from "@/shared/agent-api.generated";
import { postApply, type CloudSection as CloudSlice } from "@/shared/api/apply-actions";
import { toast } from "@/shared/api/toast";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";
import { ApplyBar, ResidualMissingNote, SectionGroup, SlideConfirmDialog } from "./section-kit";
import { toastActionError, useAgentConfig } from "./section-hooks";

// Example hostnames, not translatable copy.
const URL_PLACEHOLDER = "https://convex.example.com";
const BROKER_PLACEHOLDER = "mqtt.example.com";

const INPUT_CLASS =
  "touch-target w-[clamp(7.5rem,40%,14rem)] rounded-md border border-border bg-input px-[0.6rem] text-[0.9rem] text-surface-foreground focus:border-primary focus:outline-none";

interface SetupActionResult {
  ok: boolean;
  message: string;
}

export function CloudSection() {
  const { t } = useTranslation();
  const { config, ready } = useAgentConfig();
  const setup = useResource(
    useCallback((s: AbortSignal) => apiFetch<SetupStatus>("/api/v1/setup/status", { signal: s }), []),
    20_000,
  );

  const initial = cloudSeed(cloudModeFrom(setup.data?.cloud_choice?.mode), config);
  const [draft, setDraft] = useState<CloudDraft>(initial);
  const [gate, setGate] = useState<ConfirmGate<CloudSlice>>(CLOSED_GATE);
  const [busy, setBusy] = useState(false);
  const [validationError, setValidationError] = useState<string | null>(null);

  // Re-seed only when the agent's own values change, never on an unrelated poll.
  const seedKey = `${initial.mode}|${initial.url}|${initial.broker}|${initial.port}`;
  const [seededFrom, setSeededFrom] = useState(seedKey);
  if (seededFrom !== seedKey) {
    setSeededFrom(seedKey);
    setDraft(initial);
  }

  const dirty = cloudDirty(draft, initial);
  useDirtyGuard(dirty || busy, "settings:cloud");

  const edit = (patch: Partial<CloudDraft>) => {
    setDraft((d) => ({ ...d, ...patch }));
    setValidationError(null);
  };

  const requestApply = () => {
    const v = validateCloud(draft);
    if (!v.ok) {
      setValidationError(v.error);
      return;
    }
    setValidationError(null);
    setGate(openGate(v.section));
  };

  const apply = async (section: CloudSlice) => {
    setBusy(true);
    try {
      const res = await postApply({ cloud: section });
      const verdict = sectionVerdict(res, "cloud");
      if (verdict.ok) {
        toast.ok(verdict.message ?? t("settingsMore.cloud.saved"));
        setDraft((d) => ({ ...d, apiKey: "" }));
      } else {
        toast.err(verdict.message ?? t("settingsMore.cloud.applyFailed"));
      }
    } catch (err) {
      toastActionError(err, t("cockpit.settingsSections.residualMissing"));
    } finally {
      setBusy(false);
      setup.refresh();
      void useConfigStore.getState().refresh();
    }
  };

  if (!ready || !setup.ready) return <EmptyNote>{t("cockpit.settingsSections.loading")}</EmptyNote>;

  return (
    <div className="flex flex-col gap-[0.6rem]">
      {setup.status === 501 ? <ResidualMissingNote /> : null}

      <SectionGroup title={t("settingsMore.cloud.posture")}>
        {CLOUD_MODE_OPTIONS.map((o) => (
          <ListItem
            key={o.value}
            headline={t(o.labelKey)}
            supporting={t(o.descriptionKey)}
            selected={draft.mode === o.value}
            onClick={() => edit({ mode: o.value })}
          />
        ))}
      </SectionGroup>

      {draft.mode === "self_hosted" ? (
        <SectionGroup title={t("settingsMore.cloud.selfHostedEndpoints")}>
          <TextSettingRow
            title={t("settingsMore.cloud.convexUrl")}
            value={draft.url}
            placeholder={URL_PLACEHOLDER}
            inputMode="url"
            onChange={(v) => edit({ url: v })}
          />
          <TextSettingRow
            title={t("settingsMore.cloud.mqttBroker")}
            value={draft.broker}
            placeholder={BROKER_PLACEHOLDER}
            onChange={(v) => edit({ broker: v })}
          />
          <TextSettingRow
            title={t("settingsMore.cloud.port")}
            value={draft.port}
            inputMode="numeric"
            onChange={(v) => edit({ port: v })}
          />
          <SettingRow title={t("settingsMore.cloud.apiKey")} summary={t("settingsMore.cloud.apiKeyHint")}>
            <input
              type="password"
              autoComplete="new-password"
              value={draft.apiKey}
              placeholder={t("settingsMore.cloud.apiKeyPlaceholder")}
              aria-label={t("settingsMore.cloud.apiKey")}
              onChange={(e) => edit({ apiKey: e.target.value })}
              className={INPUT_CLASS}
            />
          </SettingRow>
          {validationError ? (
            <div className="mx-[0.75rem] rounded-md border border-err/40 bg-err/10 px-[0.75rem] py-[0.4rem] text-[0.8rem] text-err">
              {validationError}
            </div>
          ) : null}
        </SectionGroup>
      ) : null}

      <ApplyBar
        dirty={dirty}
        busy={busy}
        onApply={requestApply}
        onRevert={() => {
          setDraft(initial);
          setValidationError(null);
        }}
      />

      <TunnelTokenGroup />

      {gate.open ? (
        <SlideConfirmDialog
          title={t("settingsMore.cloud.confirmTitle")}
          slideLabel={t("cockpit.settingsSections.cloud.applySlide")}
          onDismiss={() => setGate(CLOSED_GATE)}
          onConfirmed={() => {
            const section = settleGate(gate, true);
            setGate(CLOSED_GATE);
            if (section) void apply(section);
          }}
        >
          <Trans
            i18nKey="settingsMore.cloud.confirmBody"
            values={{ mode: gate.payload.mode }}
            components={{ mode: <span className="font-mono font-medium" /> }}
          />
        </SlideConfirmDialog>
      ) : null}
    </div>
  );
}

/** Remote access: install a tunnel token behind a slide. */
function TunnelTokenGroup() {
  const { t } = useTranslation();
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const body = tunnelTokenBody(token);

  const submit = async () => {
    const req = tunnelTokenBody(token);
    if (!req || busy) return;
    setBusy(true);
    try {
      const res = await apiFetch<SetupActionResult>("/api/v1/setup/remote-access/cloudflare", {
        method: "POST",
        body: req,
      });
      if (res?.ok) {
        toast.ok(res.message || t("settings.shared.cfTokenInstalled"));
        setToken("");
      } else {
        toast.err(res?.message || t("settings.shared.cfTokenRejected"));
      }
    } catch (err) {
      toastActionError(err, t("cockpit.settingsSections.residualMissing"));
    } finally {
      setBusy(false);
    }
  };

  return (
    <SectionGroup title={t("settings.shared.cfTitle")}>
      <p className="px-[0.75rem] text-[0.8rem] text-muted-foreground">
        <Trans i18nKey="settings.shared.cfDescription" components={{ code: <code className="font-mono" /> }} />
      </p>
      <SettingRow title={t("settings.shared.cfTokenLabel")}>
        <input
          type="password"
          autoComplete="off"
          spellCheck={false}
          value={token}
          disabled={busy}
          placeholder={t("settings.shared.cfTokenPlaceholder")}
          aria-label={t("settings.shared.cfTokenLabel")}
          onChange={(e) => setToken(e.target.value)}
          className={INPUT_CLASS}
        />
      </SettingRow>
      <div className="px-[0.75rem] pt-[0.3rem]">
        <SlideToConfirm
          label={t("cockpit.settingsSections.cloud.tokenSlide")}
          disabled={busy || body === null}
          onConfirmed={() => void submit()}
        />
      </div>
    </SectionGroup>
  );
}

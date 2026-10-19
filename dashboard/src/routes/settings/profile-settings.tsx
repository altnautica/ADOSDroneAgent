import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { RiskBadge } from "@/components/settings/risk-badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { RadioCardGroup } from "@/components/ui/radio-card-group";
import { Switch } from "@/components/ui/switch";
import { useDirtyGuard } from "@/hooks/use-dirty-guard";
import { useNodeIdentity } from "@/hooks/use-node-identity";
import { useStatus } from "@/hooks/use-status";
import { toast, toastFromError } from "@/lib/toast";
import { type ApplyPayload, type ApplyProfile, postApply, profileFromStatus } from "@/lib/apply-actions";
import { withArmedRetry } from "@/lib/armed-retry";
import type { GroundRole } from "@/lib/types";
import { apiFetch } from "@/shared/api-fetch";
import { invalidateProfile } from "@/shared/use-profile";

// Workstation and compute are detected, never chosen here.
const PROFILE_OPTIONS = [
  {
    value: "drone" as const,
    labelKey: "settings.profile.droneLabel",
    descriptionKey: "settings.profile.droneDescription",
  },
  {
    value: "ground_station" as const,
    labelKey: "settings.profile.groundStationLabel",
    descriptionKey: "settings.profile.groundStationDescription",
  },
  {
    value: "visor" as const,
    labelKey: "settings.profile.visorLabel",
    descriptionKey: "settings.profile.visorDescription",
  },
] as const;

const ROLE_OPTIONS = [
  {
    value: "direct" as const,
    labelKey: "settings.profile.directLabel",
    descriptionKey: "settings.profile.directDescription",
  },
  {
    value: "relay" as const,
    labelKey: "settings.profile.relayLabel",
    descriptionKey: "settings.profile.relayDescription",
  },
  {
    value: "receiver" as const,
    labelKey: "settings.profile.receiverLabel",
    descriptionKey: "settings.profile.receiverDescription",
  },
] as const;

export function ProfileSettings() {
  const { t } = useTranslation();
  const status = useStatus();
  const identity = useNodeIdentity();
  const qc = useQueryClient();

  const [profile, setProfile] = useState<ApplyProfile>("drone");
  const [role, setRole] = useState<GroundRole>("direct");
  const [autoRestart, setAutoRestart] = useState(true);
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [redetecting, setRedetecting] = useState(false);
  // The pending armed-override question: its text and the answer's resolver.
  const [armedPrompt, setArmedPrompt] = useState<{
    message: string;
    resolve: (ok: boolean) => void;
  } | null>(null);

  const initialProfile = profileFromStatus(identity.profile);
  const initialRole = identity.role;

  // Re-seed the draft only when the agent's value itself changes. Keyed on the
  // whole status object this ran on every poll that moved any field, and
  // threw away the operator's unsaved selection.
  useEffect(() => {
    setProfile(initialProfile);
    setRole(initialRole);
  }, [initialProfile, initialRole]);
  const dirty =
    profile !== initialProfile ||
    (profile === "ground_station" && role !== initialRole);
  useDirtyGuard(dirty || busy);

  async function handleRedetect() {
    setRedetecting(true);
    try {
      await apiFetch("/api/v1/setup/profile/redetect", { method: "POST" });
      await qc.invalidateQueries({ queryKey: ["setup-status"] });
      toast.ok(t("settings.profile.redetectOk"));
    } catch (err) {
      toastFromError(err, t("settings.profile.redetectFailed"));
    } finally {
      setRedetecting(false);
    }
  }

  async function handleApply() {
    setBusy(true);
    try {
      const res = await withArmedRetry(
        (force) => {
          const payload: ApplyPayload = {
            profile: {
              profile,
              ground_role: profile === "ground_station" ? role : undefined,
              auto_restart: autoRestart,
            },
            force: force || undefined,
          };
          return postApply(payload);
        },
        (message) => {
          setConfirmOpen(false);
          return new Promise<boolean>((resolve) =>
            setArmedPrompt({ message, resolve }),
          );
        },
      );
      const section = res.sections.profile;
      if (res.overall && section?.ok) {
        toast.ok(section.message || t("settings.profile.saved"));
        // The node may now serve a different page set; probe it again.
        invalidateProfile();
        void qc.invalidateQueries({ queryKey: ["pairing-info"] });
      } else {
        toast.err(section?.message ?? t("settings.shared.applyFailed"));
      }
    } catch (err) {
      toastFromError(err, t("settings.shared.applyFailed"));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="space-y-6">
      <Card>
        <CardContent className="pt-5 pb-5 space-y-1">
          <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-wider text-muted-foreground">
            {t("settings.shared.current")}
            <RiskBadge tone="manual" />
          </div>
          <div className="text-sm">
            <span className="font-mono">{initialProfile}</span>
            {initialProfile === "ground_station" && (
              <>
                <span className="text-muted-foreground">{t("settings.profile.roleSeparator")}</span>
                <span className="font-mono">{initialRole}</span>
              </>
            )}
            <span className="text-muted-foreground">
              {t("settings.profile.sourceSeparator")}
            </span>
            <span className="font-mono">
              {status.data?.profile_source ?? t("settings.shared.unknown")}
            </span>
          </div>
          <div className="pt-2">
            <Button
              variant="outline"
              size="sm"
              disabled={redetecting}
              onClick={handleRedetect}
            >
              {redetecting ? t("settings.profile.detecting") : t("settings.profile.redetect")}
            </Button>
          </div>
        </CardContent>
      </Card>

      <div>
        <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-wider text-muted-foreground mb-2">
          {t("settings.profile.profileHeading")}
          <RiskBadge tone="manual" />
        </div>
        <RadioCardGroup
          value={profile}
          onChange={(v) => setProfile(v as ApplyProfile)}
          options={PROFILE_OPTIONS.map((o) => ({
            value: o.value,
            label: t(o.labelKey),
            description: t(o.descriptionKey),
          }))}
          columns={2}
        />
      </div>

      {profile === "ground_station" && (
        <div>
          <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-wider text-muted-foreground mb-2">
            {t("settings.profile.roleHeading")}
            <RiskBadge tone="manual" />
          </div>
          <RadioCardGroup
            value={role}
            onChange={(v) => setRole(v as GroundRole)}
            options={ROLE_OPTIONS.map((o) => ({
              value: o.value,
              label: t(o.labelKey),
              description: t(o.descriptionKey),
            }))}
            columns={3}
          />
        </div>
      )}

      <Card>
        <CardContent className="pt-5 pb-5 flex items-center justify-between gap-4">
          <div>
            <div className="flex items-center gap-2 text-sm font-medium">
              {t("settings.profile.autoRestart")}
              <RiskBadge tone="auto" />
            </div>
            <div className="text-xs text-muted-foreground mt-1">
              {t("settings.profile.autoRestartHint")}
            </div>
          </div>
          <Switch
            checked={autoRestart}
            onCheckedChange={setAutoRestart}
            aria-label={t("settings.profile.autoRestart")}
          />
        </CardContent>
      </Card>

      <div className="flex items-center justify-end gap-3">
        {dirty && (
          <span className="text-xs text-muted-foreground">{t("settings.shared.unsavedChanges")}</span>
        )}
        <Button
          variant="default"
          disabled={!dirty || busy}
          onClick={() => setConfirmOpen(true)}
        >
          {t("settings.profile.save")}
        </Button>
      </div>

      <ConfirmDialog
        open={confirmOpen}
        onOpenChange={setConfirmOpen}
        title={t("settings.profile.confirmTitle")}
        description={
          <div className="space-y-2">
            <div>
              {profile === "ground_station" ? (
                <Trans
                  i18nKey="settings.profile.confirmReconfigureRole"
                  values={{ profile, role }}
                  components={{ code: <span className="font-mono font-medium" /> }}
                />
              ) : (
                <Trans
                  i18nKey="settings.profile.confirmReconfigure"
                  values={{ profile }}
                  components={{ code: <span className="font-mono font-medium" /> }}
                />
              )}
            </div>
            {autoRestart && (
              <div>{t("settings.profile.confirmRestart")}</div>
            )}
          </div>
        }
        confirmLabel={t("settings.shared.apply")}
        destructive
        onConfirm={handleApply}
      />

      <ConfirmDialog
        open={armedPrompt !== null}
        onOpenChange={(open) => {
          if (!open) {
            armedPrompt?.resolve(false);
            setArmedPrompt(null);
          }
        }}
        title={t("settings.profile.armedTitle")}
        description={armedPrompt?.message ?? ""}
        confirmLabel={t("settings.profile.applyAnyway")}
        destructive
        onConfirm={() => armedPrompt?.resolve(true)}
      />
    </div>
  );
}

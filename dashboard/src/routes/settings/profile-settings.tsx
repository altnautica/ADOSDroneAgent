import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { RiskBadge } from "@/components/settings/risk-badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { RadioCardGroup } from "@/components/ui/radio-card-group";
import { useDirtyGuard } from "@/hooks/use-dirty-guard";
import { useNodeIdentity } from "@/hooks/use-node-identity";
import { useHeartbeat } from "@/hooks/use-heartbeat";
import { useStatus } from "@/hooks/use-status";
import { toast, toastFromError } from "@/shared/api/toast";
import {
  type ApplyPayload,
  type ApplyProfile,
  postApply,
  profileChangeNoteKeys,
  profileFromStatus,
} from "@/shared/api/apply-actions";
import { withArmedRetry } from "@/shared/api/armed-retry";
import type { GroundRole } from "@/shared/api/types";
import { apiFetch } from "@/shared/api-fetch";
import { invalidateProfile } from "@/shared/use-profile";
import { profileLabel, roleLabel } from "@/lib/enum-labels";
import { type ProfileSuggestion, suggestionDraft, suggestionLabel } from "@/lib/profile-suggestion";
import { useUiStore } from "@/stores/ui-store";

const SOURCE_KEYS = {
  detected: "settings.profile.source.detected",
  tiebreaker: "settings.profile.source.tiebreaker",
  override: "settings.profile.source.override",
  default: "settings.profile.source.default",
  user: "settings.profile.source.user",
} as const;

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
  const heartbeat = useHeartbeat();
  const beginRestart = useUiStore((s) => s.beginRestart);

  const [profile, setProfile] = useState<ApplyProfile>("drone");
  const [role, setRole] = useState<GroundRole>("direct");
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [redetecting, setRedetecting] = useState(false);
  const [suggestion, setSuggestion] = useState<ProfileSuggestion | null>(null);
  // The pending armed-override question's resolver.
  const [armedPrompt, setArmedPrompt] = useState<{ resolve: (ok: boolean) => void } | null>(null);

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
      setSuggestion(await apiFetch<ProfileSuggestion>("/api/v1/setup/profile/redetect", { method: "POST" }));
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
          // Always with the restart: a profile change only takes effect once
          // the agent has moved its services, and leaving that to a later
          // plain restart left the old profile's services enabled.
          const payload: ApplyPayload = {
            profile: {
              profile,
              ground_role: profile === "ground_station" ? role : undefined,
              auto_restart: true,
            },
            force: force || undefined,
          };
          return postApply(payload);
        },
        () => {
          setConfirmOpen(false);
          return new Promise<boolean>((resolve) => setArmedPrompt({ resolve }));
        },
      );
      const section = res.sections.profile;
      if (res.overall && section?.ok) {
        toast.ok(section.message || t("settings.profile.saved"));
        // An apply that needs a restart restarts the agent at once: say so
        // until it answers again.
        if (section.data?.restart_required === true) {
          beginRestart("profile", heartbeat.data?.uptime_seconds ?? null);
        }
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
            <span>{profileLabel(initialProfile)}</span>
            {initialProfile === "ground_station" && (
              <>
                <span className="text-muted-foreground">{t("settings.profile.roleSeparator")}</span>
                <span>{roleLabel(initialRole)}</span>
              </>
            )}
            <span className="text-muted-foreground">
              {t("settings.profile.sourceSeparator")}
            </span>
            <span>
              {(() => {
                const src = status.data?.profile_source;
                return src && src in SOURCE_KEYS
                  ? t(SOURCE_KEYS[src as keyof typeof SOURCE_KEYS])
                  : t("settings.shared.unknown");
              })()}
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
          {suggestion && (
            <div className="mt-3 flex flex-wrap items-center gap-3 rounded-md border border-border px-3 py-2 text-sm">
              <span>
                {t("settings.profile.suggested", { profile: suggestionLabel(suggestion) })}
              </span>
              <Button
                size="sm"
                variant="outline"
                onClick={() => {
                  const draft = suggestionDraft(suggestion, role);
                  setProfile(draft.profile);
                  setRole(draft.role);
                  setConfirmOpen(true);
                }}
              >
                {t("settings.profile.applySuggestion")}
              </Button>
            </div>
          )}
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
                  values={{ profile: profileLabel(profile), role: roleLabel(role) }}
                  components={{ code: <span className="font-mono font-medium" /> }}
                />
              ) : (
                <Trans
                  i18nKey="settings.profile.confirmReconfigure"
                  values={{ profile: profileLabel(profile) }}
                  components={{ code: <span className="font-mono font-medium" /> }}
                />
              )}
            </div>
            {profileChangeNoteKeys(initialProfile, profile).map((key) => (
              <div key={key}>{t(key)}</div>
            ))}
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
        description={t("settings.shared.armedOverride")}
        confirmLabel={t("settings.profile.applyAnyway")}
        destructive
        onConfirm={() => armedPrompt?.resolve(true)}
      />
    </div>
  );
}

// Profile: the node profile and ground-station role, plus a hardware
// re-detect. Apply posts the profile slice to the batch apply endpoint
// (residual Python) behind a slide; an armed refusal asks again with the
// armed-override slide and retries with `force`. A successful switch drops the
// cached profile so tabs and home re-resolve.

import { useCallback, useState } from "react";
import { RefreshCw } from "lucide-react";
import { Trans, useTranslation } from "react-i18next";

import { ActionButton, EmptyNote, Row } from "@/components/ui/data";
import { ListItem } from "@/components/ui/list-item";
import { useDirtyGuard } from "@/hooks/use-dirty-guard";
import { profileWordKey, roleWordKey } from "@/lib/profile-words";
import { isKiosk } from "@/lib/kiosk";
import { CLOSED_GATE, openGate, sectionVerdict, settleGate, type ConfirmGate } from "@/lib/settings/apply-form";
import { PROFILE_OPTIONS, ROLE_OPTIONS, profileDirty, profilePayload, type ProfileDraft } from "@/lib/settings/profile";
import { apiFetch } from "@/shared/api-fetch";
import type { SetupStatus } from "@/shared/agent-api.generated";
import { postApply, profileChangeNoteKeys, profileFromStatus } from "@/shared/api/apply-actions";
import { withArmedRetry } from "@/shared/api/armed-retry";
import { toast } from "@/shared/api/toast";
import "@/shared/i18n";
import { invalidateProfile, useNodeIdentity } from "@/shared/use-profile";
import { useResource } from "@/shared/use-resource";
import { ApplyBar, ResidualMissingNote, SectionGroup, SlideConfirmDialog } from "./section-kit";
import { toastActionError, useArmedConfirm } from "./section-hooks";

export function ProfileSection() {
  const { t } = useTranslation();
  const identity = useNodeIdentity();
  const setup = useResource(
    useCallback((s: AbortSignal) => apiFetch<SetupStatus>("/api/v1/setup/status", { signal: s }), []),
    20_000,
  );
  const armed = useArmedConfirm();

  const known = identity.profile !== null;
  const initialProfile = profileFromStatus(identity.profile ?? undefined);
  const initialRole = identity.role ?? "direct";

  const [draft, setDraft] = useState<ProfileDraft>({ profile: initialProfile, role: initialRole });
  const [gate, setGate] = useState<ConfirmGate<ProfileDraft>>(CLOSED_GATE);
  const [busy, setBusy] = useState(false);
  const [redetecting, setRedetecting] = useState(false);

  // Re-seed only when the agent's own value changes, never on an unrelated poll.
  const seedKey = `${initialProfile}|${initialRole}`;
  const [seededFrom, setSeededFrom] = useState(seedKey);
  if (seededFrom !== seedKey) {
    setSeededFrom(seedKey);
    setDraft((d) => ({ ...d, profile: initialProfile, role: initialRole }));
  }

  const dirty = known && profileDirty(draft, { profile: initialProfile, role: initialRole });
  useDirtyGuard(dirty || busy, "settings:profile");

  const residual = t("cockpit.settingsSections.residualMissing");

  const redetect = () => {
    setRedetecting(true);
    apiFetch("/api/v1/setup/profile/redetect", { method: "POST" })
      .then(() => toast.ok(t("settings.profile.redetectOk")))
      .catch((err: unknown) => toastActionError(err, residual))
      .finally(() => {
        setRedetecting(false);
        setup.refresh();
      });
  };

  const apply = async (snapshot: ProfileDraft) => {
    setBusy(true);
    try {
      const res = await withArmedRetry((force) => postApply(profilePayload(snapshot, force)), () => armed.confirm());
      const verdict = sectionVerdict(res, "profile");
      if (verdict.ok) {
        toast.ok(verdict.message ?? t("settings.profile.saved"));
        // The node may now serve a different page set; probe it again.
        invalidateProfile();
      } else {
        toast.err(verdict.message ?? t("settings.shared.applyFailed"));
      }
    } catch (err) {
      toastActionError(err, residual);
    } finally {
      setBusy(false);
      setup.refresh();
    }
  };

  if (!known) return <EmptyNote>{t("cockpit.settingsSections.loading")}</EmptyNote>;

  const source = setup.data?.profile_source ?? t("settings.shared.unknown");

  return (
    <div className="flex flex-col gap-[0.6rem]">
      <SectionGroup title={t("settings.shared.current")}>
        <Row
          label={t("settings.profile.profileHeading")}
          value={
            initialProfile === "ground_station"
              ? `${t(profileWordKey(initialProfile))}${t("access.profileGate.roleSuffix", { role: t(roleWordKey(initialRole)) })}`
              : t(profileWordKey(initialProfile))
          }
          hint={`${t("settings.profile.sourceSeparator")}${source}`}
        />
        {setup.status === 501 ? <ResidualMissingNote /> : null}
        <div className="px-[0.75rem]">
          <ActionButton
            label={redetecting ? t("settings.profile.detecting") : t("settings.profile.redetect")}
            icon={RefreshCw}
            onClick={redetect}
            busy={redetecting}
          />
        </div>
      </SectionGroup>

      <SectionGroup title={t("settings.profile.profileHeading")}>
        {PROFILE_OPTIONS.map((o) => (
          <ListItem
            key={o.value}
            headline={t(o.labelKey)}
            supporting={t(o.descriptionKey)}
            selected={draft.profile === o.value}
            onClick={() => setDraft((d) => ({ ...d, profile: o.value }))}
          />
        ))}
      </SectionGroup>

      {draft.profile === "ground_station" ? (
        <SectionGroup title={t("settings.profile.roleHeading")}>
          {ROLE_OPTIONS.map((o) => (
            <ListItem
              key={o.value}
              headline={t(o.labelKey)}
              supporting={t(o.descriptionKey)}
              selected={draft.role === o.value}
              onClick={() => setDraft((d) => ({ ...d, role: o.value }))}
            />
          ))}
        </SectionGroup>
      ) : null}

      <ApplyBar
        dirty={dirty}
        busy={busy}
        onApply={() => setGate(openGate(draft))}
        onRevert={() => setDraft((d) => ({ ...d, profile: initialProfile, role: initialRole }))}
      />

      {gate.open ? (
        <SlideConfirmDialog
          title={t("settings.profile.confirmTitle")}
          slideLabel={t("cockpit.settingsSections.profile.applySlide")}
          onDismiss={() => setGate(CLOSED_GATE)}
          onConfirmed={() => {
            const snapshot = settleGate(gate, true);
            setGate(CLOSED_GATE);
            if (snapshot) void apply(snapshot);
          }}
        >
          <div className="flex flex-col gap-[0.5rem]">
            <div>
              <Trans
                i18nKey={
                  gate.payload.profile === "ground_station"
                    ? "settings.profile.confirmReconfigureRole"
                    : "settings.profile.confirmReconfigure"
                }
                values={{ profile: gate.payload.profile, role: gate.payload.role }}
                components={{ code: <span className="font-mono font-medium" /> }}
              />
            </div>
            {profileChangeNoteKeys(initialProfile, gate.payload.profile).map((key) => (
              <div key={key}>{t(key)}</div>
            ))}
            {isKiosk() && initialProfile === "ground_station" && gate.payload.profile !== "ground_station" ? (
              <div>{t("cockpit.settingsSections.profile.kioskScreenOff")}</div>
            ) : null}
          </div>
        </SlideConfirmDialog>
      ) : null}

      {armed.dialog}
    </div>
  );
}

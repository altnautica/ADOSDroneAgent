import { Ban } from "lucide-react";
import { Trans, useTranslation } from "react-i18next";
import { Link } from "react-router-dom";

import i18n from "@/i18n";

import { Button } from "@/components/ui/button";
import { useNodeIdentity } from "@/hooks/use-node-identity";
import { gateDecision, type AllowedProfile } from "@/lib/node-identity";
import type { GroundRole, Profile } from "@/lib/types";

export type { AllowedProfile };

interface ProfileGateProps {
  allow: AllowedProfile[];
  roles?: GroundRole[];
  children: React.ReactNode;
}

function profileLabel(p: AllowedProfile | Profile): string {
  if (p === "drone") return i18n.t("access.profileGate.profileDrone");
  if (p === "ground_station") return i18n.t("access.profileGate.profileGroundStation");
  if (p === "auto") return i18n.t("access.profileGate.profileAuto");
  return i18n.t("access.profileGate.profileOther");
}

function roleLabel(r: GroundRole): string {
  return r === "direct"
    ? i18n.t("access.profileGate.roleDirect")
    : r === "relay"
      ? i18n.t("access.profileGate.roleRelay")
      : i18n.t("access.profileGate.roleReceiver");
}

function ProfileMismatchPanel({
  current,
  currentRole,
  allow,
  roles,
}: {
  current: Profile;
  currentRole: GroundRole;
  allow: AllowedProfile[];
  roles?: GroundRole[];
}) {
  const { t } = useTranslation();
  const expected = allow.map(profileLabel).join(t("access.profileGate.or"));
  const expectedRole = roles?.length
    ? t("access.profileGate.roleSuffix", { role: roles.map(roleLabel).join(" / ") })
    : "";
  const currentRoleSuffix =
    current === "ground_station"
      ? t("access.profileGate.roleSuffix", { role: roleLabel(currentRole) })
      : "";

  return (
    <div className="max-w-md space-y-4 py-12">
      <div className="inline-flex items-center justify-center h-12 w-12 rounded-lg bg-muted">
        <Ban className="h-5 w-5 text-muted-foreground" />
      </div>
      <h1 className="text-xl font-semibold tracking-tight">
        {t("access.profileGate.title")}
      </h1>
      <p className="text-sm text-muted-foreground">
        <Trans
          i18nKey="access.profileGate.body"
          values={{
            expected,
            expectedRole,
            current: profileLabel(current),
            currentRole: currentRoleSuffix,
          }}
          components={{ link: <Link to="/settings/profile" className="underline" /> }}
        />
      </p>
      <Button asChild variant="outline" size="sm">
        <Link to="/">{t("common.backToHome")}</Link>
      </Button>
    </div>
  );
}

export function ProfileGate({ allow, roles, children }: ProfileGateProps) {
  const { profile, role } = useNodeIdentity();

  if (gateDecision(profile, role, allow, roles) === "mismatch") {
    return (
      <ProfileMismatchPanel
        current={profile}
        currentRole={role}
        allow={allow}
        roles={roles}
      />
    );
  }

  return <>{children}</>;
}

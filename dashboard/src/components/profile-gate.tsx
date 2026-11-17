import { Ban } from "lucide-react";
import { Trans, useTranslation } from "react-i18next";
import { Link } from "react-router-dom";

import { Button } from "@/components/ui/button";
import { useNodeIdentity } from "@/hooks/use-node-identity";
import { profileLabel, roleLabel } from "@/lib/enum-labels";
import { gateDecision, type AllowedProfile, type HostKind } from "@/lib/node-identity";
import { useNodeHostKind } from "@/hooks/use-node-host-kind";
import type { GroundRole, Profile } from "@/shared/api/types";

export type { AllowedProfile };

interface ProfileGateProps {
  allow: AllowedProfile[];
  roles?: GroundRole[];
  hosts?: HostKind[];
  children: React.ReactNode;
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

function NotAvailablePanel() {
  const { t } = useTranslation();
  return (
    <div className="max-w-md space-y-4 py-12">
      <div className="inline-flex items-center justify-center h-12 w-12 rounded-lg bg-muted">
        <Ban className="h-5 w-5 text-muted-foreground" />
      </div>
      <h1 className="text-xl font-semibold tracking-tight">
        {t("access.profileGate.unavailableTitle")}
      </h1>
      <p className="text-sm text-muted-foreground">{t("access.profileGate.unavailableBody")}</p>
      <Button asChild variant="outline" size="sm">
        <Link to="/">{t("common.backToHome")}</Link>
      </Button>
    </div>
  );
}

interface ProfileGateViewProps extends ProfileGateProps {
  profile: Profile;
  role: GroundRole;
  host: HostKind;
}

/** The gate for a known identity: the page, the mismatch panel, or the
 *  neutral not-available state for a profile this dashboard does not know. */
export function ProfileGateView({
  profile,
  role,
  host,
  allow,
  roles,
  hosts,
  children,
}: ProfileGateViewProps) {
  const decision = gateDecision(profile, role, allow, roles, hosts, host);
  if (decision === "unavailable") return <NotAvailablePanel />;
  if (decision === "mismatch") {
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

export function ProfileGate({ allow, roles, hosts, children }: ProfileGateProps) {
  const { profile, role } = useNodeIdentity();
  const host = useNodeHostKind();
  return (
    <ProfileGateView profile={profile} role={role} host={host} allow={allow} roles={roles} hosts={hosts}>
      {children}
    </ProfileGateView>
  );
}

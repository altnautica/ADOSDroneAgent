import { Ban } from "lucide-react";
import { Link } from "react-router-dom";

import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { useStatus } from "@/hooks/use-status";
import type { GroundRole, Profile } from "@/lib/types";

type AllowedProfile = "drone" | "ground_station";

interface ProfileGateProps {
  allow: AllowedProfile[];
  roles?: GroundRole[];
  children: React.ReactNode;
}

function profileLabel(p: AllowedProfile | Profile): string {
  if (p === "drone") return "Drone";
  if (p === "ground_station") return "Ground station";
  if (p === "auto") return "Auto-detecting";
  return "Unknown";
}

function roleLabel(r: GroundRole): string {
  return r === "direct" ? "Direct" : r === "relay" ? "Relay" : "Receiver";
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
  const expected = allow.map(profileLabel).join(" or ");
  const expectedRole = roles?.length
    ? ` (${roles.map(roleLabel).join(" / ")} role)`
    : "";

  return (
    <div className="max-w-md space-y-4 py-12">
      <div className="inline-flex items-center justify-center h-12 w-12 rounded-lg bg-muted">
        <Ban className="h-5 w-5 text-muted-foreground" />
      </div>
      <h1 className="text-xl font-semibold tracking-tight">
        Not available on this profile
      </h1>
      <p className="text-sm text-muted-foreground">
        This page is for the {expected} profile{expectedRole}. This node is
        currently the {profileLabel(current)} profile
        {current === "ground_station" ? ` (${roleLabel(currentRole)} role)` : ""}
        . Switch profiles from{" "}
        <Link to="/settings/profile" className="underline">
          Settings &rarr; Profile
        </Link>{" "}
        or re-run the setup wizard.
      </p>
      <Button asChild variant="outline" size="sm">
        <Link to="/">Back to Home</Link>
      </Button>
    </div>
  );
}

function LoadingPlaceholder() {
  return (
    <div className="p-6 space-y-3">
      <Skeleton className="h-6 w-40" />
      <Skeleton className="h-4 w-64" />
      <Skeleton className="h-32 w-full" />
    </div>
  );
}

export function ProfileGate({ allow, roles, children }: ProfileGateProps) {
  const status = useStatus();
  const profile: Profile = (status.data?.profile as Profile) ?? "auto";
  const role: GroundRole = status.data?.ground_role ?? "direct";

  if (status.isPending && !status.data) {
    return <LoadingPlaceholder />;
  }

  if (profile === "auto" || profile === "unknown") {
    return <LoadingPlaceholder />;
  }

  if (!allow.includes(profile as AllowedProfile)) {
    return (
      <ProfileMismatchPanel
        current={profile}
        currentRole={role}
        allow={allow}
        roles={roles}
      />
    );
  }

  if (
    profile === "ground_station" &&
    roles &&
    roles.length > 0 &&
    !roles.includes(role)
  ) {
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

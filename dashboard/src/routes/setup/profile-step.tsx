import { useEffect, useState } from "react";

import { summarizeHardware } from "@/components/panels/hardware-item-list";
import { Card, CardContent } from "@/components/ui/card";
import { RadioCardGroup } from "@/components/ui/radio-card-group";
import { useHeartbeat } from "@/hooks/use-heartbeat";
import { useStatus } from "@/hooks/use-status";
import type { GroundRole, Profile, SetupStatus } from "@/lib/types";

interface Props {
  onChange: (payload: {
    profile: Profile;
    ground_role?: GroundRole;
    isValid: boolean;
  }) => void;
}

const PROFILE_OPTIONS: ReadonlyArray<{
  value: Profile;
  label: string;
  description: string;
}> = [
  {
    value: "drone",
    label: "Drone",
    description:
      "Air-side companion. Routes MAVLink, streams video, talks to the cloud relay, and pairs with a ground station.",
  },
  {
    value: "ground_station",
    label: "Ground station",
    description:
      "Ground-side receiver. Hosts WFB-rx, displays the video feed, optionally bridges to other ground nodes via mesh.",
  },
];

const ROLE_OPTIONS: ReadonlyArray<{
  value: GroundRole;
  label: string;
  description: string;
}> = [
  {
    value: "direct",
    label: "Direct",
    description:
      "Single ground node receiving the drone link directly. Simplest setup.",
  },
  {
    value: "relay",
    label: "Relay",
    description:
      "Forwards a drone link to another ground node, optionally bridges over batman-adv.",
  },
  {
    value: "receiver",
    label: "Receiver",
    description:
      "Aggregates streams from multiple relay nodes. Best for redundant deployments.",
  },
];

function suggestedFrom(status: SetupStatus | undefined): {
  profile: Profile;
  role: GroundRole;
  source: string;
} {
  const fromSuggestion = status?.profile_suggestion?.detected;
  const fromStatus = status?.profile;
  const profile: Profile = fromSuggestion ?? fromStatus ?? "drone";
  const role: GroundRole =
    status?.profile_suggestion?.ground_role_hint ??
    status?.ground_role ??
    "direct";
  return {
    profile: profile === "auto" || profile === "unknown" ? "drone" : profile,
    role,
    source: status?.profile_suggestion?.source ?? status?.profile_source ?? "unknown",
  };
}

export function ProfileStep({ onChange }: Props) {
  const status = useStatus();
  const heartbeat = useHeartbeat();

  const suggested = suggestedFrom(status.data);
  const [profile, setProfile] = useState<Profile | null>(null);
  const [role, setRole] = useState<GroundRole>(suggested.role);

  useEffect(() => {
    if (profile == null && status.data) {
      setProfile(suggested.profile);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [status.data]);

  useEffect(() => {
    onChange({
      profile: profile ?? "drone",
      ground_role: profile === "ground_station" ? role : undefined,
      isValid: profile != null,
    });
  }, [profile, role, onChange]);

  const board = heartbeat.data?.board;

  return (
    <div className="space-y-6">
      <Card>
        <CardContent className="pt-4">
          <div className="grid grid-cols-2 gap-4 text-sm">
            <div>
              <div className="text-xs text-muted-foreground uppercase tracking-wider">
                Detected board
              </div>
              <div className="font-mono">{board?.name ?? "detecting…"}</div>
            </div>
            <div>
              <div className="text-xs text-muted-foreground uppercase tracking-wider">
                RAM
              </div>
              <div className="font-mono">
                {board?.ram_mb ? `${board.ram_mb} MB` : "—"}
              </div>
            </div>
            <div>
              <div className="text-xs text-muted-foreground uppercase tracking-wider">
                Auto-detected profile
              </div>
              <div className="font-mono">
                {suggested.profile} ({suggested.source})
              </div>
            </div>
            <div>
              <div className="text-xs text-muted-foreground uppercase tracking-wider">
                Hardware check
              </div>
              <div className="font-mono">
                {(() => {
                  const items = status.data?.hardware_check?.items ?? [];
                  if (items.length === 0) return "scanning…";
                  const s = summarizeHardware(items);
                  return `${s.requiredOk} / ${s.requiredTotal} required ok`;
                })()}
              </div>
            </div>
          </div>
        </CardContent>
      </Card>

      <div>
        <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground mb-2">
          Profile
        </div>
        <RadioCardGroup
          value={profile}
          onChange={(v) => setProfile(v)}
          options={PROFILE_OPTIONS}
          columns={2}
        />
      </div>

      {profile === "ground_station" && (
        <div>
          <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground mb-2">
            Ground station role
          </div>
          <RadioCardGroup
            value={role}
            onChange={(v) => setRole(v)}
            options={ROLE_OPTIONS}
            columns={3}
          />
        </div>
      )}
    </div>
  );
}

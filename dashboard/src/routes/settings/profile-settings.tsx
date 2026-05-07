import { useEffect, useState } from "react";

import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { RiskBadge } from "@/components/settings/risk-badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { RadioCardGroup } from "@/components/ui/radio-card-group";
import { Switch } from "@/components/ui/switch";
import { useStatus } from "@/hooks/use-status";
import { ApiError } from "@/lib/api";
import {
  groundRoleFromStatus,
  postApply,
  profileFromStatus,
} from "@/lib/apply-actions";
import type { GroundRole } from "@/lib/types";

type ApplyProfile = "drone" | "ground_station";

const PROFILE_OPTIONS = [
  {
    value: "drone" as const,
    label: "Drone",
    description:
      "Air-side companion. Routes MAVLink, streams video, talks to the cloud relay.",
  },
  {
    value: "ground_station" as const,
    label: "Ground station",
    description:
      "Ground-side receiver. Hosts WFB-rx, displays the video feed, optionally bridges over mesh.",
  },
];

const ROLE_OPTIONS = [
  {
    value: "direct" as const,
    label: "Direct",
    description: "Single ground node receiving the drone link directly.",
  },
  {
    value: "relay" as const,
    label: "Relay",
    description: "Forwards a drone link to another ground node, optional batman-adv bridge.",
  },
  {
    value: "receiver" as const,
    label: "Receiver",
    description: "Aggregates streams from multiple relay nodes.",
  },
];

export function ProfileSettings() {
  const status = useStatus();

  const [profile, setProfile] = useState<ApplyProfile>("drone");
  const [role, setRole] = useState<GroundRole>("direct");
  const [autoRestart, setAutoRestart] = useState(true);
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [feedback, setFeedback] = useState<{
    kind: "ok" | "err";
    text: string;
  } | null>(null);

  useEffect(() => {
    if (status.data) {
      setProfile(profileFromStatus(status.data.profile));
      setRole(groundRoleFromStatus(status.data.ground_role));
    }
  }, [status.data]);

  const initialProfile = profileFromStatus(status.data?.profile);
  const initialRole = groundRoleFromStatus(status.data?.ground_role);
  const dirty =
    profile !== initialProfile ||
    (profile === "ground_station" && role !== initialRole);

  async function handleApply() {
    setBusy(true);
    setFeedback(null);
    try {
      const res = await postApply({
        profile: {
          profile,
          ground_role: profile === "ground_station" ? role : undefined,
          auto_restart: autoRestart,
        },
      });
      const section = res.sections.profile;
      if (res.overall && section?.ok) {
        setFeedback({
          kind: "ok",
          text: section.message || "Profile saved.",
        });
      } else {
        setFeedback({
          kind: "err",
          text: section?.message ?? "Apply failed.",
        });
      }
    } catch (err) {
      setFeedback({
        kind: "err",
        text:
          err instanceof ApiError
            ? `${err.status}: ${err.message}`
            : err instanceof Error
              ? err.message
              : String(err),
      });
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="space-y-6">
      <Card>
        <CardContent className="pt-5 pb-5 space-y-1">
          <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-wider text-muted-foreground">
            Current
            <RiskBadge tone="manual" />
          </div>
          <div className="text-sm">
            <span className="font-mono">{initialProfile}</span>
            {initialProfile === "ground_station" && (
              <>
                <span className="text-muted-foreground"> / role </span>
                <span className="font-mono">{initialRole}</span>
              </>
            )}
            <span className="text-muted-foreground">
              {" "}
              · source{" "}
            </span>
            <span className="font-mono">
              {status.data?.profile_source ?? "unknown"}
            </span>
          </div>
        </CardContent>
      </Card>

      <div>
        <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-wider text-muted-foreground mb-2">
          Profile
          <RiskBadge tone="manual" />
        </div>
        <RadioCardGroup
          value={profile}
          onChange={(v) => setProfile(v as ApplyProfile)}
          options={PROFILE_OPTIONS}
          columns={2}
        />
      </div>

      {profile === "ground_station" && (
        <div>
          <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-wider text-muted-foreground mb-2">
            Ground station role
            <RiskBadge tone="manual" />
          </div>
          <RadioCardGroup
            value={role}
            onChange={(v) => setRole(v as GroundRole)}
            options={ROLE_OPTIONS}
            columns={3}
          />
        </div>
      )}

      <Card>
        <CardContent className="pt-5 pb-5 flex items-center justify-between gap-4">
          <div>
            <div className="flex items-center gap-2 text-sm font-medium">
              Restart services after apply
              <RiskBadge tone="auto" />
            </div>
            <div className="text-xs text-muted-foreground mt-1">
              When the profile changes, dispatch a non-blocking
              ados-supervisor restart so the new profile's services come up.
            </div>
          </div>
          <Switch
            checked={autoRestart}
            onCheckedChange={setAutoRestart}
            aria-label="Restart services after apply"
          />
        </CardContent>
      </Card>

      {feedback && (
        <div
          className={`rounded-md border px-3 py-2 text-sm ${
            feedback.kind === "ok"
              ? "border-emerald-500/40 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300"
              : "border-red-500/40 bg-red-500/10 text-red-700 dark:text-red-300"
          }`}
        >
          {feedback.text}
        </div>
      )}

      <div className="flex items-center justify-end gap-3">
        {dirty && (
          <span className="text-xs text-muted-foreground">unsaved changes</span>
        )}
        <Button
          variant="default"
          disabled={!dirty || busy}
          onClick={() => setConfirmOpen(true)}
        >
          Save profile
        </Button>
      </div>

      <ConfirmDialog
        open={confirmOpen}
        onOpenChange={setConfirmOpen}
        title="Switch agent profile?"
        description={
          <div className="space-y-2">
            <div>
              The agent will reconfigure for{" "}
              <span className="font-mono font-medium">{profile}</span>
              {profile === "ground_station" && (
                <>
                  {" "}role{" "}
                  <span className="font-mono font-medium">{role}</span>
                </>
              )}
              .
            </div>
            {autoRestart && (
              <div>
                ados-supervisor will restart automatically. The dashboard may
                disconnect for a few seconds.
              </div>
            )}
          </div>
        }
        confirmLabel="Apply"
        destructive
        onConfirm={handleApply}
      />
    </div>
  );
}

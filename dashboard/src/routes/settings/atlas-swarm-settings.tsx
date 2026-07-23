import {
  ConfigEnumField,
  ConfigNumberField,
  ConfigTextField,
  ConfigToggle,
} from "@/components/settings/config-fields";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { useConfig } from "@/hooks/use-config";

const CAPTURE_PROFILE_OPTIONS = [
  {
    value: "freeform" as const,
    label: "Freeform",
    description: "No assumed path; keyframes selected on motion.",
  },
  {
    value: "orbit" as const,
    label: "Orbit",
    description: "Circling a subject.",
  },
  {
    value: "lawnmower" as const,
    label: "Lawnmower",
    description: "Parallel survey passes.",
  },
  {
    value: "inspection" as const,
    label: "Inspection",
    description: "Close, structured passes over a structure.",
  },
];

const POSE_TIER_OPTIONS = [
  {
    value: "auto" as const,
    label: "Auto",
    description: "Pick local or offload pose by board capability.",
  },
  { value: "local" as const, label: "Local", description: "Pose on this node." },
  {
    value: "offload" as const,
    label: "Offload",
    description: "Pose on a paired workstation.",
  },
  {
    value: "hybrid" as const,
    label: "Hybrid",
    description: "Mix local tracking with offloaded correction.",
  },
];

function AtlasSection() {
  const config = useConfig();
  const atlas = config.data?.atlas;

  if (!atlas) {
    return (
      <Card>
        <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
          The world model is not exposed by this agent version.
        </CardContent>
      </Card>
    );
  }

  const enabled = atlas.enabled === true;

  return (
    <div className="space-y-6">
      <Card>
        <CardContent className="pt-5 pb-5">
          <ConfigToggle
            configKey="atlas.enabled"
            label="World-model capture"
            hint="Capture pose-tagged keyframes as the node flies so a paired compute node can reconstruct a 3D world model. Off by default."
            value={atlas.enabled}
          />
        </CardContent>
      </Card>

      {enabled && (
        <>
          <Card>
            <CardContent className="pt-5 pb-5 space-y-4">
              <div className="text-sm font-semibold">Capture profile</div>
              <ConfigEnumField
                configKey="atlas.capture_profile"
                value={atlas.capture_profile}
                options={CAPTURE_PROFILE_OPTIONS}
                columns={2}
              />
            </CardContent>
          </Card>

          <Card>
            <CardContent className="pt-5 pb-5 space-y-4">
              <div>
                <div className="text-sm font-semibold">Pose tier</div>
                <p className="text-xs text-muted-foreground mt-1 leading-relaxed">
                  Where the camera pose is computed for keyframe selection.
                </p>
              </div>
              <ConfigEnumField
                configKey="atlas.pose_tier"
                value={atlas.pose_tier}
                options={POSE_TIER_OPTIONS}
                columns={2}
              />
            </CardContent>
          </Card>

          <Card>
            <CardContent className="pt-5 pb-5 space-y-5">
              <ConfigNumberField
                configKey="atlas.reconstruct_steps"
                id="atlas-steps"
                label="Reconstruction steps"
                hint="Default training steps the compute node uses when reconstructing this node's captures. Higher is more detailed and slower."
                value={atlas.reconstruct_steps}
                integer
                min={1}
              />
              <div className="border-t border-border pt-5">
                <ConfigNumberField
                  configKey="atlas.hfov_deg"
                  id="atlas-hfov"
                  label="Horizontal FOV (degrees)"
                  hint="The camera's horizontal field of view, used to derive an uncalibrated pinhole when no calibration is provided."
                  value={atlas.hfov_deg}
                  integer={false}
                  min={1}
                  max={180}
                />
              </div>
            </CardContent>
          </Card>
        </>
      )}
    </div>
  );
}

function SwarmSection() {
  const config = useConfig();
  const swarm = config.data?.swarm;

  if (!swarm) {
    return (
      <Card>
        <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
          Swarm defaults are not exposed by this agent version.
        </CardContent>
      </Card>
    );
  }

  return (
    <div className="space-y-6">
      <div className="rounded-md border border-info/40 bg-info/5 px-4 py-3 text-xs text-muted-foreground leading-relaxed">
        <span className="font-medium text-foreground">Configuration only.</span>{" "}
        These values persist to the node's config, but there is no runtime swarm
        consumer yet. They set the defaults a future swarm layer will read.
      </div>

      <Card>
        <CardContent className="pt-5 pb-5">
          <ConfigToggle
            configKey="swarm.enabled"
            label="Swarm participation"
            hint="Persist this node's intent to join a swarm. No runtime consumer acts on it yet."
            value={swarm.enabled}
          />
        </CardContent>
      </Card>

      <Card>
        <CardContent className="pt-5 pb-5 space-y-5">
          <ConfigTextField
            configKey="swarm.role"
            id="swarm-role"
            label="Role"
            hint="The node's intended swarm role (e.g. auto)."
            placeholder="auto"
            value={swarm.role}
          />
          <div className="border-t border-border pt-5">
            <ConfigTextField
              configKey="swarm.default_formation"
              id="swarm-formation"
              label="Default formation"
              hint="The default formation name (e.g. line)."
              placeholder="line"
              value={swarm.default_formation}
            />
          </div>
          <div className="border-t border-border pt-5">
            <ConfigNumberField
              configKey="swarm.default_spacing"
              id="swarm-spacing"
              label="Default spacing (metres)"
              hint="The default inter-node spacing a formation should target."
              value={swarm.default_spacing}
              integer
              min={0}
            />
          </div>
        </CardContent>
      </Card>
    </div>
  );
}

export function AtlasSwarmSettings() {
  const config = useConfig();

  if (config.isLoading) {
    return <p className="text-[11px] text-muted-foreground/70">Reading config…</p>;
  }
  if (config.isError) {
    return (
      <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
        Could not read the Atlas / swarm config from this node.
      </div>
    );
  }

  return (
    <div className="space-y-8">
      <section className="space-y-3">
        <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground">
          Atlas world model
        </div>
        <AtlasSection />
      </section>

      <section className="space-y-3">
        <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-wider text-muted-foreground">
          Swarm
          <Badge variant="default" className="font-normal">
            config only
          </Badge>
        </div>
        <SwarmSection />
      </section>
    </div>
  );
}

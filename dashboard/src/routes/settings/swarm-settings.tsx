import {
  ConfigEnumField,
  ConfigNumberField,
  ConfigTextField,
  ConfigToggle,
} from "@/components/settings/config-fields";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { useConfig } from "@/hooks/use-config";

/** The built-in formation generators. A free-text name produced no
 * formation at all, so the model now rejects anything outside this set —
 * the field offers exactly what the agent accepts. */
const FORMATION_OPTIONS = [
  {
    value: "line" as const,
    label: "Line",
    description: "Abreast, perpendicular to the heading.",
  },
  {
    value: "column" as const,
    label: "Column",
    description: "Nose to tail along the heading.",
  },
  {
    value: "wedge" as const,
    label: "Wedge",
    description: "V, anchored on the lead slot.",
  },
  {
    value: "grid" as const,
    label: "Grid",
    description: "Rows and columns around the anchor.",
  },
  {
    value: "circle" as const,
    label: "Circle",
    description: "Evenly spaced on a ring around the anchor.",
  },
];

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
          <div className="border-t border-border pt-5 space-y-2">
            <div className="text-sm font-medium">Default formation</div>
            <p className="text-xs text-muted-foreground">
              The formation preset the node starts with.
            </p>
            <ConfigEnumField
              configKey="swarm.default_formation"
              value={swarm.default_formation}
              options={FORMATION_OPTIONS}
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

export function SwarmSettings() {
  const config = useConfig();

  if (config.isLoading) {
    return <p className="text-[11px] text-muted-foreground/70">Reading config…</p>;
  }
  if (config.isError) {
    return (
      <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
        Could not read the swarm config from this node.
      </div>
    );
  }

  return (
    <section className="space-y-3">
      <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-wider text-muted-foreground">
        Swarm
        <Badge variant="default" className="font-normal">
          config only
        </Badge>
      </div>
      <SwarmSection />
    </section>
  );
}

import { ConfigEnumField, ConfigToggle } from "@/components/settings/config-fields";
import { Card, CardContent } from "@/components/ui/card";
import { useConfig } from "@/hooks/use-config";

// The legal rotation values the camera config key accepts (clockwise degrees).
const ROTATION_OPTIONS: ReadonlyArray<{
  value: string;
  label: string;
  description?: string;
}> = [
  { value: "0", label: "0°", description: "No rotation." },
  { value: "90", label: "90°", description: "Rotate clockwise by a quarter turn." },
  { value: "180", label: "180°", description: "Rotate upside down." },
  { value: "270", label: "270°", description: "Rotate counter-clockwise by a quarter turn." },
];

export function CameraSettings() {
  const config = useConfig();

  if (config.isLoading) {
    return <p className="text-[11px] text-muted-foreground/70">Reading config…</p>;
  }
  if (config.isError) {
    return (
      <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
        Could not read the camera config from this node.
      </div>
    );
  }

  const camera = config.data?.video?.camera;
  if (!camera) {
    return (
      <Card>
        <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
          Camera image settings are not exposed by this agent version.
        </CardContent>
      </Card>
    );
  }

  return (
    <div className="space-y-6">
      <Card>
        <CardContent className="pt-5 pb-5 space-y-5">
          <div>
            <div className="text-sm font-semibold">Rotation</div>
            <p className="text-xs text-muted-foreground mt-1 leading-relaxed">
              Clockwise image rotation applied by the encoder before the frame is
              compressed and sent to the live feed. The pipeline restarts to
              apply a change (the stretch is applied per-leg from the same config
              key).
            </p>
          </div>
          <ConfigEnumField
            configKey="video.camera.rotation"
            value={
              camera.rotation != null
                ? (String(camera.rotation) as "0" | "90" | "180" | "270")
                : undefined
            }
            options={ROTATION_OPTIONS}
            columns={2}
          />
          <div className="border-t border-border pt-5">
            <ConfigToggle
              configKey="video.camera.hflip"
              label="Horizontal flip"
              hint="Mirror the image left-to-right. Useful for a camera mounted facing backwards."
              value={camera.hflip}
            />
          </div>
          <div className="border-t border-border pt-5">
            <ConfigToggle
              configKey="video.camera.vflip"
              label="Vertical flip"
              hint="Mirror the image top-to-bottom. Combine with rotation to correct upside-down mounts."
              value={camera.vflip}
            />
          </div>
        </CardContent>
      </Card>
    </div>
  );
}

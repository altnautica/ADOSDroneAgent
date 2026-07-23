import { Link } from "react-router-dom";

import {
  ConfigEnumField,
  ConfigNumberField,
  ConfigToggle,
  ReadRow,
} from "@/components/settings/config-fields";
import { Card, CardContent } from "@/components/ui/card";
import { useConfig } from "@/hooks/use-config";

const BACKEND_OPTIONS: ReadonlyArray<{
  value: string;
  label: string;
  description?: string;
}> = [
  {
    value: "auto",
    label: "Auto",
    description: "Pick the best available accelerator (NPU, GPU, or CPU).",
  },
  { value: "rknn", label: "RKNN", description: "Rockchip NPU." },
  { value: "tensorrt", label: "TensorRT", description: "NVIDIA GPU." },
  {
    value: "opencv_dnn",
    label: "OpenCV DNN",
    description: "CPU inference via OpenCV.",
  },
  { value: "tflite", label: "TFLite", description: "TensorFlow Lite." },
];

export function VisionSettings() {
  const config = useConfig();

  if (config.isLoading) {
    return <p className="text-[11px] text-muted-foreground/70">Reading config…</p>;
  }
  if (config.isError) {
    return (
      <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
        Could not read the vision config from this node.
      </div>
    );
  }

  const vision = config.data?.vision;
  if (!vision) {
    return (
      <Card>
        <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
          The vision engine is not exposed by this agent version.
        </CardContent>
      </Card>
    );
  }

  const enabled = vision.enabled === true;

  return (
    <div className="space-y-6">
      <Card>
        <CardContent className="pt-5 pb-5 space-y-5">
          <ConfigToggle
            configKey="vision.enabled"
            label="On-board detection engine"
            hint="Run the detection engine on this node so it can publish detections locally. Off by default; a fresh node runs no vision until this is on."
            value={vision.enabled}
          />
        </CardContent>
      </Card>

      {enabled && (
        <>
          <Card>
            <CardContent className="pt-5 pb-5 space-y-4">
              <div>
                <div className="text-sm font-semibold">Inference backend</div>
                <p className="text-xs text-muted-foreground mt-1 leading-relaxed">
                  Which accelerator the engine runs on. Auto picks the best the
                  board advertises; the explicit choices force a specific one.
                </p>
              </div>
              <ConfigEnumField
                configKey="vision.backend"
                value={vision.backend}
                options={BACKEND_OPTIONS}
                columns={2}
              />
            </CardContent>
          </Card>

          <Card>
            <CardContent className="pt-5 pb-5 space-y-5">
              <ConfigNumberField
                configKey="vision.confidence_threshold"
                id="vision-confidence"
                label="Confidence threshold"
                hint="Minimum detection confidence to report, from 0 to 1. Higher drops weak detections."
                value={vision.confidence_threshold}
                integer={false}
                min={0}
                max={1}
              />
              <div className="border-t border-border pt-5">
                <ConfigToggle
                  configKey="vision.auto_download"
                  label="Auto-download models"
                  hint="Fetch a selected model from the registry when it is not already on disk."
                  value={vision.auto_download}
                />
              </div>
              <div className="border-t border-border pt-5">
                <ConfigNumberField
                  configKey="vision.models_cache_max_mb"
                  id="vision-cache"
                  label="Model cache cap (MB)"
                  hint="How much disk the downloaded-model cache may use before older models are pruned."
                  value={vision.models_cache_max_mb}
                  integer
                  min={0}
                />
              </div>
              {vision.models_dir && (
                <div className="border-t border-border pt-5">
                  <ReadRow label="Models directory" value={vision.models_dir} />
                </div>
              )}
            </CardContent>
          </Card>
        </>
      )}

      {/* Honest boundary: model selection + offload are managed elsewhere. */}
      <Card>
        <CardContent className="pt-5 pb-5 space-y-1.5">
          <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground">
            Managed elsewhere
          </div>
          <p className="text-xs text-muted-foreground leading-relaxed">
            The active detector model is not a config key here; it is selected in
            Mission Control's vision hub, which writes the engine's detector and
            restarts it. Where detection runs (on this node or offloaded to a
            workstation) is set on the{" "}
            <Link
              to="/settings/offload"
              className="underline underline-offset-2 hover:text-foreground"
            >
              Offload
            </Link>{" "}
            page.
          </p>
        </CardContent>
      </Card>
    </div>
  );
}

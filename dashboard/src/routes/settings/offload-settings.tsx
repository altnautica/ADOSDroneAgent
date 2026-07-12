import { useEffect, useState } from "react";

import { RiskBadge } from "@/components/settings/risk-badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { RadioCardGroup } from "@/components/ui/radio-card-group";
import { useConfig } from "@/hooks/use-config";
import { putConfigValue } from "@/lib/apply-actions";
import { toast, toastFromError } from "@/lib/toast";

type Mode = "auto" | "on" | "off";

const OFFLOAD_MODE_OPTIONS = [
  {
    value: "auto" as const,
    label: "Auto",
    description:
      "Offload the detector to a workstation when this board has no NPU and one is reachable on the LAN.",
  },
  {
    value: "on" as const,
    label: "On",
    description: "Always offload, even if a local accelerator is present.",
  },
  {
    value: "off" as const,
    label: "Off",
    description: "Never offload. Detection runs locally (or not at all).",
  },
];

const SERVING_MODE_OPTIONS = [
  {
    value: "auto" as const,
    label: "Auto",
    description: "Accept and serve offloaded perception from drones on the LAN.",
  },
  {
    value: "on" as const,
    label: "On",
    description: "Always serve offloaded perception.",
  },
  {
    value: "off" as const,
    label: "Off",
    description: "Do not serve perception offload (reconstruction still runs).",
  },
];

function asMode(v: string | undefined): Mode {
  return v === "on" || v === "off" ? v : "auto";
}

/** Drone half: where the heavy detector runs. */
function DroneOffload() {
  const config = useConfig();
  const [mode, setMode] = useState<Mode>("auto");
  const [addr, setAddr] = useState("");
  const [addrBusy, setAddrBusy] = useState(false);

  useEffect(() => {
    if (config.data) {
      setMode(asMode(config.data.perception?.offload?.enabled));
      setAddr(config.data.perception?.offload?.compute_node_addr ?? "");
    }
  }, [config.data]);

  const initialAddr = config.data?.perception?.offload?.compute_node_addr ?? "";

  async function applyMode(next: Mode) {
    const previous = mode;
    setMode(next);
    try {
      await putConfigValue("perception.offload.enabled", next);
      toast.ok(`Offload set to ${next}.`);
    } catch (err) {
      setMode(previous);
      toastFromError(err, "Could not update the offload mode.");
    }
  }

  async function applyAddr() {
    setAddrBusy(true);
    try {
      await putConfigValue("perception.offload.compute_node_addr", addr.trim());
      toast.ok(addr.trim() ? "Workstation pinned." : "Auto-discover restored.");
      config.refetch();
    } catch (err) {
      toastFromError(err, "Could not save the workstation address.");
    } finally {
      setAddrBusy(false);
    }
  }

  return (
    <div className="space-y-6">
      <div>
        <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-wider text-muted-foreground mb-2">
          Offload
          <RiskBadge tone="manual" />
        </div>
        <RadioCardGroup
          value={mode}
          onChange={(v) => applyMode(v as Mode)}
          options={OFFLOAD_MODE_OPTIONS}
          columns={3}
        />
      </div>

      <Card>
        <CardContent className="pt-5 pb-5 space-y-3">
          <div>
            <Label htmlFor="offload-addr">Pin a workstation</Label>
            <div className="text-xs text-muted-foreground mt-1">
              Leave empty to auto-discover a workstation on the LAN. Set a{" "}
              <span className="font-mono">host:port</span> to always offload to a
              specific one (a secured or cross-subnet node).
            </div>
          </div>
          <div className="flex items-center gap-3">
            <Input
              id="offload-addr"
              value={addr}
              placeholder="auto-discover"
              onChange={(e) => setAddr(e.target.value)}
              className="font-mono"
            />
            <Button
              variant="default"
              disabled={addr === initialAddr || addrBusy}
              onClick={applyAddr}
            >
              Save
            </Button>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}

/** Workstation half: whether this node serves offloaded perception + which model. */
function WorkstationServing() {
  const config = useConfig();
  const [mode, setMode] = useState<Mode>("auto");
  const [model, setModel] = useState("");
  const [modelBusy, setModelBusy] = useState(false);

  useEffect(() => {
    if (config.data) {
      setMode(asMode(config.data.perception?.serving?.enabled));
      setModel(config.data.perception?.serving?.detector_model ?? "");
    }
  }, [config.data]);

  const initialModel = config.data?.perception?.serving?.detector_model ?? "";

  async function applyMode(next: Mode) {
    const previous = mode;
    setMode(next);
    try {
      await putConfigValue("perception.serving.enabled", next);
      toast.ok(`Serving set to ${next}.`);
    } catch (err) {
      setMode(previous);
      toastFromError(err, "Could not update the serving mode.");
    }
  }

  async function applyModel() {
    setModelBusy(true);
    try {
      await putConfigValue("perception.serving.detector_model", model.trim());
      toast.ok(model.trim() ? "Detector model set." : "Default detector restored.");
      config.refetch();
    } catch (err) {
      toastFromError(err, "Could not save the detector model.");
    } finally {
      setModelBusy(false);
    }
  }

  return (
    <div className="space-y-6">
      <div>
        <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-wider text-muted-foreground mb-2">
          Offload serving
          <RiskBadge tone="manual" />
        </div>
        <RadioCardGroup
          value={mode}
          onChange={(v) => applyMode(v as Mode)}
          options={SERVING_MODE_OPTIONS}
          columns={3}
        />
      </div>

      <Card>
        <CardContent className="pt-5 pb-5 space-y-3">
          <div>
            <Label htmlFor="serving-model">Detector model</Label>
            <div className="text-xs text-muted-foreground mt-1">
              The served detector, by model id or an <span className="font-mono">.onnx</span>{" "}
              path. Leave empty for the node's default.
            </div>
          </div>
          <div className="flex items-center gap-3">
            <Input
              id="serving-model"
              value={model}
              placeholder="default"
              onChange={(e) => setModel(e.target.value)}
              className="font-mono"
            />
            <Button
              variant="default"
              disabled={model === initialModel || modelBusy}
              onClick={applyModel}
            >
              Save
            </Button>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}

export function OffloadSettings() {
  const config = useConfig();
  const profile = config.data?.agent?.profile;
  const isWorkstation = profile === "workstation" || profile === "compute";
  const isGroundStation = profile === "ground_station";

  if (isGroundStation) {
    return (
      <Card>
        <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
          Perception offload applies to drone and workstation nodes. This node is
          a ground station.
        </CardContent>
      </Card>
    );
  }

  return isWorkstation ? <WorkstationServing /> : <DroneOffload />;
}

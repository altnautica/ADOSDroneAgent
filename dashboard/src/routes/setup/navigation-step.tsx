import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Compass } from "lucide-react";

import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { apiFetch } from "@/lib/api";

export type NavRangefinderTopology = "companion" | "fc";
export type NavRangefinderDriver =
  | "tfluna_uart"
  | "garmin_lidarlite_i2c"
  | "vl53l1x_i2c";

export interface NavigationState {
  enableOpticalFlow: boolean;
  enableVio: boolean;
  enableRangefinder: boolean;
  cameraDevice: string;
  rangefinder: {
    topology: NavRangefinderTopology;
    driver: NavRangefinderDriver;
    devicePath: string;
    baud?: number;
    address?: string;
  };
}

interface Props {
  onChange: (next: NavigationState) => void;
}

interface NavCapabilities {
  vio_capable: boolean;
  csi_count: number;
  usb_uvc_count: number;
  rangefinder_ports: Array<{ bus: string; path: string }>;
}

interface NavCameraEntry {
  device: string;
  name: string;
  kind: string;
  current_role: string;
  recommended_role: string;
}

interface NavCamerasResponse {
  cameras: NavCameraEntry[];
}

const RANGEFINDER_DRIVERS: ReadonlyArray<{
  value: NavRangefinderDriver;
  label: string;
}> = [
  { value: "tfluna_uart", label: "TF-Luna (UART)" },
  { value: "garmin_lidarlite_i2c", label: "Garmin LIDAR-Lite (I2C)" },
  { value: "vl53l1x_i2c", label: "VL53L1X (I2C)" },
];

export function NavigationStep({ onChange }: Props) {
  const caps = useQuery<NavCapabilities>({
    queryKey: ["setup", "nav", "capabilities"],
    queryFn: () => apiFetch("/api/v1/setup/navigation/capabilities"),
  });
  const cams = useQuery<NavCamerasResponse>({
    queryKey: ["setup", "nav", "cameras"],
    queryFn: () => apiFetch("/api/v1/setup/navigation/cameras"),
  });

  const [enableOpticalFlow, setEnableOpticalFlow] = useState(false);
  const [enableVio, setEnableVio] = useState(false);
  const [enableRangefinder, setEnableRangefinder] = useState(false);
  const [cameraDevice, setCameraDevice] = useState<string>("");
  const [topology, setTopology] = useState<NavRangefinderTopology>("companion");
  const [driver, setDriver] = useState<NavRangefinderDriver>("tfluna_uart");
  const [devicePath, setDevicePath] = useState<string>("");
  const [baud, setBaud] = useState<string>("115200");
  const [address, setAddress] = useState<string>("");

  // Seed sensible defaults once the discovery responses land.
  useEffect(() => {
    if (cameraDevice === "" && cams.data?.cameras.length) {
      const recommended =
        cams.data.cameras.find((c) => c.recommended_role === "nav") ??
        cams.data.cameras[0];
      if (recommended) setCameraDevice(recommended.device);
    }
  }, [cams.data, cameraDevice]);

  useEffect(() => {
    if (devicePath === "" && caps.data?.rangefinder_ports.length) {
      setDevicePath(caps.data.rangefinder_ports[0].path);
    }
  }, [caps.data, devicePath]);

  // Echo state up to the wizard parent on every change.
  useEffect(() => {
    onChange({
      enableOpticalFlow,
      enableVio,
      enableRangefinder,
      cameraDevice,
      rangefinder: {
        topology,
        driver,
        devicePath,
        baud: baud ? Number(baud) : undefined,
        address: address || undefined,
      },
    });
  }, [
    enableOpticalFlow,
    enableVio,
    enableRangefinder,
    cameraDevice,
    topology,
    driver,
    devicePath,
    baud,
    address,
    onChange,
  ]);

  const vioGated = !caps.data?.vio_capable;

  return (
    <div className="space-y-6">
      <Card>
        <CardContent className="pt-4 space-y-2">
          <h3 className="text-sm font-medium flex items-center gap-2">
            <Compass className="h-4 w-4 text-info" />
            GPS-denied navigation (optional)
          </h3>
          <p className="text-sm text-muted-foreground">
            Enable optical flow, visual inertial odometry, or a rangefinder so
            this drone can navigate without GPS. Each toggle is independent and
            the whole step is skippable.
          </p>
          {caps.data && (
            <p className="text-xs text-muted-foreground font-mono">
              cameras: {caps.data.csi_count} CSI + {caps.data.usb_uvc_count} USB
              {" · "}rangefinder ports: {caps.data.rangefinder_ports.length}
              {" · "}VIO capable: {caps.data.vio_capable ? "yes" : "no"}
            </p>
          )}
        </CardContent>
      </Card>

      <Card>
        <CardContent className="pt-4 space-y-3">
          <Toggle
            label="Enable optical flow"
            description="Downward-facing camera + IMU for low-altitude position hold."
            checked={enableOpticalFlow}
            onChange={setEnableOpticalFlow}
          />
          <Toggle
            label="Enable VIO (visual inertial odometry)"
            description={
              vioGated
                ? "This board does not declare VIO support. Toggle anyway to override."
                : "Forward-facing camera + IMU for full 6-DoF position estimation."
            }
            checked={enableVio}
            onChange={setEnableVio}
          />
          <Toggle
            label="Enable rangefinder"
            description="Hard altitude reference for landing, terrain following, and obstacle avoidance."
            checked={enableRangefinder}
            onChange={setEnableRangefinder}
          />
        </CardContent>
      </Card>

      {(enableOpticalFlow || enableVio) && (
        <Card>
          <CardContent className="pt-4 space-y-2">
            <Label htmlFor="nav-camera">Navigation camera</Label>
            <select
              id="nav-camera"
              className="w-full border border-border rounded px-2 py-1 bg-background text-sm"
              value={cameraDevice}
              onChange={(e) => setCameraDevice(e.target.value)}
            >
              <option value="">— select a camera —</option>
              {cams.data?.cameras.map((c) => (
                <option key={c.device} value={c.device}>
                  {c.name} ({c.kind}) — {c.device}
                  {c.recommended_role === "nav" ? "  ★" : ""}
                </option>
              ))}
            </select>
          </CardContent>
        </Card>
      )}

      {enableRangefinder && (
        <Card>
          <CardContent className="pt-4 space-y-3">
            <div className="space-y-1.5">
              <Label>Rangefinder topology</Label>
              <div className="flex gap-4">
                <RadioRow
                  name="topology"
                  value="companion"
                  current={topology}
                  onChange={(v) => setTopology(v as NavRangefinderTopology)}
                  label="Wired to companion"
                />
                <RadioRow
                  name="topology"
                  value="fc"
                  current={topology}
                  onChange={(v) => setTopology(v as NavRangefinderTopology)}
                  label="Wired to flight controller"
                />
              </div>
            </div>

            <div className="space-y-1.5">
              <Label htmlFor="nav-driver">Driver</Label>
              <select
                id="nav-driver"
                className="w-full border border-border rounded px-2 py-1 bg-background text-sm"
                value={driver}
                onChange={(e) => setDriver(e.target.value as NavRangefinderDriver)}
              >
                {RANGEFINDER_DRIVERS.map((d) => (
                  <option key={d.value} value={d.value}>
                    {d.label}
                  </option>
                ))}
              </select>
            </div>

            <div className="space-y-1.5">
              <Label htmlFor="nav-device-path">Device path</Label>
              <Input
                id="nav-device-path"
                value={devicePath}
                onChange={(e) => setDevicePath(e.target.value)}
                placeholder="/dev/ttyS3 or /dev/i2c-1"
              />
            </div>

            {driver === "tfluna_uart" && (
              <div className="space-y-1.5">
                <Label htmlFor="nav-baud">UART baud rate</Label>
                <Input
                  id="nav-baud"
                  value={baud}
                  onChange={(e) => setBaud(e.target.value.replace(/\D/g, ""))}
                  placeholder="115200"
                />
              </div>
            )}

            {(driver === "garmin_lidarlite_i2c" || driver === "vl53l1x_i2c") && (
              <div className="space-y-1.5">
                <Label htmlFor="nav-i2c-address">I2C address (hex)</Label>
                <Input
                  id="nav-i2c-address"
                  value={address}
                  onChange={(e) => setAddress(e.target.value)}
                  placeholder="0x62"
                />
              </div>
            )}
          </CardContent>
        </Card>
      )}

      <p className="text-xs text-muted-foreground">
        Skip this step to leave navigation off. You can configure it later from
        Settings → Plugins.
      </p>
    </div>
  );
}

interface ToggleProps {
  label: string;
  description: string;
  checked: boolean;
  onChange: (next: boolean) => void;
}

function Toggle({ label, description, checked, onChange }: ToggleProps) {
  return (
    <div className="flex items-start justify-between gap-4">
      <div className="space-y-0.5">
        <h4 className="text-sm font-medium">{label}</h4>
        <p className="text-xs text-muted-foreground">{description}</p>
      </div>
      <label className="inline-flex items-center cursor-pointer gap-2 pt-0.5">
        <input
          type="checkbox"
          className="h-4 w-4 rounded border-border accent-primary"
          checked={checked}
          onChange={(e) => onChange(e.target.checked)}
        />
        <span className="text-sm">enable</span>
      </label>
    </div>
  );
}

interface RadioRowProps {
  name: string;
  value: string;
  current: string;
  onChange: (v: string) => void;
  label: string;
}

function RadioRow({ name, value, current, onChange, label }: RadioRowProps) {
  return (
    <label className="inline-flex items-center gap-2 text-sm cursor-pointer">
      <input
        type="radio"
        name={name}
        value={value}
        checked={current === value}
        onChange={() => onChange(value)}
        className="h-4 w-4 accent-primary"
      />
      <span>{label}</span>
    </label>
  );
}

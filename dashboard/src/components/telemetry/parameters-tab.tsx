import { MspSettingsTab } from "@/components/fc/msp-settings-tab";
import { useHeartbeat } from "@/hooks/use-heartbeat";
import { useSnapshot } from "@/hooks/use-snapshot";
import { resolveFirmwareType } from "@/lib/fc-firmware";

import { MavlinkParams } from "./mavlink-params";

export function ParametersTab() {
  // Route by FC family. Betaflight and iNav have no `/api/params` cache (the
  // agent is a byte-pipe for MSP), so they read settings from the FC directly
  // over the transparent MSP proxy; ArduPilot / PX4 use the agent's parameter
  // cache. The dispatch component reads only the identity so its hooks stay
  // unconditional.
  const hb = useHeartbeat();
  const snap = useSnapshot();
  const firmwareType = resolveFirmwareType(hb.data?.fcFirmware, snap.data?.fc?.vehicle);
  if (firmwareType === "betaflight" || firmwareType === "inav") {
    return (
      <MspSettingsTab
        firmware={firmwareType}
        firmwareVersion={snap.data?.fc?.firmware ?? undefined}
      />
    );
  }
  return <MavlinkParams />;
}

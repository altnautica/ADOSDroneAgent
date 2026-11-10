import { BatteryLow, BatteryWarning, Satellite, SignalZero, VideoOff, WifiOff } from "lucide-react";
import { describe, expect, it } from "vitest";

import { computeAlerts, type AlertInputs } from "@/lib/alerts";

function inputs(over: Partial<AlertInputs> = {}): AlertInputs {
  return { status: null, telemetry: null, battery: null, msSinceLive: 0, video: "live", ...over };
}

describe("cockpit alerts", () => {
  it("carry the shared rules' ids, each with its icon, worst first", () => {
    const alerts = computeAlerts(
      inputs({
        telemetry: { armed: true, battery: { voltage: 14, current: null, remaining: 5, temperature: null }, gps: { fix_type: 2, satellites: 5, eph: null, epv: null } },
        video: "frozen",
        status: { network: { uplink_type: "wifi", uplink_reachable: false } } as AlertInputs["status"],
      }),
    );
    expect(alerts.map((a) => [a.id, a.level, a.icon])).toEqual([
      ["batt_crit", "critical", BatteryWarning],
      ["gps_no_3d", "warning", Satellite],
      ["video_frozen", "warning", VideoOff],
      ["uplink", "advisory", WifiOff],
    ]);
    expect(computeAlerts(inputs({ telemetry: { armed: true }, msSinceLive: 12_000 }))[0]).toMatchObject({ id: "link_lost", icon: SignalZero });
    expect(
      computeAlerts(inputs({ telemetry: { battery: { voltage: 15, current: null, remaining: 18, temperature: null } } }))[0],
    ).toMatchObject({ id: "batt_low", icon: BatteryLow });
  });
});

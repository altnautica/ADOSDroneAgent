import { describe, expect, it } from "vitest";

import { fcPrearmItem, storageItem } from "@/lib/cockpit-flow";
import { pillAlerts, pillSummary } from "@/shared/hud/status-pill";
import { useStatusPillStore } from "@/stores/status-pill-store";

const none = { alerts: [], readiness: [], prearm: [] as string[], storagePct: 40 };
const lowBattery = { id: "batt_low", level: "warning" as const, title: "Battery low" };
const linkLost = { id: "link_lost", level: "critical" as const, title: "Vehicle link lost" };

describe("status pill acknowledgement", () => {
  it("flashes on a new alert until the list is opened (acknowledged)", () => {
    const alerts = pillAlerts({ ...none, alerts: [lowBattery] });
    expect(pillSummary(alerts, useStatusPillStore.getState().acked).flashing).toBe(true);
    useStatusPillStore.getState().ack(alerts.map((x) => x.id));
    expect(pillSummary(alerts, useStatusPillStore.getState().acked).flashing).toBe(false);
    const more = pillAlerts({ ...none, alerts: [lowBattery, linkLost] });
    expect(pillSummary(more, useStatusPillStore.getState().acked).flashing).toBe(true);
  });
});

describe("pre-flight checklist: flight controller checks and storage", () => {
  it("fails while the FC reported a PreArm text, passes when it reported none, absent while unknown", () => {
    expect(fcPrearmItem(undefined)).toBeNull();
    expect(fcPrearmItem([])).toEqual({ id: "fc_prearm", label: "Flight controller checks", state: "pass" });
    expect(fcPrearmItem(["PreArm: Gyros inconsistent", "PreArm: RC not calibrated"])).toEqual({
      id: "fc_prearm",
      label: "Flight controller: RC not calibrated",
      state: "fail",
    });
  });

  it("grades storage at the spec's 90 and 97 percent, as the status pill does", () => {
    expect(storageItem(null)).toEqual({ id: "storage", label: "On-board storage (not reported)", state: "warn" });
    expect(storageItem(50)).toEqual({ id: "storage", label: "On-board storage 50% used", state: "pass" });
    expect(storageItem(89).state).toBe("pass");
    expect(storageItem(90).state).toBe("warn");
    expect(storageItem(96).state).toBe("warn");
    expect(storageItem(97).state).toBe("fail");
  });
});

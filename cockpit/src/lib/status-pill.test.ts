import { Battery } from "lucide-react";
import { describe, expect, it } from "vitest";

import type { CockpitAlert } from "@/lib/alerts";
import { fcPrearmItem, storageItem } from "@/lib/cockpit-flow";
import { pillAlerts, pillSummary } from "@/lib/status-pill";
import { useStatusPillStore } from "@/stores/status-pill-store";

const none = { alerts: [], readiness: [], prearm: [] as string[], storagePct: 40 };
const lowBattery: CockpitAlert = { id: "battery", level: "warning", title: "Battery low", icon: Battery };
const linkLost: CockpitAlert = { id: "link-lost", level: "critical", title: "Vehicle link lost", icon: Battery };

describe("status pill", () => {
  it("reads All systems OK in the ok colour with nothing active", () => {
    const s = pillSummary(pillAlerts(none), new Set());
    expect(s).toMatchObject({ severity: "ok", text: "All systems OK", count: 0, flashing: false });
  });

  it("shows the one alert's text, or the count, coloured by the worst", () => {
    const one = pillSummary(pillAlerts({ ...none, alerts: [lowBattery] }), new Set());
    expect(one).toMatchObject({ severity: "warning", text: "Battery low", count: 1 });
    const many = pillSummary(
      pillAlerts({ ...none, alerts: [lowBattery, linkLost], prearm: ["PreArm: Compass not calibrated"] }),
      new Set(),
    );
    expect(many).toMatchObject({ severity: "critical", text: "3 warnings", count: 3 });
    expect(many.alerts[0].source).toBe("link");
  });

  it("collects pilot readiness, FC pre-arm and storage, never an advisory", () => {
    const a = pillAlerts({
      alerts: [{ id: "uplink", level: "advisory", title: "Uplink offline", icon: Battery }],
      readiness: [
        { id: "gps", ok: false, severity: "block", msg: "No GNSS fix" },
        { id: "ekf", ok: true, severity: "block", msg: "" },
      ],
      prearm: ["PreArm: Throttle below failsafe"],
      storagePct: 96,
    });
    expect(a.map((x) => x.source).sort()).toEqual(["fc_prearm", "pilot_readiness", "storage"]);
    expect(a.find((x) => x.source === "fc_prearm")!.text).toBe("Flight controller: Throttle below failsafe");
    expect(a.find((x) => x.source === "storage")!.severity).toBe("critical");
  });

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

  it("grades storage at 85 and 95 percent", () => {
    expect(storageItem(null).state).toBe("warn");
    expect(storageItem(50).state).toBe("pass");
    expect(storageItem(90).state).toBe("warn");
    expect(storageItem(95).state).toBe("fail");
  });
});

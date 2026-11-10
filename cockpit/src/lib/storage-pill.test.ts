// The cockpit pill's storage item for each golden status frame. The
// dashboard runs the same cases (`dashboard/src/lib/home-status.test.ts`),
// so both surfaces list the same items for the same frame.

import { describe, expect, it } from "vitest";

import { droneStatusFromFull } from "@/lib/api";
import type { StatusFull } from "@/shared/agent-api.generated";
import cases from "@/shared/hud/fixtures/storage-cases.json";
import { pillAlerts } from "@/shared/hud/status-pill";

describe("cockpit status pill storage", () => {
  for (const c of cases) {
    it(c.name, () => {
      // A drone's status comes through the full-status mapping; a ground
      // station's `/api/v1/ground-station/status` already carries system.disk_pct.
      const system =
        c.input.profile === "drone"
          ? droneStatusFromFull(c.input.full as unknown as StatusFull, null).system
          : (c.input.full as { system: { disk_pct: number } }).system;
      const items = pillAlerts({
        alerts: [],
        readiness: [],
        prearm: [],
        storagePct: system?.disk_pct ?? null,
        fcStatus: [],
        pilotAgeMs: 100,
        agentReachable: true,
      });
      expect(items.map((a) => ({ source: a.source, severity: a.severity }))).toEqual(c.expected);
    });
  }
});

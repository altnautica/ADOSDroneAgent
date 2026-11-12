// The pre-flight checklist items, built once for the pre-flight sheet and
// the Menu's Pre-flight row: the pilot's readiness items, the flight
// controller's own pre-arm checks, the vehicle link, GNSS, battery and home
// from the telemetry, the controller (when ADOS Pilot flies the sticks) and
// on-board storage. A failing check reads as its failure.

import { useEffect, useState } from "react";

import { usePadStatus } from "@/hooks/use-gamepad";
import { useNow } from "@/hooks/use-now";
import { usePilot } from "@/hooks/use-pilot";
import { pilotReadinessItems } from "@/components/feed/cockpit/preflight-items";
import { fcPrearmItem, storageItem, type ChecklistItem } from "@/lib/cockpit-flow";
import { apiFetch } from "@/shared/api-fetch";
import type { AgentHeartbeat } from "@/shared/api/types";
import { preflightChecks } from "@/shared/hud/preflight";
import { specText } from "@/shared/hud/spec-text";
import { useFlightStore } from "@/stores/flight-store";
import { useStatusStore } from "@/stores/status-store";

function useStoragePct(): number | null {
  const [pct, setPct] = useState<number | null>(null);
  useEffect(() => {
    const ac = new AbortController();
    apiFetch<AgentHeartbeat>("/api/status", { signal: ac.signal })
      .then((s) => setPct(typeof s.health?.disk_percent === "number" ? s.health.disk_percent : null))
      .catch(() => setPct(null));
    return () => ac.abort();
  }, []);
  return pct;
}

export function usePreflightItems(): ChecklistItem[] {
  const stamped = usePilot();
  // Ages the pilot's items out with no new frame to re-render them.
  const now = useNow(stamped ? 500 : null);
  const pilot = stamped?.data ?? null;
  const telemetry = useFlightStore((s) => s.telemetry);
  const live = useFlightStore((s) => s.live);
  const battery = useStatusStore((s) => s.battery);
  const pad = usePadStatus();
  const storage = useStoragePct();

  const items: ChecklistItem[] = pilotReadinessItems(stamped, now);
  const prearm = fcPrearmItem(telemetry?.prearm);
  if (prearm) items.push(prearm);
  for (const c of preflightChecks({ telemetry, battery, live })) {
    items.push({ id: c.id, label: c.ok ? c.label : c.fail, state: c.ok ? "pass" : c.id === "link" || c.id === "battery" ? "fail" : "warn" });
  }
  if (pilot?.controlMode === "ados") {
    items.push({
      id: "controller",
      label: pad.connected
        ? pad.name
          ? specText("flow.preflight.controller_named", { name: pad.name })
          : specText("flow.preflight.controller")
        : specText("flow.preflight.controller_optional"),
      state: pad.connected ? "pass" : "warn",
    });
  }
  items.push(storageItem(storage));
  return items;
}

/** The Menu row's summary: passed of total, and the worst state. */
export function preflightTally(items: readonly ChecklistItem[]): { passed: number; total: number; state: ChecklistItem["state"] } {
  const passed = items.filter((i) => i.state === "pass").length;
  const state = items.some((i) => i.state === "fail") ? "fail" : items.some((i) => i.state === "warn") ? "warn" : "pass";
  return { passed, total: items.length, state };
}

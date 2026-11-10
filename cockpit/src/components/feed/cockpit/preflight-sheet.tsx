// The pre-flight checklist, which gates the first arm of a session: the
// pilot's readiness items, the flight controller's own pre-arm checks (its
// PreArm/Arm texts from the last 10 s), the vehicle link, GNSS, battery and
// home from the telemetry, the controller (when ADOS Pilot flies the sticks)
// and on-board storage. A failing blocker keeps "Checks done" disabled;
// warnings are shown and accepted by passing. A stale pilot state's items
// read not reported and fail, so a frozen frame never passes the first arm.

import { useEffect, useState } from "react";
import { CircleAlert, CircleCheck, CircleX } from "lucide-react";

import { Sheet } from "@/components/ui/sheet";
import { usePadStatus } from "@/hooks/use-gamepad";
import { useNow } from "@/hooks/use-now";
import { usePilot } from "@/hooks/use-pilot";
import { checklistPassable, fcPrearmItem, storageItem, type ChecklistItem } from "@/lib/cockpit-flow";
import { preflightChecks } from "@/shared/hud/preflight";
import { apiFetch } from "@/shared/api-fetch";
import type { AgentHeartbeat } from "@/shared/api/types";
import { specText } from "@/shared/hud/spec-text";
import { useFlightStore } from "@/stores/flight-store";
import { usePreflightStore } from "@/stores/preflight-store";
import { useStatusStore } from "@/stores/status-store";

import { pilotReadinessItems } from "./preflight-items";

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

export function PreflightSheet() {
  const pass = usePreflightStore((s) => s.pass);
  const dismiss = usePreflightStore((s) => s.dismiss);
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
    items.push({ id: c.id, label: c.label, state: c.ok ? "pass" : c.id === "link" || c.id === "battery" ? "fail" : "warn" });
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

  const ok = checklistPassable(items);
  return (
    <Sheet
      title={specText("flow.preflight.title")}
      onDismiss={dismiss}
      footer={
        <button
          type="button"
          onClick={pass}
          disabled={!ok}
          data-testid="preflight-pass"
          className="min-h-[56px] w-full rounded-lg border border-primary bg-primary/25 text-[0.95rem] font-semibold disabled:opacity-45"
        >
          {specText(ok ? "flow.preflight.done" : "flow.preflight.fix")}
        </button>
      }
    >
      <ul className="flex flex-col gap-[0.25rem] py-[0.4rem]" data-testid="preflight-list">
        {items.map((i) => (
          <li key={i.id} className="flex min-h-[40px] items-center gap-[0.5rem] text-[0.85rem]">
            {i.state === "pass" ? (
              <CircleCheck className="h-[1.1rem] w-[1.1rem] shrink-0 text-ok" aria-label="Pass" />
            ) : i.state === "warn" ? (
              <CircleAlert className="h-[1.1rem] w-[1.1rem] shrink-0 text-warn" aria-label="Warning" />
            ) : (
              <CircleX className="h-[1.1rem] w-[1.1rem] shrink-0 text-err" aria-label="Fail" />
            )}
            <span>{i.label}</span>
          </li>
        ))}
      </ul>
    </Sheet>
  );
}

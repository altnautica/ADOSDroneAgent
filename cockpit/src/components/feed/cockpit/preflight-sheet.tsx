// The pre-flight checklist, which gates the first arm of a session: the
// pilot's readiness items, the flight controller's own pre-arm checks (its
// PreArm/Arm texts from the last 10 s), the vehicle link, GNSS, battery and
// home from the telemetry, the controller (when ADOS Pilot flies the sticks)
// and on-board storage (`usePreflightItems`). A failing blocker keeps "Checks
// done" disabled; warnings are shown and accepted by passing. A stale pilot
// state's items read not reported and fail, so a frozen frame never passes
// the first arm.

import { CircleAlert, CircleCheck, CircleX } from "lucide-react";

import { Sheet } from "@/components/ui/sheet";
import { usePreflightItems } from "@/hooks/use-preflight-items";
import { checklistPassable } from "@/lib/cockpit-flow";
import { specText } from "@/shared/hud/spec-text";
import { usePreflightStore } from "@/stores/preflight-store";

export function PreflightSheet() {
  const pass = usePreflightStore((s) => s.pass);
  const dismiss = usePreflightStore((s) => s.dismiss);
  const items = usePreflightItems();

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
              <CircleCheck className="h-[1.1rem] w-[1.1rem] shrink-0 text-ok" aria-label={specText("setup.result.pass")} />
            ) : i.state === "warn" ? (
              <CircleAlert className="h-[1.1rem] w-[1.1rem] shrink-0 text-warn" aria-label={specText("setup.result.warn")} />
            ) : (
              <CircleX className="h-[1.1rem] w-[1.1rem] shrink-0 text-err" aria-label={specText("setup.result.fail")} />
            )}
            <span>{i.label}</span>
          </li>
        ))}
      </ul>
    </Sheet>
  );
}

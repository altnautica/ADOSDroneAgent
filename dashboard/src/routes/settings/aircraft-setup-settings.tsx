// Settings › Aircraft setup: the agent-driven flight-controller setup wizard
// (calibrations, initial parameters, the ADOS failsafe profile, then the
// in-air tuning), rendered from the agent's `/api/v1/fc-setup` step state.
// The dashboard is the primary setup surface; the cockpit renders the same
// wizard for the kiosk and the field. The pilot's readiness (the same nav
// status the flight chip reads) marks the cards the flight controller says
// need a fix. `?step=<id>` (an alert's "Open setup") opens that step, each
// time the link is followed.

import { useMemo, useState } from "react";
import { useLocation, useNavigate } from "react-router-dom";

import { navPilot } from "@/hooks/use-flight-chip";
import { useMediaQuery } from "@/hooks/use-media-query";
import { useNavStatus } from "@/hooks/use-nav-status";
import { useFcSetup } from "@/shared/setup-wizard/use-fc-setup";
import { WizardView, type OpenStepRequest } from "@/shared/setup-wizard/wizard-view";

export function AircraftSetupSettings() {
  const setup = useFcSetup();
  const nav = useNavStatus();
  const readiness = useMemo(() => navPilot(nav.data)?.readiness ?? null, [nav.data]);
  // Below md the split step card is too narrow: stack it, as the cockpit
  // does in portrait.
  const wide = useMediaQuery("(min-width: 768px)");
  const location = useLocation();
  const navigate = useNavigate();
  // One open request per navigation that carries `?step=`.
  const [open, setOpen] = useState<{ key: string | null; seq: number; request: OpenStepRequest | null }>({
    key: null,
    seq: 0,
    request: null,
  });
  if (open.key !== location.key) {
    const step = new URLSearchParams(location.search).get("step");
    const seq = open.seq + 1;
    setOpen({ key: location.key, seq, request: step ? { step, seq } : null });
  }
  return (
    <div className="px-4 py-4 lg:px-6">
      <WizardView
        setup={setup}
        layout={wide ? "split" : "stacked"}
        readiness={readiness}
        openRequest={open.request}
        // Drop the handled `?step=` so a reload does not open it again.
        onOpenHandled={() => void navigate(location.pathname, { replace: true })}
      />
    </div>
  );
}

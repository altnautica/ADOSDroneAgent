// Settings › Aircraft setup: the agent-driven flight-controller setup wizard
// (calibrations, initial parameters, the ADOS failsafe profile, then the
// in-air tuning), rendered from the agent's `/api/v1/fc-setup` step state.
// The dashboard is the primary setup surface; the cockpit renders the same
// wizard for the kiosk and the field.

import { useFcSetup } from "@/shared/setup-wizard/use-fc-setup";
import { WizardView } from "@/shared/setup-wizard/wizard-view";

export function AircraftSetupSettings() {
  const setup = useFcSetup();
  return (
    <div className="px-4 py-4 lg:px-6">
      <WizardView setup={setup} layout="split" />
    </div>
  );
}

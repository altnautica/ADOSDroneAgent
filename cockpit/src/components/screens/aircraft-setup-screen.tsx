// Settings › Aircraft setup on the cockpit: the agent-driven flight-controller
// setup wizard (the same overview and guided flow the dashboard uses), opened
// from Settings, from the flight-state chip's NOT READY "Set up aircraft"
// link, and on the HDMI kiosk. Drone only: a ground station has no setup
// engine. A television or the kiosk scales it by the spec's 1.33, always
// draws the focus ring (the D-pad drives it there) and gets the "Step k of n"
// bar instead of the side stepper.

import { useState } from "react";
import { useTranslation } from "react-i18next";

import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { useWidthClass } from "@/hooks/use-width-class";
import { takePendingSetupStep } from "@/lib/aircraft-setup";
import { gridMeasures } from "@/lib/cockpit-layout";
import "@/shared/i18n";
import { useFcSetup } from "@/shared/setup-wizard/use-fc-setup";
import { WizardView } from "@/shared/setup-wizard/wizard-view";
import { useNavStore } from "@/stores/nav-store";

export function AircraftSetupScreen() {
  const { t } = useTranslation();
  const setup = useFcSetup();
  // The step an alert's "Open setup" asked for: read once, opened as if its
  // card was tapped.
  const [openStep] = useState(takePendingSetupStep);
  const back = useNavStore((s) => s.back);
  const { widthClass, portrait } = useWidthClass();
  const m = gridMeasures(widthClass, false);
  return (
    <ConsoleScaffold title={t("nav.aircraftSetup")} onBack={back}>
      <WizardView
        setup={setup}
        scale={m.scale}
        focusRings={m.focusRingsAlways ? "always" : "visible"}
        layout={portrait || widthClass === "compact" ? "stacked" : "split"}
        openStep={openStep}
      />
    </ConsoleScaffold>
  );
}

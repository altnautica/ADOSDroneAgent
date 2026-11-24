// Settings › Aircraft setup on the cockpit: the agent-driven flight-controller
// setup wizard (the same overview and guided flow the dashboard uses), opened
// from Settings, from the flight-state chip's NOT READY "Set up aircraft"
// link, from an alert's "Open setup", and on the HDMI kiosk. Drone only: a
// ground station has no setup engine. Its type is 0.875 rem, so it reads at
// the size of the screen frame around it at every panel size (the root is
// the cockpit's one scale, lib/ui-scale.ts). A television or the kiosk always
// draws the focus ring (the D-pad drives it there); the wizard picks the
// side stepper or the "Step k of n" bar from the width it gets.

import { useTranslation } from "react-i18next";

import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { useWidthClass } from "@/hooks/use-width-class";
import { useNavStream } from "@/hooks/use-nav-stream";
import { useFreshPilot } from "@/hooks/use-pilot";
import { clearPendingSetupStep, usePendingSetupStep } from "@/lib/aircraft-setup";
import { gridMeasures } from "@/lib/cockpit-layout";
import "@/shared/i18n";
import { useFcSetup } from "@/shared/setup-wizard/use-fc-setup";
import { WizardView } from "@/shared/setup-wizard/wizard-view";
import { useNavStore } from "@/stores/nav-store";

export function AircraftSetupScreen() {
  const { t } = useTranslation();
  const setup = useFcSetup();
  // The step an alert's "Open setup" asked for, live: a request made while
  // this screen is already open opens its step too.
  const openRequest = usePendingSetupStep();
  // The pilot's readiness marks the cards the flight controller says need a
  // fix. The Feed holds the nav stream only while it shows, so this screen
  // holds its own (one screen shows at a time).
  useNavStream(true);
  const readiness = useFreshPilot()?.readiness ?? null;
  const back = useNavStore((s) => s.back);
  const { widthClass, portrait } = useWidthClass();
  const m = gridMeasures(widthClass, false);
  return (
    <ConsoleScaffold title={t("nav.aircraftSetup")} onBack={back}>
      <WizardView
        setup={setup}
        baseSize="0.875rem"
        focusRings={m.focusRingsAlways ? "always" : "visible"}
        layout={portrait || widthClass === "compact" ? "stacked" : "split"}
        openRequest={openRequest}
        onOpenHandled={clearPendingSetupStep}
        readiness={readiness}
      />
    </ConsoleScaffold>
  );
}

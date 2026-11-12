// Settings › Aircraft setup on the cockpit: the agent-driven flight-controller
// setup wizard (the same renderer the dashboard uses), opened from Settings,
// from the flight-state chip's NOT READY "Set up aircraft" link, and on the
// HDMI kiosk. A television or the kiosk scales it by the spec's 1.33 and
// always draws the focus ring, since the D-pad drives it there.

import { useTranslation } from "react-i18next";

import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { useWidthClass } from "@/hooks/use-width-class";
import { gridMeasures } from "@/lib/cockpit-layout";
import "@/shared/i18n";
import { useFcSetup } from "@/shared/setup-wizard/use-fc-setup";
import { WizardView } from "@/shared/setup-wizard/wizard-view";
import { useNavStore } from "@/stores/nav-store";

export function AircraftSetupScreen() {
  const { t } = useTranslation();
  const setup = useFcSetup();
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
      />
    </ConsoleScaffold>
  );
}

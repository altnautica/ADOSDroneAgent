// Capture and sensor pages. `/capture` drives the world-engine extension's
// capture session (visor); `/sensors` shows the sensor streams on a visor or
// a drone.

import { useTranslation } from "react-i18next";

import { PageShell } from "@/components/page-shell";
import { CapturePanel } from "@/components/panels/capture-panel";
import { SensorsPanel } from "@/components/panels/sensors-panel";

export function CaptureRoute() {
  const { t } = useTranslation();
  return (
    <PageShell title={t("nav.capture")} blurb={t("app.pages.captureBlurb")} maxWidth="max-w-3xl">
      <CapturePanel />
    </PageShell>
  );
}

export function SensorsRoute() {
  const { t } = useTranslation();
  return (
    <PageShell title={t("nav.sensors")} blurb={t("app.pages.sensorsBlurb")}>
      <SensorsPanel />
    </PageShell>
  );
}

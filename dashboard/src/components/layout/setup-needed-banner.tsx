// The first-connection prompt on a drone: when the attached flight controller
// has no completed ground setup (or reports a calibration pre-arm failure),
// a banner links to Settings › Aircraft setup. Dismissed for the browser
// session. Shown on the drone profile only, and not on the setup page itself.

import { Wrench } from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Link, useLocation } from "react-router-dom";

import { useNodeIdentity } from "@/hooks/use-node-identity";
import { setupNeeded } from "@/shared/setup-wizard/parse";
import { setupText } from "@/shared/setup-wizard/strings";
import { useFcSetup } from "@/shared/setup-wizard/use-fc-setup";

const DISMISS_KEY = "ados-setup-banner-dismissed";
const NEEDED_TEXT_KEY = "readiness.setup_needed";
const LINK_TEXT_KEY = "setup.wizard.entry.set_up_aircraft";

function SetupNeededPrompt() {
  const { t } = useTranslation();
  const setup = useFcSetup({ stream: false });
  const [dismissed, setDismissed] = useState(() => sessionStorage.getItem(DISMISS_KEY) === "1");
  if (dismissed || !setupNeeded(setup.status)) return null;
  return (
    <div role="status" className="flex items-center gap-2 rounded-md border border-warn/40 bg-warn/10 px-3 py-2 text-sm text-warn">
      <Wrench className="h-4 w-4 shrink-0" />
      <span className="flex-1">{setupText(NEEDED_TEXT_KEY)}</span>
      <Link to="/settings/aircraft-setup" className="font-semibold underline">
        {setupText(LINK_TEXT_KEY)}
      </Link>
      <button
        type="button"
        onClick={() => {
          sessionStorage.setItem(DISMISS_KEY, "1");
          setDismissed(true);
        }}
        className="text-xs underline"
      >
        {t("layout.banners.setupNeededDismiss")}
      </button>
    </div>
  );
}

export function SetupNeededBanner() {
  const { profile } = useNodeIdentity();
  const { pathname } = useLocation();
  if (profile !== "drone" || pathname.startsWith("/settings/aircraft-setup")) return null;
  return <SetupNeededPrompt />;
}

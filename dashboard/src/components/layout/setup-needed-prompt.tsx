// The setup-needed banner's body: reads the FC setup status and, while the
// aircraft still needs setup, links to Settings › Aircraft setup. Dismissed for
// the browser session. It brings the setup client and its spec text, so the
// banner gate loads it lazily.

import { Wrench } from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Link } from "react-router-dom";

import { setupNeeded } from "@/shared/setup-wizard/parse";
import { setupText } from "@/shared/setup-wizard/strings";
import { useFcSetup } from "@/shared/setup-wizard/use-fc-setup";

const DISMISS_KEY = "ados-setup-banner-dismissed";
const NEEDED_TEXT_KEY = "readiness.setup_needed";
const LINK_TEXT_KEY = "setup.wizard.entry.set_up_aircraft";

export function SetupNeededPrompt() {
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

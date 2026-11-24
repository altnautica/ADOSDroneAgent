// The first-connection prompt on a drone: when the attached flight controller
// has no completed ground setup (or reports a calibration pre-arm failure),
// a banner links to Settings › Aircraft setup. Shown on the drone profile only,
// and not on the setup page itself. The prompt brings the setup client and its
// spec text, so it loads after the shell.

import { lazy, Suspense } from "react";
import { useLocation } from "react-router-dom";

import { useNodeIdentity } from "@/hooks/use-node-identity";
import { ErrorBoundary } from "@/shared/error-boundary";

const SetupNeededPrompt = lazy(() =>
  import("@/components/layout/setup-needed-prompt").then((m) => ({ default: m.SetupNeededPrompt })),
);

export function SetupNeededBanner() {
  const { profile } = useNodeIdentity();
  const { pathname } = useLocation();
  if (profile !== "drone" || pathname.startsWith("/settings/aircraft-setup")) return null;
  // A prompt that cannot load is no worse than no prompt: the setup page is
  // still in Settings.
  return (
    <ErrorBoundary fallback={() => null}>
      <Suspense fallback={null}>
        <SetupNeededPrompt />
      </Suspense>
    </ErrorBoundary>
  );
}

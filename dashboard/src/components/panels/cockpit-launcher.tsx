import { ArrowRight, Fullscreen } from "lucide-react";

import { requestDocumentFullscreen } from "@/lib/fullscreen";
import { cn } from "@/lib/utils";

// Full-width launch banner that takes the operator from the console into the
// immersive full-screen cockpit at /cockpit — the same surface the attached
// HDMI panel renders. On a laptop we request true fullscreen first so the
// browser view matches the appliance; Chromium keeps fullscreen across the
// same-origin navigation, other engines drop it and the cockpit re-enters on
// the next gesture.
export function CockpitLauncher() {
  const enterCockpit = async () => {
    await requestDocumentFullscreen();
    // /cockpit is a separate agent-served bundle, not an SPA route — leave the
    // dashboard with a hard navigation rather than the client-side router.
    window.location.assign("/cockpit");
  };

  return (
    <button
      type="button"
      onClick={enterCockpit}
      aria-label="Enter the full-screen cockpit"
      className={cn(
        "group relative w-full overflow-hidden rounded-lg border text-left shadow-sm",
        "border-amber-500/30 bg-gradient-to-r from-amber-500/10 via-card to-card",
        "px-4 py-4 sm:px-5 sm:py-5 transition-colors",
        "hover:border-amber-500/50 hover:from-amber-500/[0.16]",
        "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-amber-500/50",
      )}
    >
      <div className="flex items-center gap-4">
        <div className="flex h-11 w-11 shrink-0 items-center justify-center rounded-lg bg-amber-500/15 text-amber-300 ring-1 ring-amber-500/30">
          <Fullscreen className="h-5 w-5" />
        </div>
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-1.5 text-base font-semibold tracking-tight text-foreground">
            Enter Cockpit
            <ArrowRight className="h-4 w-4 text-amber-300 transition-transform group-hover:translate-x-0.5" />
          </div>
          <p className="mt-0.5 text-sm text-muted-foreground">
            Immersive full-screen piloting &amp; video — works on this laptop or
            the ground-station touchscreen.
          </p>
        </div>
        <span className="hidden shrink-0 items-center gap-1.5 rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-1.5 text-xs font-medium text-amber-200 sm:inline-flex">
          Launch
        </span>
      </div>
    </button>
  );
}

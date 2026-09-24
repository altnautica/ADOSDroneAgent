import { useCallback, useEffect, useState, type ReactNode } from "react";

import { verifyAccess } from "@/lib/access";
import { setAuthRequiredHandler } from "@/lib/api";
import { fetchPinStatus, type PinStatus } from "@/lib/pin";
import { PinSplash } from "./PinSplash";

type GateState = "checking" | "ok" | "locked";

/** A minimal branded loading screen while the gate probes, so the app never
 * flashes behind the splash. */
function GateChecking() {
  return (
    <div className="fixed inset-0 z-[200] flex items-center justify-center bg-background">
      <div className="flex items-center gap-2 opacity-70">
        <img src="/brand.svg" alt="" className="h-7 w-7 rounded-md" />
        <span className="text-base font-semibold tracking-tight">ADOS</span>
      </div>
    </div>
  );
}

/**
 * Gates the dashboard behind the PIN splash when a paired agent is reached
 * off-box without a credential.
 *
 * On mount it probes a gated route (`verifyAccess`): authorized renders the
 * app; a refused probe reads the PIN status and shows the branded splash —
 * which already knows how to offer *setting* a PIN when the node has none yet.
 * When a panel request is refused mid-session the gate re-probes in the
 * background, with the app still mounted: a refusal of that one request (a
 * denied plugin capability, a relay-forbidden path) leaves the operator where
 * they are, and only a probe the agent also refuses locks the dashboard.
 */
export function DashboardAccessGate({ children }: { children: ReactNode }) {
  const [state, setState] = useState<GateState>("checking");
  const [pinStatus, setPinStatus] = useState<PinStatus | null>(null);

  const probe = useCallback(async () => {
    if ((await verifyAccess()) === "ok") {
      setState("ok");
      return;
    }
    try {
      setPinStatus(await fetchPinStatus());
    } catch {
      setPinStatus(null);
    }
    setState("locked");
  }, []);

  useEffect(() => {
    void probe();
  }, [probe]);

  useEffect(() => {
    setAuthRequiredHandler(() => {
      void probe();
    });
    return () => setAuthRequiredHandler(null);
  }, [probe]);

  if (state === "checking") return <GateChecking />;
  if (state === "locked") {
    return (
      <PinSplash
        status={pinStatus}
        onUnlocked={() => {
          setState("checking");
          void probe();
        }}
      />
    );
  }
  return <>{children}</>;
}

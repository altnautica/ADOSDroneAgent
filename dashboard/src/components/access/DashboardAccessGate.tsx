import { useCallback, useEffect, useState, type ReactNode } from "react";

import { ApiError, apiFetch, setAuthRequiredHandler } from "@/lib/api";
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
 * On mount (and whenever `apiFetch` reports a data-plane 401 mid-session) it
 * probes a gated route: a `200` means we are authorized (on-box, or a stored
 * valid session/`?ados_key=`), so the app renders; a `401` means we need the
 * PIN, so it reads the PIN status and shows the branded splash. Any other error
 * lets the app render and handle it with its own states rather than blocking the
 * whole dashboard.
 */
export function DashboardAccessGate({ children }: { children: ReactNode }) {
  const [state, setState] = useState<GateState>("checking");
  const [pinStatus, setPinStatus] = useState<PinStatus | null>(null);

  const probe = useCallback(async () => {
    try {
      // `skipAuthSignal` so this probe's own 401 does not re-notify the gate.
      await apiFetch("/api/status", { skipAuthSignal: true });
      setState("ok");
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) {
        try {
          setPinStatus(await fetchPinStatus());
        } catch {
          setPinStatus(null);
        }
        setState("locked");
      } else {
        // A non-auth failure (transient network, 503 on an idle service): render
        // the app and let its panels surface their own errors.
        setState("ok");
      }
    }
  }, []);

  useEffect(() => {
    void probe();
  }, [probe]);

  useEffect(() => {
    setAuthRequiredHandler(() => {
      setState("checking");
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

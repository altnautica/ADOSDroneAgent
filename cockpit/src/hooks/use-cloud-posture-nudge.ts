// The one-shot cloud-posture nudge: an agent still on the `cloud` posture (the
// legacy default) whose operator has not acknowledged the new `local` default
// shows a banner with "Open settings" and "Got it". The agent owns the
// suppression flag (`/api/v1/setup/nudges`), so the nudge follows the node,
// not the browser. Both routes are residual (py); an agent without them stays
// silent.

import { useCallback, useEffect, useState } from "react";

import { apiFetch } from "@/shared/api-fetch";

export const CLOUD_NUDGE_ID = "cloud_posture_default_changed";

/** Whether to show the nudge: only on the `cloud` posture and only when the
 *  agent has not recorded an acknowledgement. */
export function shouldShowCloudNudge(mode: unknown, acked: unknown): boolean {
  if (mode !== "cloud") return false;
  return !(Array.isArray(acked) && acked.includes(CLOUD_NUDGE_ID));
}

function cloudMode(status: unknown): unknown {
  if (!status || typeof status !== "object" || !("cloud_choice" in status)) return undefined;
  const choice = status.cloud_choice;
  return choice && typeof choice === "object" && "mode" in choice ? choice.mode : undefined;
}

export function useCloudPostureNudge(): { visible: boolean; ack: () => void } {
  const [visible, setVisible] = useState(false);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const mode = cloudMode(await apiFetch("/api/status", { skipAuthSignal: true }));
        if (mode !== "cloud") return;
        const body = await apiFetch("/api/v1/setup/nudges", { skipAuthSignal: true });
        const acked = body && typeof body === "object" && "acked" in body ? body.acked : undefined;
        if (!cancelled) setVisible(shouldShowCloudNudge(mode, acked));
      } catch {
        // An older agent or a refused read: no nudge.
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const ack = useCallback(() => {
    setVisible(false);
    // Best-effort: a failed ack shows the nudge again on the next load.
    void apiFetch(`/api/v1/setup/nudges/${CLOUD_NUDGE_ID}/ack`, { method: "POST" }).catch(() => undefined);
  }, []);

  return { visible, ack };
}

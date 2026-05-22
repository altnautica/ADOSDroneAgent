// Fires a one-shot toast on Home when the agent is still on the
// `cloud` posture (the legacy default) and the operator has not yet
// acknowledged the new `local` default. The toast offers a one-click
// jump to /settings/cloud plus a "Got it" button that POSTs to the
// agent so the prompt suppresses on every future load.
//
// The agent owns the suppression flag (persisted in setup/state.json)
// so the prompt follows the agent through reflashes that wipe the
// browser.

import { useEffect, useRef } from "react";
import { useNavigate } from "react-router-dom";
import { toast as sonnerToast } from "sonner";

import { apiFetch } from "@/lib/api";

const NUDGE_ID = "cloud_posture_default_changed";

interface NudgeResponse {
  acked: string[];
}

export function useCloudPostureNudge(currentMode: string | undefined) {
  const navigate = useNavigate();
  const firedRef = useRef(false);

  useEffect(() => {
    if (firedRef.current) return;
    if (currentMode !== "cloud") return;

    let cancelled = false;
    (async () => {
      try {
        const res = await apiFetch<NudgeResponse>("/api/setup/nudges");
        if (cancelled) return;
        if (res.acked?.includes(NUDGE_ID)) return;
        firedRef.current = true;

        const ack = async () => {
          try {
            await apiFetch(`/api/setup/nudges/${NUDGE_ID}/ack`, {
              method: "POST",
            });
          } catch {
            // Best-effort; if the ack fails the prompt re-renders on
            // the next load — operator will see it again, not a
            // correctness issue.
          }
        };

        sonnerToast(
          "Cloud relay is on by default for upgraded agents.",
          {
            description:
              "New installs default to local-only. Switch if your drone never needs to be reached from outside the LAN.",
            duration: 12000,
            action: {
              label: "Switch",
              onClick: () => {
                void ack();
                navigate("/settings/cloud");
              },
            },
            cancel: {
              label: "Got it",
              onClick: () => {
                void ack();
              },
            },
            onDismiss: () => {
              void ack();
            },
            onAutoClose: () => {
              void ack();
            },
          },
        );
      } catch {
        // Nudge endpoint not reachable — older agent or transient
        // failure. Stay silent.
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [currentMode, navigate]);
}

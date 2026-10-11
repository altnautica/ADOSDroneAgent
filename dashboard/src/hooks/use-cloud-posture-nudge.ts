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

import i18n from "@/i18n";
import { apiFetch } from "@/shared/api-fetch";

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
    // Latch synchronously so React StrictMode's dev double-invoke
    // does not race two in-flight fetches to the same one-shot
    // toast. The ack still runs once per onClick / onDismiss /
    // onAutoClose because the server-side flag is idempotent.
    firedRef.current = true;

    let cancelled = false;
    (async () => {
      try {
        const res = await apiFetch<NudgeResponse>("/api/v1/setup/nudges");
        if (cancelled) return;
        if (res.acked?.includes(NUDGE_ID)) return;

        const ack = async () => {
          try {
            await apiFetch(`/api/v1/setup/nudges/${NUDGE_ID}/ack`, {
              method: "POST",
            });
          } catch {
            // Best-effort; if the ack fails the prompt re-renders on
            // the next load — operator will see it again, not a
            // correctness issue.
          }
        };

        sonnerToast(
          i18n.t("app.cloudNudge.title"),
          {
            description: i18n.t("app.cloudNudge.description"),
            duration: 12000,
            action: {
              label: i18n.t("app.cloudNudge.switch"),
              onClick: () => {
                void ack();
                navigate("/settings/cloud");
              },
            },
            cancel: {
              label: i18n.t("app.cloudNudge.gotIt"),
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

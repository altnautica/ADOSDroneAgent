// The pending-reboot banner. When a Settings write lands on a path a service
// reads only at startup, that path is recorded in the config store and this
// amber bar appears across the top of every screen — the honest surface:
// the panel never shows a reboot-gated change as already live. It offers a
// guarded reboot (tap arms, hold fires), or a video-service restart when every
// pending change is a video setting, and says when the request fails. It stays
// until the box has actually rebooted, detected from the agent's uptime.

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { RotateCw } from "lucide-react";

import { GuardedButton } from "@/components/ui/guarded-button";
import { useConfigStore } from "@/stores/config-store";
import { useStatusStore } from "@/stores/status-store";
import { apiFetch } from "@/shared/api-fetch";
import { rebootAgent } from "@/shared/api/setup-actions";
import { toast, toastFromError } from "@/shared/api/toast";
import "@/shared/i18n";
import { prettify } from "@/lib/settings-schema";
import { videoRestartApplies } from "@/lib/reboot-scope";
import { cn } from "@/lib/utils";

export function RebootBanner({ floating = false }: { floating?: boolean }) {
  const { t } = useTranslation();
  const paths = useConfigStore((s) => s.pendingRebootPaths);
  const clearReboot = useConfigStore((s) => s.clearReboot);
  const maybeAutoClear = useConfigStore((s) => s.maybeAutoClearReboot);
  const status = useStatusStore((s) => s.status);
  const uptime = status?.system?.uptime_seconds ?? null;
  const [rebooting, setRebooting] = useState(false);

  // Clear the banner once the box's uptime proves it rebooted after the change.
  useEffect(() => {
    maybeAutoClear(uptime);
  }, [uptime, maybeAutoClear]);

  if (paths.length === 0) return null;
  const videoOnly = videoRestartApplies(paths);

  const reboot = async () => {
    if (rebooting) return;
    setRebooting(true);
    // If the board never goes down, return to the armed state so the operator
    // can retry rather than sit on "Rebooting…".
    window.setTimeout(() => setRebooting(false), 25_000);
    try {
      await rebootAgent();
      toast.ok(t("diagnostics.reboot.queued"), t("cockpit.system.rebootDetail"));
    } catch (err) {
      setRebooting(false);
      toastFromError(err, t("diagnostics.reboot.failed"));
    }
  };

  const restartVideo = async () => {
    if (rebooting) return;
    setRebooting(true);
    try {
      await apiFetch("/api/services/ados-video/restart", { method: "POST", body: {} });
      // The video service read the new values on start: nothing is pending.
      clearReboot();
      toast.ok(t("cockpit.shell.videoRestarted"));
    } catch (err) {
      toastFromError(err, t("cockpit.shell.videoRestartFailed"));
    } finally {
      setRebooting(false);
    }
  };

  const summary = paths.map((p) => prettify(p.split(".").pop() ?? p)).join(", ");

  return (
    <div
      className={cn(
        "flex shrink-0 items-center gap-[0.5rem] bg-warn/20 px-[0.6rem] py-[0.3rem] text-warn",
        floating && "pointer-events-auto backdrop-blur-sm",
      )}
    >
      <RotateCw
        className={cn("h-[1.1rem] w-[1.1rem] shrink-0", rebooting && "animate-spin")}
        aria-hidden
      />
      <div className="min-w-0 flex-1">
        <div className="truncate text-[0.8rem] font-medium">
          {rebooting
            ? t(videoOnly ? "cockpit.shell.videoRestarting" : "diagnostics.reboot.rebooting")
            : t(videoOnly ? "cockpit.shell.videoRestartPending" : "cockpit.shell.rebootPending")}
        </div>
        <div className="truncate text-[0.75rem] opacity-80">{summary}</div>
      </div>
      {rebooting ? null : videoOnly ? (
        <GuardedButton
          label={t("cockpit.shell.restartVideo")}
          guardLabel={t("cockpit.system.restartGuard")}
          icon={RotateCw}
          onFired={() => void restartVideo()}
        />
      ) : (
        <GuardedButton
          label={t("diagnostics.reboot.button")}
          guardLabel={t("cockpit.shell.rebootGuard")}
          icon={RotateCw}
          onFired={() => void reboot()}
        />
      )}
    </div>
  );
}

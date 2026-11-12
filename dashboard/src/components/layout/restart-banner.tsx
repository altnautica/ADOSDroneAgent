import { RotateCw } from "lucide-react";
import { useEffect } from "react";
import { useTranslation } from "react-i18next";

import { useHeartbeat } from "@/hooks/use-heartbeat";
import { useNow } from "@/hooks/use-now";
import { restartElapsedS, restartPhase } from "@/lib/restart-pending";
import { toast } from "@/shared/api/toast";
import { useUiStore } from "@/stores/ui-store";

/** "Rebooting… (n s)" while a restart the operator asked for is pending. It
 *  clears itself with a "back online" toast once the agent answers again, or
 *  with an error toast when it has not come back in time. */
export function RestartBanner() {
  const { t } = useTranslation();
  const pending = useUiStore((s) => s.restartPending);
  const markDown = useUiStore((s) => s.markRestartDown);
  const clear = useUiStore((s) => s.clearRestart);
  const heartbeat = useHeartbeat();
  const now = useNow(1_000);

  useEffect(() => {
    if (pending && heartbeat.isError) markDown();
  }, [pending, heartbeat.isError, markDown]);

  const phase = pending
    ? restartPhase(pending, now, {
        reachable: !heartbeat.isError && heartbeat.data != null,
        uptimeS: heartbeat.data?.uptime_seconds ?? null,
      })
    : null;

  useEffect(() => {
    if (phase === "back") {
      toast.ok(t("layout.restart.back"));
      clear();
    } else if (phase === "timedOut") {
      toast.err(t("layout.restart.timedOut"));
      clear();
    }
  }, [phase, clear, t]);

  if (!pending || phase !== "waiting") return null;
  const seconds = restartElapsedS(pending, now);
  return (
    <div
      className="flex items-center gap-2 rounded-md border border-info/40 bg-info/10 px-3 py-2 text-sm text-info"
      role="status"
    >
      <RotateCw className="h-4 w-4 shrink-0 animate-spin motion-reduce:animate-none" />
      <span>
        {pending.kind === "reboot"
          ? t("layout.restart.rebooting", { seconds })
          : t("layout.restart.restarting", { seconds })}
      </span>
    </div>
  );
}

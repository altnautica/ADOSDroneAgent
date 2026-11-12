import { AlertCircle, AlertTriangle } from "lucide-react";
import { useLocation } from "react-router-dom";

import { useHeartbeat } from "@/hooks/use-heartbeat";
import { useNodeIdentity } from "@/hooks/use-node-identity";
import { useSnapshot } from "@/hooks/use-snapshot";
import { buildBanners } from "@/shared/api/banners";
import { cn } from "@/lib/utils";
import { RestartBanner } from "@/components/layout/restart-banner";
import { SetupNeededBanner } from "@/components/layout/setup-needed-banner";
import { useUiStore } from "@/stores/ui-store";

/** The FC-link banner reads the snapshot, so it polls (every 3 s) only on the
 *  drone Home page; every other page shows just the agent-offline banner. */
const BANNER_SNAPSHOT_MS = 3_000;

export function BannerHost() {
  const { profile } = useNodeIdentity();
  const heartbeat = useHeartbeat();
  const { pathname } = useLocation();
  // A restart the operator asked for is not an outage: its own banner speaks.
  const restarting = useUiStore((s) => s.restartPending !== null);
  const onDroneHome = profile === "drone" && pathname === "/";
  const snapshot = useSnapshot({ intervalMs: BANNER_SNAPSHOT_MS, enabled: onDroneHome });

  const banners = buildBanners({
    agentUnreachable: heartbeat.isError && !restarting,
    profile,
    fcConnected: onDroneHome && snapshot.isSuccess ? (snapshot.data.fc?.connected ?? null) : null,
    fcReachable: heartbeat.data?.fcReachable,
  });

  return (
    <div className="px-4 lg:px-6 pt-3 space-y-2 empty:hidden">
      <SetupNeededBanner />
      <RestartBanner />
      {banners.map((b) => {
        const Icon = b.severity === "err" ? AlertCircle : AlertTriangle;
        return (
          <div
            key={b.id}
            className={cn(
              "flex items-center gap-2 rounded-md border px-3 py-2 text-sm",
              b.severity === "err"
                ? "border-destructive/40 bg-destructive/10 text-destructive"
                : "border-warn/40 bg-warn/10 text-warn",
            )}
            role="status"
          >
            <Icon className="h-4 w-4 shrink-0" />
            <span>{b.message}</span>
          </div>
        );
      })}
    </div>
  );
}

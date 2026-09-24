import { AlertCircle, AlertTriangle } from "lucide-react";

import { useStatus } from "@/hooks/use-status";
import { useHeartbeat } from "@/hooks/use-heartbeat";
import { useSnapshot } from "@/hooks/use-snapshot";
import { cn } from "@/lib/utils";
import type { Severity } from "@/lib/types";

interface Banner {
  id: string;
  severity: Severity;
  message: string;
}

function buildBanners(
  status: ReturnType<typeof useStatus>,
  heartbeat: ReturnType<typeof useHeartbeat>,
  snapshot: ReturnType<typeof useSnapshot>,
): Banner[] {
  const banners: Banner[] = [];

  if (heartbeat.isError) {
    banners.push({
      id: "agent-offline",
      severity: "err",
      message: "Agent is unreachable. Reconnecting…",
    });
  }

  if (status.isSuccess && snapshot.isSuccess) {
    // An MSP flight controller never sends a MAVLink heartbeat, so
    // `fc.connected` stays false for it; `fcReachable` covers both.
    if (
      status.data.profile === "drone" &&
      snapshot.data.fc?.connected === false &&
      heartbeat.data?.fcReachable !== true
    ) {
      banners.push({
        id: "fc-disconnected",
        severity: "warn",
        message: "Flight controller is disconnected.",
      });
    }
  }

  return banners;
}

export function BannerHost() {
  const status = useStatus();
  const heartbeat = useHeartbeat();
  const snapshot = useSnapshot();

  const banners = buildBanners(status, heartbeat, snapshot);

  if (banners.length === 0) return null;

  return (
    <div className="px-4 lg:px-6 pt-3 space-y-2">
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

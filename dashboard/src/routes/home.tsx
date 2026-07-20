import { CloudOff, Globe, RefreshCw } from "lucide-react";
import { Link } from "react-router-dom";

import { CloudPanel } from "@/components/panels/cloud-panel";
import { CockpitLauncher } from "@/components/panels/cockpit-launcher";
import { FcPanel } from "@/components/panels/fc-panel";
import { HardwarePanel } from "@/components/panels/hardware-panel";
import { MeshPanel } from "@/components/panels/mesh-panel";
import { NetworkPanel } from "@/components/panels/network-panel";
import { ServicesPanel } from "@/components/panels/services-panel";
import { SourcesPanel } from "@/components/panels/sources-panel";
import { SparklinesRow } from "@/components/panels/sparklines-row";
import { StatusTiles } from "@/components/panels/status-tiles";
import { VideoPanel } from "@/components/panels/video-panel";
import { WfbRxPanel } from "@/components/panels/wfb-rx-panel";
import { WfbTxPanel } from "@/components/panels/wfb-tx-panel";
import { Button } from "@/components/ui/button";
import { useCloudPostureNudge } from "@/hooks/use-cloud-posture-nudge";
import { useHeartbeat } from "@/hooks/use-heartbeat";
import { useStatus } from "@/hooks/use-status";
import { cn } from "@/lib/utils";
import { modeFromStatus, regionFromStatus } from "@/lib/region";
import type { SetupStatus } from "@/lib/types";

export function HomeRoute() {
  const status = useStatus();
  const heartbeat = useHeartbeat();
  const profile = status.data?.profile ?? "auto";
  const role = status.data?.ground_role ?? "direct";
  // One-shot prompt for agents still on the legacy `cloud` posture.
  // The hook self-gates and runs at most once per agent.
  useCloudPostureNudge(status.data?.cloud_choice?.mode);

  // Coherent agent-offline state: when the heartbeat has errored AND
  // we have no cached data, render a single page-level message instead
  // of a grid of "—" panels that look broken.
  const offline =
    heartbeat.isError && !heartbeat.data && status.isError && !status.data;
  if (offline) {
    return <OfflineHome onRetry={() => {
      heartbeat.refetch();
      status.refetch();
    }} />;
  }

  return profile === "ground_station" ? <GroundHome role={role} /> : <DroneHome />;
}

function OfflineHome({ onRetry }: { onRetry: () => void }) {
  return (
    <div className="max-w-2xl mx-auto py-12">
      <div className="rounded-lg border border-border bg-muted/20 p-8 flex flex-col items-center text-center space-y-4">
        <div className="h-12 w-12 rounded-full bg-destructive/10 flex items-center justify-center">
          <CloudOff className="h-6 w-6 text-destructive" />
        </div>
        <div>
          <h1 className="text-xl font-semibold tracking-tight">
            Agent unreachable
          </h1>
          <p className="text-sm text-muted-foreground mt-2 max-w-sm">
            The dashboard couldn't reach the agent's REST API. Check that the
            board is powered, on the network, and that{" "}
            <span className="font-mono">ados-supervisor</span> is running.
          </p>
        </div>
        <Button variant="outline" size="sm" onClick={onRetry}>
          <RefreshCw className="h-3.5 w-3.5" />
          Retry
        </Button>
      </div>
    </div>
  );
}

// Compact chip showing the operating-region RF posture. Amber when the
// radio is unrestricted (operator-responsibility framing), neutral when
// a region is pinned. Links to the Region settings page.
function RegionChip() {
  const status = useStatus();
  const reg = status.data?.regulatory as SetupStatus["regulatory"];
  const mode = modeFromStatus(reg);
  const region = regionFromStatus(reg);
  const unrestricted = mode !== "region" || !region;

  return (
    <Link
      to="/settings/region"
      title={
        unrestricted
          ? "Unrestricted RF posture — operator responsible for local RF compliance. Click to pin a region."
          : `Operating region pinned to ${region}. Click to change.`
      }
      className={cn(
        "inline-flex items-center gap-1.5 rounded-md border px-2 py-1 text-xs font-medium transition-colors",
        unrestricted
          ? "border-amber-500/40 bg-amber-500/5 text-amber-200 hover:bg-amber-500/10"
          : "border-border bg-card text-muted-foreground hover:bg-accent/30",
      )}
    >
      <Globe className="h-3.5 w-3.5" />
      {unrestricted ? "Region: Unrestricted" : `Region: Pinned to ${region}`}
    </Link>
  );
}

function DroneHome() {
  return (
    <div className="space-y-4 max-w-[1400px]">
      <header className="flex items-start justify-between gap-4">
        <div>
          <h1 className="text-xl font-semibold tracking-tight">Home</h1>
          <p className="text-sm text-muted-foreground">
            Live status, video, flight controller, and 60s telemetry.
          </p>
        </div>
        <RegionChip />
      </header>

      <StatusTiles />
      <SparklinesRow />

      <div className="grid grid-cols-1 lg:grid-cols-12 gap-4 items-stretch">
        <div className="lg:col-span-7 xl:col-span-8">
          <VideoPanel />
        </div>
        <div className="lg:col-span-5 xl:col-span-4 flex flex-col gap-4">
          <FcPanel />
          <WfbTxPanel />
          <CloudPanel />
        </div>
        <div className="lg:col-span-6">
          <HardwarePanel />
        </div>
        <div className="lg:col-span-6">
          <NetworkPanel />
        </div>
        <div className="lg:col-span-12">
          <ServicesPanel />
        </div>
      </div>
    </div>
  );
}

function GroundHome({ role }: { role: "direct" | "relay" | "receiver" }) {
  const subtitle =
    role === "receiver"
      ? "Aggregating relays into one stream. Live receive metrics, mesh state, and per-source FEC."
      : role === "relay"
        ? "Forwarding the drone link. Live receive metrics, mesh state, and gateway election."
        : "Direct ground node. Live receive metrics for the drone link.";

  return (
    <div className="space-y-4 max-w-[1400px]">
      <CockpitLauncher />

      <header className="flex items-start justify-between gap-4">
        <div>
          <h1 className="text-xl font-semibold tracking-tight">Home</h1>
          <p className="text-sm text-muted-foreground">{subtitle}</p>
        </div>
        <RegionChip />
      </header>

      <StatusTiles />
      <SparklinesRow />

      <div className="grid grid-cols-1 lg:grid-cols-12 gap-4 items-stretch">
        <div className="lg:col-span-7 xl:col-span-8">
          <VideoPanel />
        </div>
        <div className="lg:col-span-5 xl:col-span-4 flex flex-col gap-4">
          <WfbRxPanel />
          <CloudPanel />
        </div>

        {(role === "relay" || role === "receiver") && (
          <div className="lg:col-span-6">
            <MeshPanel />
          </div>
        )}
        {role === "receiver" && (
          <div className="lg:col-span-6">
            <SourcesPanel />
          </div>
        )}

        <div className="lg:col-span-6">
          <HardwarePanel />
        </div>
        <div className="lg:col-span-6">
          <NetworkPanel />
        </div>
        <div className="lg:col-span-12">
          <ServicesPanel />
        </div>
      </div>
    </div>
  );
}

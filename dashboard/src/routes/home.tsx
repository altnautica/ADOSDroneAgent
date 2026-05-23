import { ChevronRight, CloudOff, RefreshCw } from "lucide-react";
import { Link } from "react-router-dom";

import { CloudPanel } from "@/components/panels/cloud-panel";
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
import { Button } from "@/components/ui/button";
import { useCloudPostureNudge } from "@/hooks/use-cloud-posture-nudge";
import { useHeartbeat } from "@/hooks/use-heartbeat";
import { useStatus } from "@/hooks/use-status";

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

  // Resume banner when the operator dismissed the wizard via Skip to
  // Home without finalizing. Sits above the home grid so the entry
  // point back into setup is always one click away.
  const showResume =
    status.data?.setup_skipped === true && status.data?.setup_finalized !== true;

  const body = profile === "ground_station" ? <GroundHome role={role} /> : <DroneHome />;
  if (!showResume) return body;
  return (
    <div className="space-y-4">
      <ResumeSetupBanner />
      {body}
    </div>
  );
}

function ResumeSetupBanner() {
  return (
    <div className="flex items-center justify-between rounded-md border border-amber-500/40 bg-amber-500/5 px-4 py-2.5">
      <div className="text-sm">
        <span className="font-medium text-amber-200">Setup is incomplete.</span>{" "}
        <span className="text-muted-foreground">
          Some onboarding steps were skipped. Resume any time.
        </span>
      </div>
      <Button asChild variant="outline" size="sm">
        <Link to="/setup" className="gap-1">
          Resume setup
          <ChevronRight className="h-3.5 w-3.5" />
        </Link>
      </Button>
    </div>
  );
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

function DroneHome() {
  return (
    <div className="space-y-4 max-w-[1400px]">
      <header>
        <h1 className="text-xl font-semibold tracking-tight">Home</h1>
        <p className="text-sm text-muted-foreground">
          Live status, video, flight controller, and 60s telemetry.
        </p>
      </header>

      <StatusTiles />
      <SparklinesRow />

      <div className="grid grid-cols-1 lg:grid-cols-12 gap-4 items-stretch">
        <div className="lg:col-span-7 xl:col-span-8">
          <VideoPanel />
        </div>
        <div className="lg:col-span-5 xl:col-span-4 flex flex-col gap-4">
          <FcPanel />
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
      <header>
        <h1 className="text-xl font-semibold tracking-tight">Home</h1>
        <p className="text-sm text-muted-foreground">{subtitle}</p>
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

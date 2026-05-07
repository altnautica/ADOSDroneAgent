import { CloudPanel } from "@/components/panels/cloud-panel";
import { FcPanel } from "@/components/panels/fc-panel";
import { MeshPanel } from "@/components/panels/mesh-panel";
import { NetworkPanel } from "@/components/panels/network-panel";
import { ServicesPanel } from "@/components/panels/services-panel";
import { SourcesPanel } from "@/components/panels/sources-panel";
import { SparklinesRow } from "@/components/panels/sparklines-row";
import { StatusTiles } from "@/components/panels/status-tiles";
import { VideoPanel } from "@/components/panels/video-panel";
import { WfbRxPanel } from "@/components/panels/wfb-rx-panel";
import { useStatus } from "@/hooks/use-status";

export function HomeRoute() {
  const status = useStatus();
  const profile = status.data?.profile ?? "auto";
  const role = status.data?.ground_role ?? "direct";

  if (profile === "ground_station") {
    return <GroundHome role={role} />;
  }

  return <DroneHome />;
}

function DroneHome() {
  return (
    <div className="space-y-6 max-w-[1400px]">
      <header>
        <h1 className="text-xl font-semibold tracking-tight">Home</h1>
        <p className="text-sm text-muted-foreground">
          Live status, video, flight controller, and 60s telemetry.
        </p>
      </header>

      <StatusTiles />
      <SparklinesRow />

      <div className="grid grid-cols-1 lg:grid-cols-12 gap-4">
        <div className="lg:col-span-7 xl:col-span-8">
          <VideoPanel />
        </div>
        <div className="lg:col-span-5 xl:col-span-4">
          <FcPanel />
        </div>
        <div className="lg:col-span-4">
          <NetworkPanel />
        </div>
        <div className="lg:col-span-4">
          <CloudPanel />
        </div>
        <div className="lg:col-span-4">
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
    <div className="space-y-6 max-w-[1400px]">
      <header>
        <h1 className="text-xl font-semibold tracking-tight">Home</h1>
        <p className="text-sm text-muted-foreground">{subtitle}</p>
      </header>

      <StatusTiles />
      <SparklinesRow />

      <div className="grid grid-cols-1 lg:grid-cols-12 gap-4">
        <div className="lg:col-span-7 xl:col-span-8">
          <VideoPanel />
        </div>
        <div className="lg:col-span-5 xl:col-span-4">
          <WfbRxPanel />
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

        <div className="lg:col-span-4">
          <NetworkPanel />
        </div>
        <div className="lg:col-span-4">
          <CloudPanel />
        </div>
        <div className="lg:col-span-4">
          <ServicesPanel />
        </div>
      </div>
    </div>
  );
}

import { CloudPanel } from "@/components/panels/cloud-panel";
import { FcPanel } from "@/components/panels/fc-panel";
import { NetworkPanel } from "@/components/panels/network-panel";
import { ServicesPanel } from "@/components/panels/services-panel";
import { StatusTiles } from "@/components/panels/status-tiles";
import { VideoPanelStub } from "@/components/panels/video-panel-stub";
import { useStatus } from "@/hooks/use-status";

export function HomeRoute() {
  const status = useStatus();
  const profile = status.data?.profile ?? "auto";

  return (
    <div className="space-y-6 max-w-[1400px]">
      <header>
        <h1 className="text-xl font-semibold tracking-tight">Home</h1>
        <p className="text-sm text-muted-foreground">
          {profile === "ground_station"
            ? "Ground station overview will land in v0.14.5."
            : profile === "drone"
              ? "Live status, video, and flight controller for this drone."
              : "Profile is detecting…"}
        </p>
      </header>

      <StatusTiles />

      <div className="grid grid-cols-1 lg:grid-cols-12 gap-4">
        <div className="lg:col-span-7 xl:col-span-8">
          <VideoPanelStub />
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

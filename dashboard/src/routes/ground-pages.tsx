// Single-panel routes wired off the sidebar so ground-station operators
// can drill into Receive / Mesh / Sources without leaving the AppShell.
// Each page is a thin wrapper around the corresponding Home panel.

import { MeshPanel } from "@/components/panels/mesh-panel";
import { SourcesPanel } from "@/components/panels/sources-panel";
import { WfbRxPanel } from "@/components/panels/wfb-rx-panel";

interface ShellProps {
  title: string;
  blurb: string;
  children: React.ReactNode;
}

function PageShell({ title, blurb, children }: ShellProps) {
  return (
    <div className="space-y-6 max-w-3xl">
      <header>
        <h1 className="text-xl font-semibold tracking-tight">{title}</h1>
        <p className="text-sm text-muted-foreground">{blurb}</p>
      </header>
      {children}
    </div>
  );
}

export function ReceiveRoute() {
  return (
    <PageShell
      title="WFB Receive"
      blurb="WFB-rx adapter, channel, link quality, and FEC counters."
    >
      <WfbRxPanel />
    </PageShell>
  );
}

export function MeshRoute() {
  return (
    <PageShell
      title="Mesh"
      blurb="batman-adv role, gateway election, and partition state."
    >
      <MeshPanel />
    </PageShell>
  );
}

export function SourcesRoute() {
  return (
    <PageShell
      title="Sources"
      blurb="Per-relay aggregation and FEC dedup stats."
    >
      <SourcesPanel />
    </PageShell>
  );
}

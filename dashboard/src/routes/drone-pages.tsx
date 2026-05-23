// Single-panel routes wired off the drone-profile sidebar so the
// operator can drill into the WFB-tx surface without leaving the
// AppShell. Each page is a thin wrapper around a Home panel and
// renders the same panel content full-width.

import { WfbTxPanel } from "@/components/panels/wfb-tx-panel";

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

export function TransmitRoute() {
  return (
    <PageShell
      title="WFB Transmit"
      blurb="WFB-tx adapter, channel, bandwidth, MCS index, and TX power."
    >
      <WfbTxPanel />
    </PageShell>
  );
}

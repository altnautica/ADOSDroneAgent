// Single-panel routes wired off the sidebar so ground-station operators
// can drill into Receive / Mesh / Sources without leaving the AppShell.
// Each page is a thin wrapper around the corresponding Home panel.

import { useTranslation } from "react-i18next";

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
  const { t } = useTranslation();
  return (
    <PageShell
      title={t("nav.receive")}
      blurb={t("app.pages.receiveBlurb")}
    >
      <WfbRxPanel />
    </PageShell>
  );
}

export function MeshRoute() {
  const { t } = useTranslation();
  return (
    <PageShell
      title={t("nav.mesh")}
      blurb={t("app.pages.meshBlurb")}
    >
      <MeshPanel />
    </PageShell>
  );
}

export function SourcesRoute() {
  const { t } = useTranslation();
  return (
    <PageShell
      title={t("nav.sources")}
      blurb={t("app.pages.sourcesBlurb")}
    >
      <SourcesPanel />
    </PageShell>
  );
}

// The quick menu, the flight screen's Menu, on the shared sheet: over the
// Feed, the top-bar items a narrow bar moves here (link, navigation source,
// controller, layer level); a card for every tab this node shows; the
// running extensions' cockpit panels; and the way back to the node
// dashboard. Opened by the quick-menu command (the top bar's Menu, panel
// long-press, gamepad Options); closed by back, Escape or a tap outside, with
// the controller moving between its cards and its button hints shown while a
// gamepad is connected. The exit is hidden on the kiosk (`?kiosk=1`), where
// the cockpit is the whole display.

import { useState } from "react";
import { ArrowLeft, LayoutDashboard } from "lucide-react";

import { FlightMenuSection } from "@/components/feed/cockpit/top-bar";
import { PluginFrameHost } from "@/components/plugins/plugin-overlay-host";
import { Card } from "@/components/ui/card";
import { Sheet } from "@/components/ui/sheet";
import { usePadStatus } from "@/hooks/use-gamepad";
import { mountablePanels } from "@/lib/extensions";
import { isKiosk } from "@/lib/kiosk";
import { activeScreenId, useNavStore } from "@/stores/nav-store";
import { getScreen, tabScreens } from "@/nav/registry";
import { useNodeIdentity } from "@/shared/use-profile";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useStatusStore } from "@/stores/status-store";

export function QuickMenu() {
  const goTab = useNavStore((s) => s.goTab);
  const closeQuickMenu = useNavStore((s) => s.closeQuickMenu);
  const activeTabId = useNavStore((s) => s.activeTabId);
  const onFeed = useNavStore((s) => Boolean(getScreen(activeScreenId(s)).fullBleed) && s.detailStack.length === 0);
  const padConnected = usePadStatus().connected;
  const { profile, role } = useNodeIdentity();
  const fcFamily = useStatusStore((s) => s.status?.fc_family ?? null);
  const tabs = tabScreens(profile, role, fcFamily);
  const extensions = useExtensionsStore((s) => s.extensions);
  const panels = extensions.flatMap((ext) => mountablePanels(ext, "cockpit.panel").map((p) => ({ ext, p })));
  const [openPanel, setOpenPanel] = useState<string | null>(null);
  const kiosk = isKiosk();

  const shown = panels.find(({ ext, p }) => `${ext.pluginId}:${p.id}` === openPanel) ?? null;

  return (
    <Sheet
      title={
        shown ? (
          <button
            type="button"
            onClick={() => setOpenPanel(null)}
            className="touch-target -ml-[0.3rem] flex items-center gap-[0.3rem] rounded-md px-[0.3rem] hover:bg-muted"
          >
            <ArrowLeft className="h-[1.2rem] w-[1.2rem]" aria-hidden />
            {shown.p.title}
          </button>
        ) : (
          "Go to"
        )
      }
      onDismiss={closeQuickMenu}
    >
      {shown ? (
        <PluginFrameHost ext={shown.ext} title={shown.p.title} className="h-[22rem] w-full rounded-lg" />
      ) : (
        <>
          {onFeed ? (
            <div className="mb-[0.8rem]">
              <FlightMenuSection />
            </div>
          ) : null}
          <div className="grid grid-cols-2 gap-[0.5rem]">
            {tabs.map((tab) =>
              tab.icon ? (
                <Card
                  key={tab.id}
                  title={tab.title}
                  icon={tab.icon}
                  current={tab.id === activeTabId}
                  onClick={() => goTab(tab.id)}
                />
              ) : null,
            )}
          </div>

          {panels.length > 0 ? (
            <>
              <h3 className="mb-[0.4rem] mt-[0.8rem] text-[0.8rem] uppercase tracking-wide text-muted-foreground">
                Extension panels
              </h3>
              <div className="grid grid-cols-2 gap-[0.5rem]">
                {panels.map(({ ext, p }) => (
                  <button
                    key={`${ext.pluginId}:${p.id}`}
                    type="button"
                    onClick={() => setOpenPanel(`${ext.pluginId}:${p.id}`)}
                    className="touch-target min-h-[3.5rem] rounded-md bg-muted px-[0.5rem] text-[0.85rem] text-surface-foreground hover:bg-border"
                  >
                    {p.title}
                  </button>
                ))}
              </div>
            </>
          ) : null}

          {!kiosk ? (
            <button
              type="button"
              onClick={() => window.location.assign("/")}
              className="touch-target mt-[0.8rem] flex w-full items-center justify-center gap-[0.5rem] rounded-md border border-border text-[0.9rem] text-surface-foreground hover:bg-muted"
            >
              <LayoutDashboard className="h-[1.2rem] w-[1.2rem]" aria-hidden />
              Exit to dashboard
            </button>
          ) : null}
          {padConnected ? (
            <div className="mt-[0.8rem] flex justify-center gap-[1rem] font-mono text-[0.75rem] text-muted-foreground" data-testid="pad-hints">
              <span>✕ select</span>
              <span>○ back</span>
              <span>D-pad move</span>
              <span>Options close</span>
            </div>
          ) : null}
        </>
      )}
    </Sheet>
  );
}

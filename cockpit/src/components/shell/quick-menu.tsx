// The quick menu, the flight screen's Menu, on the shared sheet. Its entries
// follow the cockpit spec's order (`layout.json` `menu.order`,
// `lib/quick-menu-model`): over the Feed, the top-bar items the bar has moved
// here (only those actually hidden), View (cycles the views), Cameras (with
// more than one leg), Layers (the level and every layer flag), Pre-flight and
// Flight modes; then Controls, the running extensions' cockpit panels, Set up
// aircraft (only while the pilot reports setup is needed), Settings with a
// card for every screen this node shows, and Exit to the node dashboard
// (none on the kiosk, `?kiosk=1`, where the cockpit is the whole display).
// Opened by the top bar's Menu, a panel long-press, the controller's Start
// button off the Feed, and on the Feed the DualShock's Options, which
// `ds4.json` binds to `quick_settings`, the Menu. Closed by back, Escape or
// a tap outside, with the controller moving between its entries and its
// button hints shown while a gamepad is connected.

import { useMemo, useState, type ReactNode } from "react";
import { ArrowLeft, ClipboardCheck, Eye, Gamepad2, LayoutDashboard, Navigation, Wrench } from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { FlightMenuSection, LayersMenuSection } from "@/components/feed/cockpit/top-bar";
import { CameraSwitcher } from "@/components/feed/stream-tabs";
import { PluginFrameHost } from "@/components/plugins/plugin-overlay-host";
import { Card } from "@/components/ui/card";
import { Sheet } from "@/components/ui/sheet";
import { usePadStatus } from "@/hooks/use-gamepad";
import { useFreshPilot } from "@/hooks/use-pilot";
import { preflightTally, usePreflightItems } from "@/hooks/use-preflight-items";
import { useRoster } from "@/hooks/use-roster";
import { useWidthClass } from "@/hooks/use-width-class";
import { VIEW_LABELS } from "@/lib/cockpit-view";
import { knownOnGround } from "@/lib/controller-tour";
import { mountablePanels } from "@/lib/extensions";
import { feedCameras } from "@/lib/feed-legs";
import { menuTextPx, movedItems } from "@/lib/flight-layout";
import { isKiosk } from "@/lib/kiosk";
import { menuEntries, type MenuEntryId } from "@/lib/quick-menu-model";
import { getScreen, tabScreens } from "@/nav/registry";
import { specText } from "@/shared/hud/spec-text";
import { severityTextColor } from "@/shared/hud/status-pill";
import { useNodeIdentity } from "@/shared/use-profile";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useFlightStore } from "@/stores/flight-store";
import { activeScreenId, useNavStore } from "@/stores/nav-store";
import { useSetupOfferStore } from "@/stores/setup-offer-store";
import { useStatusStore } from "@/stores/status-store";
import { useTopBarStore } from "@/stores/top-bar-store";

/** The Settings screen's Controls page. */
const CONTROLS_SCREEN_ID = "settings:!controls";

function MenuRow({
  icon: Icon,
  label,
  detail,
  onClick,
  entry,
}: {
  icon: LucideIcon;
  label: string;
  detail?: ReactNode;
  onClick: () => void;
  entry: MenuEntryId;
}) {
  const fontSize = menuTextPx(useWidthClass().widthClass);
  return (
    <button
      type="button"
      onClick={onClick}
      data-menu-entry={entry}
      className="touch-target flex min-h-[48px] w-full items-center gap-[10px] rounded-md border border-border px-[12px] text-left text-surface-foreground hover:bg-muted"
      style={{ fontSize }}
    >
      <Icon className="h-[18px] w-[18px] shrink-0" aria-hidden />
      {label}
      {detail}
    </button>
  );
}

function MenuHeading({ children }: { children: string }) {
  return <h3 className="mb-[4px] mt-[4px] text-[12px] uppercase tracking-wide text-muted-foreground">{children}</h3>;
}

/** The Pre-flight row's passed/total, coloured by the worst item. */
function PreflightDetail() {
  const t = preflightTally(usePreflightItems());
  return (
    <span style={{ color: severityTextColor(t.state === "fail" ? "critical" : t.state === "warn" ? "warning" : "ok") }} data-testid="menu-preflight-count">
      {`· ${t.passed}/${t.total}`}
    </span>
  );
}

export function QuickMenu() {
  const goTab = useNavStore((s) => s.goTab);
  const openDetail = useNavStore((s) => s.openDetail);
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
  const { widthClass } = useWidthClass();
  const movedCount = useTopBarStore((s) => s.movedCount);
  const view = useCockpitViewStore((s) => s.view);
  const cycleView = useCockpitViewStore((s) => s.cycleView);
  const openSheet = useCockpitUiStore((s) => s.openSheet);
  // A visor carries its own cameras like a drone, so it lists the same roster.
  const roster = useRoster(onFeed && (profile === "drone" || profile === "visor"));
  const streams = useStatusStore((s) => s.status?.video?.streams);
  const cameras = useMemo(() => feedCameras(streams, roster), [streams, roster]);
  const pilot = useFreshPilot();
  const setupNeeded = pilot?.readiness.some((r) => r.id === "setup_needed" && !r.ok) ?? false;
  const setupOffered = useSetupOfferStore((s) => s.uid !== null);
  const closeSetupOffer = useSetupOfferStore((s) => s.close);
  const telemetryLive = useFlightStore((s) => s.live && !s.stale);
  const telemetryArmed = useFlightStore((s) => s.telemetry?.armed ?? null);
  const onGround = knownOnGround(pilot, { live: telemetryLive, armed: telemetryArmed });

  const entries = menuEntries({
    onFeed,
    movedCount,
    cameraCount: cameras.length,
    panelCount: panels.length,
    setupNeeded,
    setupOffered,
    knownOnGround: onGround,
    kiosk,
  });
  const shown = panels.find(({ ext, p }) => `${ext.pluginId}:${p.id}` === openPanel) ?? null;
  const sheet = (id: "preflight" | "mode") => {
    closeQuickMenu();
    openSheet(id);
  };

  const entry = (id: MenuEntryId) => {
    switch (id) {
      case "overflow":
        return <FlightMenuSection moved={movedItems(movedCount)} />;
      case "view":
        return <MenuRow entry={id} icon={Eye} label={`${specText("cockpit.menu.view")} · ${VIEW_LABELS[view] ?? view}`} onClick={cycleView} />;
      case "cameras":
        return (
          <div>
            <MenuHeading>{specText("cockpit.menu.cameras")}</MenuHeading>
            <CameraSwitcher cameras={cameras} />
          </div>
        );
      case "layers":
        return <LayersMenuSection />;
      case "preflight":
        return <MenuRow entry={id} icon={ClipboardCheck} label={specText("cockpit.menu.preflight")} detail={<PreflightDetail />} onClick={() => sheet("preflight")} />;
      case "modes":
        return <MenuRow entry={id} icon={Navigation} label={specText("cockpit.menu.modes")} onClick={() => sheet("mode")} />;
      case "controls":
        return <MenuRow entry={id} icon={Gamepad2} label={specText("cockpit.menu.controls")} onClick={() => openDetail(CONTROLS_SCREEN_ID)} />;
      case "extension_panels":
        return (
          <div>
            <MenuHeading>{specText("cockpit.menu.extension_panels")}</MenuHeading>
            <div className="grid grid-cols-2 gap-[8px]">
              {panels.map(({ ext, p }) => (
                <button
                  key={`${ext.pluginId}:${p.id}`}
                  type="button"
                  onClick={() => setOpenPanel(`${ext.pluginId}:${p.id}`)}
                  className="touch-target min-h-[56px] rounded-md bg-muted px-[8px] text-surface-foreground hover:bg-border"
                  style={{ fontSize: menuTextPx(widthClass) }}
                >
                  {p.title}
                </button>
              ))}
            </div>
          </div>
        );
      case "setup":
        return <MenuRow entry={id} icon={Wrench} label={specText("cockpit.menu.setup")} onClick={() => closeSetupOffer(true)} />;
      case "settings":
        return (
          <div>
            <MenuHeading>{specText("cockpit.menu.settings")}</MenuHeading>
            <div className="grid grid-cols-2 gap-[8px]" data-menu-entry={id}>
              {tabs.map((tab) =>
                tab.icon ? (
                  <Card key={tab.id} title={tab.title} icon={tab.icon} current={tab.id === activeTabId} onClick={() => goTab(tab.id)} />
                ) : null,
              )}
            </div>
          </div>
        );
      case "exit":
        return <MenuRow entry={id} icon={LayoutDashboard} label={specText("cockpit.menu.exit")} onClick={() => window.location.assign("/")} />;
    }
  };

  return (
    <Sheet
      title={
        shown ? (
          <button
            type="button"
            onClick={() => setOpenPanel(null)}
            className="touch-target -ml-[4px] flex items-center gap-[4px] rounded-md px-[4px] hover:bg-muted"
          >
            <ArrowLeft className="h-[18px] w-[18px]" aria-hidden />
            {shown.p.title}
          </button>
        ) : (
          specText("cockpit.menu.go_to")
        )
      }
      onDismiss={closeQuickMenu}
    >
      {shown ? (
        <PluginFrameHost ext={shown.ext} title={shown.p.title} className="h-[22rem] w-full rounded-lg" />
      ) : (
        <>
          <div className="flex flex-col gap-[8px]" data-testid="quick-menu-entries">
            {entries.map((id) => (
              <div key={id} data-menu-slot={id}>
                {entry(id)}
              </div>
            ))}
          </div>
          {padConnected ? (
            <div className="mt-[12px] flex justify-center gap-[16px] font-mono text-[12px] text-muted-foreground" data-testid="pad-hints">
              <span>{specText("cockpit.menu.pad.select")}</span>
              <span>{specText("cockpit.menu.pad.back")}</span>
              <span>{specText("cockpit.menu.pad.move")}</span>
              <span>{specText("cockpit.menu.pad.close")}</span>
            </div>
          ) : null}
        </>
      )}
    </Sheet>
  );
}

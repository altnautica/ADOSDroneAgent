// The cockpit shell. The active screen is resolved from the navigator (top of
// the detail stack, or the active tab).
//
// The Feed (the flight screen) draws its own grid. Full screen (the kiosk, or
// the top bar's Full screen button) it takes the whole viewport and the shell
// adds no chrome, only the Menu its top bar opens. In a browser otherwise it
// sits beside the menu rail with no status strip or action bar. Every other
// screen renders framed: the status strip on top, the menu rail (a strip in
// portrait) beside it and the action bar below.

import { Suspense, lazy, useEffect, useRef } from "react";

import { ActionBar, type InputStatus } from "@/components/shell/action-bar";
import { renderProfile, renderProfileClass } from "@/lib/render-profile";
import { MenuRail } from "@/components/shell/menu-rail";
import { ReachNotice } from "@/components/shell/reach-notice";
import { RebootBanner } from "@/components/shell/reboot-banner";
import { StatusStrip } from "@/components/shell/status-strip";
import { BannerHost } from "@/components/shell/banner-host";
import { DirtyGuardDialog } from "@/components/shell/dirty-guard-dialog";
import { verifyAccess } from "@/shared/api/access";
import { setAuthChallengeHandler } from "@/shared/api-fetch";
import { useReachStore } from "@/stores/reach-store";
import { useStatusStore } from "@/stores/status-store";
import { useStatusPoll } from "@/hooks/use-status-poll";
import { BoxViewport, useBoxViewport } from "@/hooks/use-width-class";
import { useExtensionsStore } from "@/stores/extensions-store";
import { getScreen, homeTabFor, tabScreens } from "@/nav/registry";
import { useNodeIdentity } from "@/shared/use-profile";
import { activeScreenId, useNavStore } from "@/stores/nav-store";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import type { ScreenContext } from "@/nav/navigator";

// The quick menu loads the first time it is opened, outside the Feed's chunk.
const QuickMenu = lazy(() => import("@/components/shell/quick-menu").then((m) => ({ default: m.QuickMenu })));

export function CockpitShell({ input }: { input: InputStatus }) {
  const activeTabId = useNavStore((s) => s.activeTabId);
  const detailStack = useNavStore((s) => s.detailStack);
  const quickMenuOpen = useNavStore((s) => s.quickMenuOpen);
  const flightFullscreen = useCockpitUiStore((s) => s.flightFullscreen);
  const dispatch = useNavStore((s) => s.dispatch);
  const setVisibleTabs = useNavStore((s) => s.setVisibleTabs);
  const loadExtensions = useExtensionsStore((s) => s.load);

  // One status poll for the whole shell (strip, band, alerts, screens), and the
  // running extensions read once so their skills and panels are ready.
  useStatusPoll();
  useEffect(() => {
    void loadExtensions();
  }, [loadExtensions]);

  // Mid-session re-probe: a refused request asks the gate's probe whether the
  // credential is really gone. A refusal of that one request (a denied plugin
  // capability) leaves the operator where they are; only a refused probe
  // raises the reach notice and its PIN form, and drops the dead session.
  useEffect(() => {
    setAuthChallengeHandler((status) => {
      void verifyAccess().then((verdict) => {
        if (verdict === "locked") useReachStore.getState().report(status, false);
      });
    });
    return () => setAuthChallengeHandler(null);
  }, []);

  // Tell the navigator which tabs this node actually shows, so the panel's own
  // buttons step the same set the operator can see. Declared here rather than
  // in the rail because the rail can be collapsed, and navigation must not
  // depend on whether the menu happens to be on screen.
  // The profile's home tab is declared with them: it opens there unless the
  // operator chose a tab this profile shows.
  const { profile, role } = useNodeIdentity();
  const fcFamily = useStatusStore((s) => s.status?.fc_family ?? null);
  const visibleTabIds = tabScreens(profile, role, fcFamily).map((t) => t.id);
  const visibleKey = visibleTabIds.join(",");
  const home = homeTabFor(profile);
  useEffect(() => {
    setVisibleTabs(visibleKey ? visibleKey.split(",") : [], home);
  }, [visibleKey, home, setVisibleTabs]);

  const screen = getScreen(activeScreenId({ activeTabId, detailStack }));
  // A full-bleed screen only paints edge-to-edge when it is the top surface
  // and the cockpit is full screen (the kiosk, or the top bar's Full screen
  // button); a detail screen pushed over it renders framed.
  const onTop = Boolean(screen.fullBleed) && detailStack.length === 0;
  const fullBleed = onTop && flightFullscreen;
  // The Feed takes its width class from its own box, which beside the menu
  // rail is narrower than the window.
  const feedBox = useRef<HTMLElement>(null);
  const feedViewport = useBoxViewport(feedBox, onTop);
  const ctx: ScreenContext = { dispatch };
  const body = <Suspense fallback={null}>{screen.render(ctx)}</Suspense>;
  const quickMenu = quickMenuOpen ? (
    <Suspense fallback={null}>
      <QuickMenu />
    </Suspense>
  ) : null;

  if (onTop) {
    // The Feed full screen is the whole viewport: no status strip, menu rail
    // or action bar (cockpit spec `legacy_chrome: "none"`); navigation is
    // behind the top bar's Menu. In a browser not full screen it sits beside
    // the menu rail. Either way the Feed's top bar and alert column carry the
    // reach and offline notices.
    // One tree shape for both: the rail's slot is null when full screen, so
    // the Feed under `main` keeps its place and never remounts on the toggle
    // or Escape (a remount releases the stick session and the vehicle brakes).
    return (
      <div
        className={`relative h-full w-full overflow-hidden bg-black text-foreground ${
          fullBleed ? "" : "flex landscape:flex-row portrait:flex-col-reverse "
        }${renderProfileClass(renderProfile())}`}
        data-shell={fullBleed ? "flight" : "flight-framed"}
      >
        {fullBleed ? null : <MenuRail />}
        <main ref={feedBox} className={fullBleed ? "relative h-full w-full" : "relative min-h-0 min-w-0 flex-1"}>
          <BoxViewport.Provider value={feedViewport}>{body}</BoxViewport.Provider>
        </main>
        {quickMenu}
        <DirtyGuardDialog />
      </div>
    );
  }

  return (
    <>
      <div
        className={`relative h-full w-full overflow-hidden bg-background text-foreground ${renderProfileClass(
          renderProfile(),
        )}`}
        data-shell="framed"
      >
        <div className="flex h-full w-full flex-col">
          <StatusStrip />
          <ReachNotice />
          <BannerHost />
          <RebootBanner />
          <div className="flex min-h-0 flex-1 landscape:flex-row portrait:flex-col-reverse">
            <MenuRail />
            <main className="relative min-h-0 min-w-0 flex-1 p-[0.4rem]">{body}</main>
          </div>
          <ActionBar input={input} />
        </div>
        {quickMenu}
        <DirtyGuardDialog />
      </div>
    </>
  );
}

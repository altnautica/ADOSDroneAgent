// The cockpit shell. The active screen is resolved from the navigator (top of
// the detail stack, or the active tab).
//
// The Feed (the full-bleed flight screen) takes the whole viewport and draws
// its own grid; the shell adds no chrome around it, only the reach and
// offline notices under its top bar and the Menu (quick menu) its top bar
// opens. Every other screen renders framed: the status strip on top, the
// menu rail (a strip in portrait) beside it and the action bar below.

import { Suspense, lazy, useEffect } from "react";

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
import { useExtensionsStore } from "@/stores/extensions-store";
import { getScreen, homeTabFor, tabScreens } from "@/nav/registry";
import { useNodeIdentity } from "@/shared/use-profile";
import { activeScreenId, useNavStore } from "@/stores/nav-store";
import type { ScreenContext } from "@/nav/navigator";

// The quick menu loads the first time it is opened, outside the Feed's chunk.
const QuickMenu = lazy(() => import("@/components/shell/quick-menu").then((m) => ({ default: m.QuickMenu })));

export function CockpitShell({ input }: { input: InputStatus }) {
  const activeTabId = useNavStore((s) => s.activeTabId);
  const detailStack = useNavStore((s) => s.detailStack);
  const quickMenuOpen = useNavStore((s) => s.quickMenuOpen);
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
  // A full-bleed screen only paints edge-to-edge when it is the top surface; a
  // detail screen pushed over it renders framed.
  const fullBleed = Boolean(screen.fullBleed) && detailStack.length === 0;
  const ctx: ScreenContext = { dispatch };
  const body = <Suspense fallback={null}>{screen.render(ctx)}</Suspense>;

  if (fullBleed) {
    // The Feed is the whole viewport: no status strip, menu rail or action
    // bar (cockpit spec `legacy_chrome: "none"`). Navigation is behind the
    // top bar's Menu; the Feed shows the reach and offline notices in its
    // alert column.
    return (
      <div
        className={`relative h-full w-full overflow-hidden bg-black text-foreground ${renderProfileClass(renderProfile())}`}
        data-shell="flight"
      >
        {body}
        {quickMenuOpen ? (
          <Suspense fallback={null}>
            <QuickMenu />
          </Suspense>
        ) : null}
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
        {quickMenuOpen ? (
          <Suspense fallback={null}>
            <QuickMenu />
          </Suspense>
        ) : null}
        <DirtyGuardDialog />
      </div>
    </>
  );
}

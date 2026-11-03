// The cockpit shell — the full-bleed layer stack plus the persistent chrome.
//
// Layers, back to front:
//   L0 video   — the Feed screen's edge-to-edge WHEP feed (full-bleed screens)
//   L1 content — a framed screen body (non-full-bleed screens)
//   L2 HUD     — the Feed screen's telemetry overlay
//   L3 chrome  — the status strip (top), menu (rail/strip), action bar (bottom)
//
// The active screen is resolved from the navigator (top of the detail stack, or
// the active tab). A full-bleed screen paints its own L0/L2 and the chrome
// floats translucently on top; a framed screen renders in the content region
// with solid chrome. The menu can collapse to give the feed the whole panel.

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
  const menuCollapsed = useNavStore((s) => s.menuCollapsed);
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

  const menuStrip = (
    <div className="flex min-h-0 flex-1 landscape:flex-row portrait:flex-col-reverse">
      {!menuCollapsed || !fullBleed ? (
        <MenuRail floating={fullBleed} />
      ) : null}
      {fullBleed ? (
        <div className="flex-1" />
      ) : (
        <main className="relative min-h-0 min-w-0 flex-1 p-[0.4rem]">{body}</main>
      )}
    </div>
  );

  return (
    <>
      <div
        className={`relative h-full w-full overflow-hidden bg-background text-foreground ${renderProfileClass(
          renderProfile(),
        )}`}
      >
        {fullBleed ? body : null}
        <div
          className={
            fullBleed
              ? "pointer-events-none absolute inset-0 flex flex-col"
              : "flex h-full w-full flex-col"
          }
        >
          <StatusStrip floating={fullBleed} />
          <ReachNotice />
          <BannerHost fullBleed={fullBleed} />
          <RebootBanner floating={fullBleed} />
          {menuStrip}
          <ActionBar floating={fullBleed} input={input} />
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

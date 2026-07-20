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

import { ActionBar, type InputStatus } from "@/components/shell/action-bar";
import { MenuRail } from "@/components/shell/menu-rail";
import { QuickMenu } from "@/components/shell/quick-menu";
import { RebootBanner } from "@/components/shell/reboot-banner";
import { StatusStrip } from "@/components/shell/status-strip";
import { TelemetryProvider } from "@/hooks/telemetry-context";
import { getScreen } from "@/nav/registry";
import { activeScreenId, useNavStore } from "@/stores/nav-store";
import type { ScreenContext } from "@/nav/navigator";

export function CockpitShell({ input }: { input: InputStatus }) {
  const activeTabId = useNavStore((s) => s.activeTabId);
  const detailStack = useNavStore((s) => s.detailStack);
  const menuCollapsed = useNavStore((s) => s.menuCollapsed);
  const quickMenuOpen = useNavStore((s) => s.quickMenuOpen);
  const dispatch = useNavStore((s) => s.dispatch);

  const screen = getScreen(activeScreenId({ activeTabId, detailStack }));
  // A full-bleed screen only paints edge-to-edge when it is the top surface; a
  // detail screen pushed over it renders framed.
  const fullBleed = Boolean(screen.fullBleed) && detailStack.length === 0;
  const ctx: ScreenContext = { dispatch };
  const body = screen.render(ctx);

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
    <TelemetryProvider>
      <div className="relative h-full w-full overflow-hidden bg-background text-foreground">
        {fullBleed ? body : null}
        <div
          className={
            fullBleed
              ? "pointer-events-none absolute inset-0 flex flex-col"
              : "flex h-full w-full flex-col"
          }
        >
          <StatusStrip floating={fullBleed} />
          <RebootBanner floating={fullBleed} />
          {menuStrip}
          <ActionBar floating={fullBleed} input={input} />
        </div>
        {quickMenuOpen ? <QuickMenu /> : null}
      </div>
    </TelemetryProvider>
  );
}

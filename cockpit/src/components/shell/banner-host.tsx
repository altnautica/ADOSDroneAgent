// The shell's banner slot, beside the reboot banner and the reach notice:
// agent unreachable, a drone's flight controller disconnected, and the
// one-shot cloud-posture nudge. A full-bleed screen (the Feed) has its own
// alert stack, so there only the agent-offline banner shows. The `err` and
// `warn` banners wrap to two lines and size in px times the width class's
// chrome scale; the advisory cloud line stays on one truncated line. Off the
// flight screen it also carries the one-time aircraft setup offer.

import { AlertCircle, AlertTriangle, Cloud } from "lucide-react";

import { SetupOfferBanner } from "@/components/shell/setup-offer-banner";
import { useCloudPostureNudge } from "@/hooks/use-cloud-posture-nudge";
import { useWidthClass } from "@/hooks/use-width-class";
import { gridMeasures } from "@/lib/cockpit-layout";
import { bannerTextStyle, shellBanners } from "@/lib/shell-banners";
import { cn } from "@/lib/utils";
import { useProfile } from "@/shared/use-profile";
import { useNavStore } from "@/stores/nav-store";
import { useReachStore } from "@/stores/reach-store";
import { useStatusStore } from "@/stores/status-store";

export function BannerHost({ fullBleed = false }: { fullBleed?: boolean }) {
  const stale = useStatusStore((s) => s.stale);
  const hasStatus = useStatusStore((s) => s.status != null);
  const fcConnected = useStatusStore((s) => s.status?.fc_connected);
  const refusal = useReachStore((s) => s.refusal);
  const profile = useProfile();
  const goTab = useNavStore((s) => s.goTab);
  const nudge = useCloudPostureNudge();
  const { widthClass } = useWidthClass();
  const scale = gridMeasures(widthClass, false).scale;

  const banners = shellBanners({ statusStale: stale, refusal, profile, fcConnected, hasStatus }).filter(
    (b) => !fullBleed || b.id === "agent-offline",
  );
  const showNudge = nudge.visible && !fullBleed;
  // The setup offer stays off the flight screen: a full-bleed host shows it never.
  const setupOffer = fullBleed ? null : <SetupOfferBanner />;
  if (banners.length === 0 && !showNudge && !setupOffer) return null;

  return (
    <div className={cn("flex shrink-0 flex-col", fullBleed && "pointer-events-auto")}>
      {banners.map((b) => {
        const Icon = b.severity === "err" ? AlertCircle : AlertTriangle;
        return (
          <div
            key={b.id}
            role="status"
            className={cn("flex items-center font-medium", b.severity === "err" ? "bg-err/20 text-err" : "bg-warn/20 text-warn")}
            style={{ gap: 8 * scale, padding: `${5 * scale}px ${12 * scale}px` }}
          >
            <Icon className="shrink-0" style={{ width: 16 * scale, height: 16 * scale }} aria-hidden />
            <span className="min-w-0" style={bannerTextStyle(scale)} data-testid="shell-banner-text">
              {b.message}
            </span>
          </div>
        );
      })}
      {setupOffer}
      {showNudge ? (
        <div role="status" className="flex items-center gap-[0.5rem] bg-primary/10 px-[0.75rem] py-[0.3rem] text-[0.8rem]">
          <Cloud className="h-[1rem] w-[1rem] shrink-0 text-hud-primary" aria-hidden />
          <div className="min-w-0 flex-1">
            <div className="truncate font-medium text-foreground">Cloud relay is on by default for upgraded agents.</div>
            <div className="truncate text-muted-foreground">
              New installs default to local-only. Switch if this node never needs to be reached from outside the LAN.
            </div>
          </div>
          <button
            type="button"
            onClick={() => {
              nudge.ack();
              goTab("settings");
            }}
            className="touch-target rounded-md border border-border px-[0.6rem] text-[0.78rem] font-semibold text-foreground"
          >
            Open settings
          </button>
          <button
            type="button"
            onClick={nudge.ack}
            className="touch-target rounded-md px-[0.6rem] text-[0.78rem] text-muted-foreground hover:text-foreground"
          >
            Got it
          </button>
        </div>
      ) : null}
    </div>
  );
}

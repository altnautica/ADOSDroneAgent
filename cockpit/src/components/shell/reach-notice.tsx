// What the panel says when the agent is refusing it, rather than dashing every
// field and leaving the operator to guess.
//
// A refused call and an absent value look identical once they reach a surface:
// both render as an em dash. That is the right shape for a reading the agent
// genuinely does not have, and the wrong one for a node that declined to
// answer, because only the second has something the operator can act on. An
// unpaired node in particular answers exactly one route — its own identity —
// and that response already carries the code that would pair it, so the panel
// has the way out in hand while showing a screen full of dashes.
//
// Rendered as a band above the screen rather than in place of it: the shell,
// the clock and the menu are still true, and replacing a working surface with a
// modal would take away the operator's ability to look around the node while
// they deal with it.

import { Suspense, lazy } from "react";
import { Trans, useTranslation } from "react-i18next";

import "@/shared/i18n";
import { useReachStore } from "@/stores/reach-store";

// The PIN form carries the access locale, so it loads only when a refusal
// actually shows it, never in the shell's own chunk.
const SetUpAccess = lazy(() => import("@/components/shell/set-up-access").then((m) => ({ default: m.SetUpAccess })));

export function ReachNotice() {
  const { t } = useTranslation();
  const refusal = useReachStore((s) => s.refusal);
  const pairingCode = useReachStore((s) => s.pairingCode);

  if (refusal === "none") return null;

  const unpaired = refusal === "unpaired";

  return (
    <div
      role="status"
      className="flex shrink-0 flex-col items-center gap-y-[0.35rem] border-b border-primary/40 bg-primary/10 px-[1rem] py-[0.5rem] text-center text-[0.8rem]"
    >
      <div className="flex w-full flex-wrap items-center justify-center gap-x-[0.75rem] gap-y-[0.25rem]">
        <span className="font-semibold text-hud-primary">
          {unpaired ? t("cockpit.shell.reach.unpairedTitle") : t("cockpit.shell.reach.signedOutTitle")}
        </span>
        <span className="text-muted-foreground">
          {unpaired ? t("cockpit.shell.reach.unpairedBody") : t("cockpit.shell.reach.signedOutBody")}
        </span>
        {unpaired && pairingCode ? (
          <span className="text-muted-foreground">
            <Trans
              i18nKey="cockpit.shell.reach.pairWithCode"
              values={{ code: pairingCode }}
              components={{ code: <span className="select-text font-mono font-semibold text-foreground" /> }}
            />
          </span>
        ) : null}
        {!unpaired ? (
          <span className="text-muted-foreground">{t("cockpit.shell.reach.enterPin")}</span>
        ) : null}
      </div>
      {/* The way past either refusal from a trusted-LAN browser: set or enter
          the node's access PIN, which mints a short-lived session the agent
          accepts as the data-plane credential. */}
      <div className="flex w-full max-w-[34rem] justify-center">
        <Suspense fallback={null}>
          <SetUpAccess />
        </Suspense>
      </div>
    </div>
  );
}

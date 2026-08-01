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

import { useReachStore } from "@/stores/reach-store";
import { SetUpAccess } from "@/components/shell/set-up-access";

export function ReachNotice() {
  const refusal = useReachStore((s) => s.refusal);
  const pairingCode = useReachStore((s) => s.pairingCode);

  if (refusal === "none") return null;

  const unpaired = refusal === "unpaired";

  return (
    <div
      role="status"
      className="flex shrink-0 flex-col items-center gap-y-[0.35rem] border-b border-amber/40 bg-amber/10 px-[1rem] py-[0.5rem] text-center text-[0.8rem]"
    >
      <div className="flex w-full flex-wrap items-center justify-center gap-x-[0.75rem] gap-y-[0.25rem]">
        <span className="font-semibold text-amber">
          {unpaired ? "This node is not paired" : "Not signed in to this node"}
        </span>
        <span className="text-muted-foreground">
          {unpaired
            ? "It answers its own identity and nothing else, so every reading below is unavailable rather than empty."
            : "It is paired, and this browser holds no credential for it, so its readings are unavailable rather than empty."}
        </span>
        {unpaired && pairingCode ? (
          <span className="text-muted-foreground">
            Pair it from Mission Control with code{" "}
            <span className="font-mono font-semibold text-foreground">
              {pairingCode}
            </span>
            , or reach it over its hotspot or a USB cable.
          </span>
        ) : null}
        {!unpaired ? (
          <span className="text-muted-foreground">
            Sign in on this node&apos;s dashboard and return here.
          </span>
        ) : null}
      </div>
      {/* The founder's route past an unpaired door: set or enter the dashboard
          PIN to unlock the DATA surfaces from this trusted-LAN browser, exactly
          as they chose over blind trusted-network-open. */}
      {unpaired ? (
        <div className="flex w-full max-w-[34rem] justify-center">
          <SetUpAccess />
        </div>
      ) : null}
    </div>
  );
}

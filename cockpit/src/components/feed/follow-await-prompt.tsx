// The waiting-for-instructions prompt: while follow-me holds at its latched
// point for the operator (`holding-await-operator`), a banner under the follow
// strip names why and offers the actions `followPrompt` allows: search again,
// pick a target, return home, land, stop following. Modal-free, so the video
// and the columns stay live. It never takes focus on its own: R, P, H, L and
// X answer only once the operator has put focus in it (a click, Tab, the
// controller's UI navigation), and a key pressed anywhere else goes on to its
// usual owner. Return home and land request the same built-in as the left
// column, so its gate and its hold-to-confirm apply to a tap and a key alike.
// The touch targets grow with the width class, so the kiosk's are the largest.

import { useRef, type KeyboardEvent } from "react";

import { useNow } from "@/hooks/use-now";
import { skillContextNow } from "@/hooks/use-skill-context";
import { putPluginConfig } from "@/lib/api";
import type { WidthClass } from "@/lib/cockpit-layout";
import { FOLLOW_ACTION_KEYS, followActionForKey, runFollowAction } from "@/lib/follow-actions";
import { railButtonPx } from "@/lib/follow-rail";
import { requestSkill } from "@/lib/skill-runner";
import { BUILTIN_SKILL_BY_ID } from "@/lib/skills";
import { cn } from "@/lib/utils";
import { toast } from "@/shared/api/toast";
import { followPrompt, type FollowPromptAction } from "@/shared/hud/follow-prompt";
import { specText } from "@/shared/hud/spec-text";
import { FOLLOW_FRESH_MS } from "@/shared/hud/target-state";
import { useNavStreamStore } from "@/stores/nav-stream-store";
import { useSettingsStore } from "@/stores/settings-store";

/** Pick a target: drop the designation, so the next box tapped on the video
 *  (Follow) designates afresh and follow-me re-locks on it. */
function startPick(): void {
  useNavStreamStore.getState().designate(null);
  toast.info(specText("follow.prompt.action.redesignate"));
}

export function FollowAwaitPrompt({ widthClass }: { widthClass: WidthClass }) {
  const follow = useNavStreamStore((s) => s.follow);
  const glove = useSettingsStore((s) => s.glove);
  const now = useNow(follow ? 1000 : null);
  const ref = useRef<HTMLDivElement>(null);

  const fresh = follow && now - follow.at <= FOLLOW_FRESH_MS ? follow.data : null;
  const prompt = fresh ? followPrompt(fresh) : null;
  const seq = fresh?.operatorResumeSeq ?? 0;
  if (!prompt) return null;

  /** The left column's path: gate, then the hold-to-confirm sheet. The sheet
   *  answers Enter only from the page or itself, so a prompt that held the
   *  keyboard hands it back. */
  const flight = (skillId: "rth" | "land"): boolean => {
    const opened = requestSkill(BUILTIN_SKILL_BY_ID[skillId], skillContextNow());
    const el = document.activeElement;
    if (opened && el instanceof HTMLElement && ref.current?.contains(el)) el.blur();
    return opened;
  };

  const run = (action: FollowPromptAction) => {
    void runFollowAction(action, { put: putPluginConfig, flight, startPick, seq }).then((r) => {
      if (!r.ok) toast.err(specText(`follow.prompt.action.${action}`), r.err);
    });
  };

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.repeat) return;
    const action = followActionForKey(e, prompt.actions);
    if (!action) return;
    e.preventDefault();
    e.stopPropagation();
    run(action);
  };

  const size = railButtonPx(widthClass, glove);
  return (
    <div
      ref={ref}
      role="alert"
      aria-labelledby="follow-prompt-title"
      tabIndex={0}
      onKeyDown={onKeyDown}
      data-zone="follow_prompt"
      className="chrome-panel pointer-events-auto flex max-w-[min(92vw,720px)] flex-col gap-[8px] border border-hud-crit/70 p-[10px] outline-none focus-visible:ring-2 focus-visible:ring-hud-crit"
    >
      <div className="flex flex-col gap-[2px]">
        <span id="follow-prompt-title" className={cn("font-semibold text-hud-crit", widthClass === "tv" ? "text-[20px]" : "text-[15px]")}>
          {specText(prompt.titleKey)}
        </span>
        <span className={cn("text-hud-ink-2", widthClass === "tv" ? "text-[16px]" : "text-[13px]")}>{specText(prompt.bodyKey)}</span>
      </div>
      <div className="flex flex-wrap gap-[6px]">
        {prompt.actions.map((a) => (
          <button
            key={a}
            type="button"
            // Space on a focused button presses that button, never Pause
            // (`use-skill-keys`): a tap leaves focus where it was, and a
            // button pressed from the keyboard hands it to the prompt.
            onMouseDown={(e) => e.preventDefault()}
            onClick={(e) => {
              if (document.activeElement === e.currentTarget) ref.current?.focus();
              run(a);
            }}
            aria-keyshortcuts={FOLLOW_ACTION_KEYS[a].toUpperCase()}
            style={{ minHeight: size, minWidth: size * 2 }}
            className={cn(
              "flex items-center justify-center gap-[6px] rounded-md border px-[10px] font-semibold",
              widthClass === "tv" ? "text-[16px]" : "text-[13px]",
              a === "stop" ? "border-hud-crit/60 text-hud-crit" : "border-hud-hair text-hud-ink hover:bg-hud-hair",
            )}
          >
            <span>{specText(`follow.prompt.action.${a}`)}</span>
            <kbd aria-hidden className="rounded border border-hud-hair px-[4px] font-mono text-[11px] text-hud-ink-2">
              {FOLLOW_ACTION_KEYS[a].toUpperCase()}
            </kbd>
          </button>
        ))}
      </div>
    </div>
  );
}

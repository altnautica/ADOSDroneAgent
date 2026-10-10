// Keyboard chords and gamepad bindings for the Skill Bar, active while the
// Feed is mounted. Both route through `requestSkill`, so the gates and the
// confirm tier match a tap on the bar; the button that opened a sheet also
// completes its hold.
//
// Keyboard: Shift+A arm/disarm, Shift+T takeoff, Shift+L land, Shift+R RTL,
// Shift+P pause, Shift+X kill. Gamepad: the per-skill buttons from settings.

import { useEffect, useRef } from "react";

import { useSkillContext } from "@/hooks/use-skill-context";
import { onGamepadButtons, risingEdges } from "@/lib/gamepad-bus";
import { requestSkill } from "@/lib/skill-runner";
import { CORE_BY_ID, type Skill, type SkillContext } from "@/lib/skills";
import { useConfirmStore } from "@/stores/confirm-store";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useSettingsStore } from "@/stores/settings-store";

const KEY_CHORDS: Record<string, string> = {
  A: "arm",
  T: "takeoff",
  L: "land",
  R: "rtl",
  P: "pause",
  X: "kill",
};

/** The skill a binding id resolves to now: `arm` follows the arm state. */
export function skillForBinding(id: string, ctx: SkillContext, extensionSkills: Skill[]): Skill | null {
  if (id === "arm") return CORE_BY_ID[ctx.armed ? "disarm" : "arm"];
  return CORE_BY_ID[id] ?? extensionSkills.find((s) => s.id === id) ?? null;
}

export function useSkillKeys(): void {
  const ctx = useSkillContext();
  const ctxRef = useRef(ctx);
  useEffect(() => {
    ctxRef.current = ctx;
  }, [ctx]);

  useEffect(() => {
    const extensionSkills = () => useExtensionsStore.getState().extensions.flatMap((e) => e.skills);

    const onKey = (e: KeyboardEvent) => {
      if (!e.shiftKey || e.ctrlKey || e.metaKey || e.altKey || e.repeat) return;
      if (useConfirmStore.getState().pending) return;
      const target = e.target as HTMLElement | null;
      if (target && (target.tagName === "INPUT" || target.tagName === "TEXTAREA")) return;
      const id = KEY_CHORDS[e.key.toUpperCase()];
      if (!id) return;
      const skill = skillForBinding(id, ctxRef.current, extensionSkills());
      if (skill && requestSkill(skill, ctxRef.current)) e.preventDefault();
    };

    const offPad = onGamepadButtons((pressed, prev) => {
      if (useConfirmStore.getState().pending) return;
      const bindings = useSettingsStore.getState().skillButtons;
      for (const button of risingEdges(pressed, prev)) {
        const id = Object.keys(bindings).find((k) => bindings[k] === button);
        if (!id) continue;
        const skill = skillForBinding(id, ctxRef.current, extensionSkills());
        if (skill) requestSkill(skill, ctxRef.current, button);
      }
    });

    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("keydown", onKey);
      offPad();
    };
  }, []);
}

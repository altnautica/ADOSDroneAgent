// Gamepad skill buttons: which controller button fires each Skill Bar skill
// on the Feed. "Assign" waits for the next button press; the menu buttons
// (A, B, Start, D-pad) stay reserved for navigation. Extension skills are
// listed alongside the built-ins.

import { useEffect, useState } from "react";

import { NAV_BUTTONS } from "@/hooks/use-gamepad";
import { onGamepadButtons, risingEdges } from "@/lib/gamepad-bus";
import { CORE_BY_ID } from "@/lib/skills";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useSettingsStore } from "@/stores/settings-store";

const BUILTIN_ROWS: { id: string; label: string }[] = [
  { id: "arm", label: "Arm / Disarm" },
  { id: "takeoff", label: CORE_BY_ID.takeoff.label },
  { id: "land", label: CORE_BY_ID.land.label },
  { id: "rtl", label: CORE_BY_ID.rtl.label },
  { id: "pause", label: CORE_BY_ID.pause.label },
  { id: "resume", label: CORE_BY_ID.resume.label },
  { id: "kill", label: CORE_BY_ID.kill.label },
];

export function GamepadBindings() {
  const bindings = useSettingsStore((s) => s.skillButtons);
  const bind = useSettingsStore((s) => s.bindSkillButton);
  const reset = useSettingsStore((s) => s.resetSkillButtons);
  const extensions = useExtensionsStore((s) => s.extensions);
  const [capturing, setCapturing] = useState<string | null>(null);

  useEffect(() => {
    if (!capturing) return;
    const off = onGamepadButtons((pressed, prev) => {
      const button = risingEdges(pressed, prev).find((b) => !NAV_BUTTONS.includes(b));
      if (button === undefined) return;
      bind(capturing, button);
      setCapturing(null);
    });
    const timeout = setTimeout(() => setCapturing(null), 8000);
    return () => {
      off();
      clearTimeout(timeout);
    };
  }, [capturing, bind]);

  const rows = [
    ...BUILTIN_ROWS,
    ...extensions.flatMap((e) => e.skills.map((s) => ({ id: s.id, label: `${s.label} (${e.name})` }))),
  ];

  return (
    <div className="flex flex-col gap-[0.3rem]">
      <p className="text-[0.75rem] text-muted-foreground">
        A, B, Start and the D-pad navigate the menus and cannot be assigned.
      </p>
      {rows.map((row) => {
        const button = bindings[row.id];
        const waiting = capturing === row.id;
        return (
          <div
            key={row.id}
            className="flex min-h-[48px] items-center gap-[0.6rem] rounded-md bg-input/30 px-[0.7rem] py-[0.3rem]"
          >
            <span className="min-w-0 flex-1 truncate text-[0.9rem] text-surface-foreground">{row.label}</span>
            <span className="font-mono text-[0.8rem] text-muted-foreground" aria-live="polite">
              {waiting ? "Press a button…" : button === undefined ? "Unassigned" : `Button ${button}`}
            </span>
            <button
              type="button"
              onClick={() => setCapturing(waiting ? null : row.id)}
              className="touch-target rounded-md bg-primary/20 px-[0.7rem] text-[0.8rem] text-surface-foreground hover:bg-primary/30"
            >
              {waiting ? "Cancel" : "Assign"}
            </button>
            {button !== undefined ? (
              <button
                type="button"
                onClick={() => bind(row.id, null)}
                className="touch-target rounded-md px-[0.7rem] text-[0.8rem] text-muted-foreground hover:bg-muted"
              >
                Clear
              </button>
            ) : null}
          </div>
        );
      })}
      <button
        type="button"
        onClick={reset}
        className="touch-target mt-[0.3rem] self-start rounded-md border border-border px-[0.8rem] text-[0.8rem] text-surface-foreground hover:bg-muted"
      >
        Restore defaults
      </button>
    </div>
  );
}

// The Feed's view, layer, profile and sheet actions (cockpit spec `ds4.json`
// and `views.json`), registered while the Feed is mounted so every controller
// binding and keyboard key fires the same code: Triangle / `V` cycles the
// view, L3 swaps video and map, the D-pad (picker closed) steps the layer
// level, F1–F3 and Options + D-pad pick the flight profile, Options (the
// spec's `quick_settings`) opens the Menu, and the VTOL transition goes to
// whichever flight the aircraft is not in.

import type { NavCommand } from "@/nav/navigator";
import { useNavStore } from "@/stores/nav-store";
import { PROFILE_KEYS } from "@/lib/cockpit-spec";
import { registerCockpitActions, runCockpitAction, type CockpitAction } from "@/lib/cockpit-actions";
import { prevLevel } from "@/lib/cockpit-view";
import { sendPilotAction } from "@/lib/pilot-link";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";
import { specText } from "@/shared/hud/spec-text";

export const PROFILES = ["cine", "normal", "sport"] as const;

/** The profile one step from `current` (clamped at the ends). */
export function stepProfile(current: string | null, dir: 1 | -1): (typeof PROFILES)[number] {
  const i = PROFILES.indexOf((current ?? "normal") as (typeof PROFILES)[number]);
  return PROFILES[Math.min(PROFILES.length - 1, Math.max(0, (i < 0 ? 1 : i) + dir))];
}

const press = (fn: () => void) => (phase: string) => {
  if (phase === "press") fn();
};

export function registerFeedActions(): () => void {
  const view = useCockpitViewStore.getState;
  const actions: CockpitAction[] = [
    { id: "view.next", label: "Next view", group: "view", run: press(() => view().cycleView()) },
    {
      id: "swap_map",
      label: specText("cockpit.pip.swap"),
      group: "view",
      run: press(() => view().setView(view().view === "map" ? "pilot" : "map")),
    },
    { id: "layer.level.next", label: "Layer level", group: "view", run: press(() => view().cycleLevel()) },
    {
      id: "layer.level.prev",
      label: "Layer level (back)",
      group: "view",
      run: press(() => view().setLevel(prevLevel(view().level))),
    },
    { id: "quick_settings", label: "Quick settings", group: "view", run: press(() => useNavStore.setState({ quickMenuOpen: true })) },
    ...PROFILES.map(
      (p): CockpitAction => ({
        id: `profile:${p}`,
        label: `Profile ${p}`,
        group: "profile",
        run: press(() => void sendPilotAction(`profile:${p}`)),
      }),
    ),
    ...([1, -1] as const).map(
      (dir): CockpitAction => ({
        id: dir === 1 ? "profile.next" : "profile.prev",
        label: dir === 1 ? "Next profile" : "Previous profile",
        group: "profile",
        run: press(() => {
          const cur = useNavStreamStore.getState().pilot?.data.profile ?? null;
          void sendPilotAction(`profile:${stepProfile(cur, dir)}`);
        }),
      }),
    ),
    {
      id: "vtol.transition",
      label: "VTOL transition",
      group: "flight",
      run: press(() => {
        const p = useNavStreamStore.getState().pilot?.data;
        if (p?.vehicleClass !== "vtol") return;
        // Q modes are the hover; anything else is forward flight.
        const hovering = (p.fcModeName ?? "").toUpperCase().startsWith("Q");
        void sendPilotAction(hovering ? "vtol:fw" : "vtol:mc");
      }),
    },
  ];
  const offActions = registerCockpitActions(actions);

  // Keyboard: `V` cycles the view, F1–F3 pick the profile.
  const onKey = (e: KeyboardEvent) => {
    if (e.repeat || e.ctrlKey || e.metaKey || e.altKey) return;
    const t = e.target;
    if (t instanceof HTMLElement && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable)) return;
    const key = e.key.toLowerCase();
    if (key === "v" && !e.shiftKey) {
      runCockpitAction("view.next");
      e.preventDefault();
      return;
    }
    const profile = profileActionForKey(key);
    if (profile) {
      runCockpitAction(profile);
      e.preventDefault();
    }
  };
  window.addEventListener("keydown", onKey);
  return () => {
    offActions();
    window.removeEventListener("keydown", onKey);
  };
}

/** F1–F3 → the profile action. The spec's values are already the action ids
 *  (`profile:cine`), so they pass through unprefixed. */
export function profileActionForKey(key: string): string | null {
  return PROFILE_KEYS[key.toLowerCase()] ?? null;
}

/** The Feed's answer to the panel buttons and encoder (registered on the
 *  command stack while the Feed is mounted). The Feed draws no menu rail, so
 *  stepping its focus would move a ring nobody sees and the next push would
 *  leave the flight screen unannounced: prev/next are taken and do nothing,
 *  and activate opens the Menu, where the focus is drawn. Overlays opened over
 *  the Feed (the Menu, sheets) sit above this on the stack and take the
 *  commands first. */
export function feedNavCommand(cmd: NavCommand): boolean {
  if (cmd === "prev" || cmd === "next") return true;
  if (cmd === "activate") {
    useNavStore.setState({ quickMenuOpen: true });
    return true;
  }
  return false;
}

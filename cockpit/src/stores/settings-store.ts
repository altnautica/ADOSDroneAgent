// Local cockpit UI preferences, persisted to localStorage. The UI-scale knob
// multiplies the fluid root font-size (styles/globals.css `--ui-scale`) so an
// operator can size the whole layout up or down for their panel and eyesight
// without changing the layout itself.

import { create } from "zustand";

const PERSIST_KEY = "ados-cockpit-ui-scale";

/** Clamp to a sane range so the knob can never make the panel unusable. */
export const UI_SCALE_MIN = 0.7;
export const UI_SCALE_MAX = 1.6;
export const UI_SCALE_STEP = 0.1;

function clampScale(v: number): number {
  if (!Number.isFinite(v)) return 1;
  return Math.min(UI_SCALE_MAX, Math.max(UI_SCALE_MIN, Math.round(v * 100) / 100));
}

function loadScale(): number {
  if (typeof localStorage === "undefined") return 1;
  try {
    const raw = localStorage.getItem(PERSIST_KEY);
    if (raw == null) return 1;
    return clampScale(Number(raw));
  } catch {
    return 1;
  }
}

function persistScale(v: number): void {
  if (typeof localStorage === "undefined") return;
  try {
    localStorage.setItem(PERSIST_KEY, String(v));
  } catch {
    // no-op
  }
}

const BINDINGS_KEY = "ados-cockpit-skill-buttons";

/** Default gamepad skill buttons. A and B drive menu navigation in this
 *  cockpit, so arm and RTL sit on the shoulder buttons; land and pause keep
 *  the face buttons the other cockpits use. `arm` drives arm or disarm,
 *  whichever applies. */
export const DEFAULT_SKILL_BUTTONS: Record<string, number> = {
  land: 2,
  pause: 3,
  arm: 4,
  rtl: 5,
};

function loadBindings(): Record<string, number> {
  if (typeof localStorage === "undefined") return { ...DEFAULT_SKILL_BUTTONS };
  try {
    const raw = localStorage.getItem(BINDINGS_KEY);
    if (!raw) return { ...DEFAULT_SKILL_BUTTONS };
    const parsed = JSON.parse(raw) as Record<string, unknown>;
    const out: Record<string, number> = {};
    for (const [k, v] of Object.entries(parsed)) {
      if (typeof v === "number" && Number.isInteger(v) && v >= 0) out[k] = v;
    }
    return out;
  } catch {
    return { ...DEFAULT_SKILL_BUTTONS };
  }
}

function persistBindings(b: Record<string, number>): void {
  if (typeof localStorage === "undefined") return;
  try {
    localStorage.setItem(BINDINGS_KEY, JSON.stringify(b));
  } catch {
    // no-op
  }
}

const ALT_REF_KEY = "ados-cockpit-alt-ref";
export type AltRef = "rel" | "msl";

function loadAltRef(): AltRef {
  try {
    return typeof localStorage !== "undefined" && localStorage.getItem(ALT_REF_KEY) === "msl" ? "msl" : "rel";
  } catch {
    return "rel";
  }
}

interface SettingsState {
  /** Altitude tape reference: home-relative or above mean sea level. */
  altRef: AltRef;
  setAltRef: (ref: AltRef) => void;
  uiScale: number;
  setUiScale: (value: number) => void;
  nudgeUiScale: (delta: number) => void;
  /** Gamepad button index per skill id. One button drives one skill. */
  skillButtons: Record<string, number>;
  bindSkillButton: (skillId: string, button: number | null) => void;
  resetSkillButtons: () => void;
}

export const useSettingsStore = create<SettingsState>((set, get) => ({
  uiScale: loadScale(),
  altRef: loadAltRef(),
  setAltRef: (ref) => {
    try {
      localStorage.setItem(ALT_REF_KEY, ref);
    } catch {
      // Storage disabled: the choice lasts for this session.
    }
    set({ altRef: ref });
  },
  setUiScale: (value) => {
    const v = clampScale(value);
    persistScale(v);
    set({ uiScale: v });
  },
  nudgeUiScale: (delta) => get().setUiScale(get().uiScale + delta),
  skillButtons: loadBindings(),
  bindSkillButton: (skillId, button) => {
    const next: Record<string, number> = {};
    for (const [k, v] of Object.entries(get().skillButtons)) {
      if (k !== skillId && v !== button) next[k] = v;
    }
    if (button !== null) next[skillId] = button;
    persistBindings(next);
    set({ skillButtons: next });
  },
  resetSkillButtons: () => {
    persistBindings({ ...DEFAULT_SKILL_BUTTONS });
    set({ skillButtons: { ...DEFAULT_SKILL_BUTTONS } });
  },
}));

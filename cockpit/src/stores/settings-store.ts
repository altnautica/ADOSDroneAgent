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

interface SettingsState {
  uiScale: number;
  setUiScale: (value: number) => void;
  nudgeUiScale: (delta: number) => void;
}

export const useSettingsStore = create<SettingsState>((set, get) => ({
  uiScale: loadScale(),
  setUiScale: (value) => {
    const v = clampScale(value);
    persistScale(v);
    set({ uiScale: v });
  },
  nudgeUiScale: (delta) => get().setUiScale(get().uiScale + delta),
}));

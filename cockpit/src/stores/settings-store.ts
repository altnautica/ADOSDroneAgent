// Local cockpit UI preferences, persisted to localStorage. The UI-scale knob
// multiplies the fluid root font-size (styles/globals.css `--ui-scale`) so an
// operator can size the whole layout up or down for their panel and eyesight
// without changing the layout itself. The theme picks the token set
// (`data-theme` on the root) and glove mode grows every touch target.

import { create } from "zustand";

import type { MinimapPin } from "@/shared/hud/minimap-mode";
import type { RadarScaleSetting } from "@/shared/hud/radar-scan";

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

const ALT_REF_KEY = "ados-cockpit-alt-ref";
export type AltRef = "rel" | "msl";

function loadAltRef(): AltRef {
  try {
    return typeof localStorage !== "undefined" && localStorage.getItem(ALT_REF_KEY) === "msl" ? "msl" : "rel";
  } catch {
    return "rel";
  }
}

function loadString(key: string): string | null {
  try {
    return typeof localStorage !== "undefined" ? localStorage.getItem(key) : null;
  } catch {
    return null;
  }
}

function saveString(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {
    // Storage disabled: the choice lasts for this session.
  }
}

const THEME_KEY = "ados-cockpit-theme";
const GLOVE_KEY = "ados-cockpit-glove";

/** The token sets the generated brand CSS carries. */
export type CockpitTheme = "dark" | "light" | "nvg";
export const COCKPIT_THEMES: readonly CockpitTheme[] = ["dark", "light", "nvg"];

function loadTheme(): CockpitTheme {
  const raw = loadString(THEME_KEY);
  return raw === "light" || raw === "nvg" ? raw : "dark";
}

const MINIMAP_PIN_KEY = "ados-cockpit-minimap-pin";
const RADAR_SCALE_KEY = "ados-cockpit-radar-scale";

function loadMinimapPin(): MinimapPin {
  const raw = loadString(MINIMAP_PIN_KEY);
  return raw === "local" || raw === "map" ? raw : "auto";
}

function loadRadarScale(): RadarScaleSetting {
  const n = Number(loadString(RADAR_SCALE_KEY));
  return n === 3 || n === 6 || n === 12 ? n : "auto";
}

interface SettingsState {
  /** Altitude tape reference: home-relative or above mean sea level. */
  altRef: AltRef;
  setAltRef: (ref: AltRef) => void;
  uiScale: number;
  setUiScale: (value: number) => void;
  nudgeUiScale: (delta: number) => void;
  theme: CockpitTheme;
  setTheme: (theme: CockpitTheme) => void;
  /** Glove mode: every touch target grows by the glove factor. */
  glove: boolean;
  setGlove: (on: boolean) => void;
  /** The minimap frame pin: AUTO (local without GPS), LOCAL or MAP. */
  minimapPin: MinimapPin;
  setMinimapPin: (pin: MinimapPin) => void;
  /** The proximity radar's display range, or `auto`. */
  radarScale: RadarScaleSetting;
  setRadarScale: (scale: RadarScaleSetting) => void;
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
  theme: loadTheme(),
  setTheme: (theme) => {
    saveString(THEME_KEY, theme);
    set({ theme });
  },
  glove: loadString(GLOVE_KEY) === "1",
  setGlove: (on) => {
    saveString(GLOVE_KEY, on ? "1" : "0");
    set({ glove: on });
  },
  minimapPin: loadMinimapPin(),
  setMinimapPin: (minimapPin) => {
    saveString(MINIMAP_PIN_KEY, minimapPin);
    set({ minimapPin });
  },
  radarScale: loadRadarScale(),
  setRadarScale: (radarScale) => {
    saveString(RADAR_SCALE_KEY, String(radarScale));
    set({ radarScale });
  },
}));

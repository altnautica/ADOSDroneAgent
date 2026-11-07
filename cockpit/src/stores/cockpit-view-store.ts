// The cockpit's view, layer level and layer flags, persisted per surface and
// per vehicle in localStorage (`ados-cockpit-view:<vehicle>`). The Feed binds
// the vehicle it shows (this drone, or a ground station's paired drone); the
// stored choice for that vehicle loads then. Readers: the Feed layout (view),
// the layer stack (effective layers), the top-bar layer button, the D-pad and
// the `V` / `O` keys.

import { create } from "zustand";

import {
  COCKPIT_VIEWS,
  LAYER_LEVELS,
  effectiveLayers,
  nextLevel,
  nextView,
  type CockpitView,
  type LayerLevel,
} from "@/lib/cockpit-view";

export { effectiveLayers } from "@/lib/cockpit-view";

const KEY_PREFIX = "ados-cockpit-view:";

interface Persisted {
  view: CockpitView;
  level: LayerLevel;
  layers: Record<string, boolean>;
}

const DEFAULTS: Persisted = { view: "pilot", level: "full", layers: {} };

function load(vehicle: string, defaultLevel: LayerLevel = DEFAULTS.level): Persisted {
  const fresh = (): Persisted => ({ ...DEFAULTS, level: defaultLevel, layers: {} });
  if (typeof localStorage === "undefined") return fresh();
  try {
    const raw = localStorage.getItem(KEY_PREFIX + vehicle);
    if (!raw) return fresh();
    const p = JSON.parse(raw) as Partial<Persisted>;
    const layers: Record<string, boolean> = {};
    if (p.layers && typeof p.layers === "object") {
      for (const [k, v] of Object.entries(p.layers)) if (typeof v === "boolean") layers[k] = v;
    }
    return {
      view: COCKPIT_VIEWS.includes(p.view as CockpitView) ? (p.view as CockpitView) : DEFAULTS.view,
      level: LAYER_LEVELS.includes(p.level as LayerLevel) ? (p.level as LayerLevel) : defaultLevel,
      layers,
    };
  } catch {
    return fresh();
  }
}

function save(vehicle: string, p: Persisted): void {
  if (typeof localStorage === "undefined") return;
  try {
    localStorage.setItem(KEY_PREFIX + vehicle, JSON.stringify(p));
  } catch {
    // storage full or blocked: the choice lasts the session
  }
}

export interface CockpitViewState extends Persisted {
  vehicle: string;
  /** Load the stored choice for `vehicle`; with none stored, the level is
   *  the width class's default (`layout.json` `default_layer_level`). */
  bindVehicle: (vehicle: string, defaultLevel: LayerLevel) => void;
  setView: (view: CockpitView) => void;
  cycleView: () => void;
  setLevel: (level: LayerLevel) => void;
  cycleLevel: () => void;
  toggleLayer: (id: string, on?: boolean) => void;
}

export const useCockpitViewStore = create<CockpitViewState>((set, get) => {
  const update = (patch: Partial<Persisted>) => {
    set(patch);
    const s = get();
    save(s.vehicle, { view: s.view, level: s.level, layers: s.layers });
  };
  return {
    vehicle: "",
    ...load(""),
    bindVehicle: (vehicle, defaultLevel) => {
      if (vehicle === get().vehicle) return;
      set({ vehicle, ...load(vehicle, defaultLevel) });
    },
    setView: (view) => update({ view }),
    cycleView: () => update({ view: nextView(get().view) }),
    setLevel: (level) => update({ level }),
    cycleLevel: () => update({ level: nextLevel(get().level) }),
    toggleLayer: (id, on) => {
      const cur = effectiveLayers("full", get().layers, "pilot", [id]).has(id);
      update({ layers: { ...get().layers, [id]: on ?? !cur } });
    },
  };
});

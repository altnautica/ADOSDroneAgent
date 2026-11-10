// Settings › Controls state, persisted in localStorage
// `ados-cockpit-controls`: the saved binding list, the stick mode, the
// deadzone, per-pad calibration, the on-screen sticks toggle and whether the
// first-connect tour ran. The editor works on `draft`; Save commits it (and,
// on a ground station's kiosk, writes the same document to
// `PUT /api/v1/ground-station/input/bindings` so the native input applies the
// map). The stick mode and the deadzone apply at once.
//
// On a ground-station kiosk `GET /api/v1/ground-station/input` says whether
// the station flies the sticks natively (`native_sticks`); the browser then
// streams no sticks and ignores PS and the arm / kill combos.

import { create } from "zustand";

import { isKiosk } from "@/lib/kiosk";
import {
  buildDoc,
  clampDeadzone,
  cloneBindings,
  defaultBindings,
  findConflicts,
  lockedLabel,
  missingLocked,
  parseControlsDoc,
  type ControlsDoc,
  type PadKind,
} from "@/lib/pad/bindings";
import type { PadCalibration } from "@/lib/pad/conditioning";
import {
  DEADZONE_DEFAULT,
  DEFAULT_STICK_MODE,
  LOCKED_ACTIONS,
  SPEC_ACTION_BY_ID,
  type PadBinding,
  type StickMode,
} from "@/lib/pad/ds4-map";
import { ApiError, apiFetch } from "@/shared/api-fetch";
import { specText } from "@/shared/hud/spec-text";

export const CONTROLS_KEY = "ados-cockpit-controls";
export const GS_INPUT_PATH = "/api/v1/ground-station/input";
export const GS_BINDINGS_PATH = "/api/v1/ground-station/input/bindings";

interface Persisted {
  doc: ControlsDoc;
  calibration: Record<string, PadCalibration>;
  onscreen_sticks: boolean;
  tour_seen: boolean;
}

function defaults(): Persisted {
  return {
    doc: buildDoc(defaultBindings(), DEFAULT_STICK_MODE, DEADZONE_DEFAULT, "ds4"),
    calibration: {},
    onscreen_sticks: false,
    tour_seen: false,
  };
}

function isCalibration(v: unknown): v is PadCalibration {
  if (typeof v !== "object" || v === null) return false;
  const c = v as Record<string, unknown>;
  return Array.isArray(c.centre) && Array.isArray(c.min) && Array.isArray(c.max);
}

function load(): Persisted {
  const base = defaults();
  if (typeof localStorage === "undefined") return base;
  try {
    const raw = localStorage.getItem(CONTROLS_KEY);
    if (!raw) return base;
    const p = JSON.parse(raw) as Partial<Record<keyof Persisted, unknown>>;
    const parsed = p.doc ? parseControlsDoc(JSON.stringify(p.doc)) : null;
    const calibration: Record<string, PadCalibration> = {};
    if (p.calibration && typeof p.calibration === "object") {
      for (const [id, c] of Object.entries(p.calibration)) {
        if (isCalibration(c)) calibration[id] = { ...c, axisMap: c.axisMap ?? null };
      }
    }
    return {
      doc: parsed?.ok ? parsed.doc : base.doc,
      calibration,
      onscreen_sticks: p.onscreen_sticks === true,
      tour_seen: p.tour_seen === true,
    };
  } catch {
    return base;
  }
}

function persist(p: Persisted): void {
  if (typeof localStorage === "undefined") return;
  try {
    localStorage.setItem(CONTROLS_KEY, JSON.stringify(p));
  } catch {
    // storage blocked or full: the change lasts this session
  }
}

/** The ground station's input state, as far as the cockpit uses it. */
export interface GroundInput {
  nativeSticks: boolean;
  controller: { name: string; connection: string; batteryPct: number | null } | null;
}

export function parseGroundInput(raw: unknown): GroundInput | null {
  if (typeof raw !== "object" || raw === null) return null;
  const r = raw as Record<string, unknown>;
  const c = r.controller as Record<string, unknown> | null | undefined;
  return {
    nativeSticks: r.native_sticks === true,
    controller:
      c && typeof c === "object" && typeof c.name === "string"
        ? {
            name: c.name,
            connection: typeof c.connection === "string" ? c.connection : "",
            batteryPct: typeof c.battery_pct === "number" ? c.battery_pct : null,
          }
        : null,
  };
}

/** `"<action>:<gesture>[:combo]"` (the ground station's 422
 *  `{detail:{error:{code, missing}}}`) → labels. */
export function missingFromServer(body: unknown): string[] {
  const b = body as { detail?: { error?: { missing?: unknown } }; missing?: unknown } | null;
  const list = b?.detail?.error?.missing ?? b?.missing;
  if (!Array.isArray(list)) return [];
  return list
    .filter((m): m is string => typeof m === "string")
    .map((m) => {
      const [action, gesture, combo] = m.split(":");
      const label = SPEC_ACTION_BY_ID[action]?.label ?? action;
      return combo ? `${label} (combo ${gesture})` : `${label} (${gesture})`;
    });
}

export type SyncState =
  | { state: "idle" }
  | { state: "saving" }
  | { state: "saved" }
  | { state: "refused"; missing: string[] }
  | { state: "error"; message: string };

interface ControlsState {
  bindings: PadBinding[];
  draft: PadBinding[];
  stickMode: StickMode;
  deadzone: number;
  pad: PadKind;
  calibration: Record<string, PadCalibration>;
  onscreenSticks: boolean;
  tourSeen: boolean;
  /** A ground-station kiosk (its native input reads the bindings too). */
  stationKiosk: boolean;
  groundInput: GroundInput | null;
  sync: SyncState;

  setDraft: (rows: PadBinding[]) => void;
  discardDraft: () => void;
  /** Commit the draft. Refused (false) while it conflicts or lacks a locked
   *  binding. */
  save: () => Promise<boolean>;
  resetToDefault: () => void;
  /** Import an export file into the draft. Returns errors, empty on success. */
  importText: (text: string) => string[];
  exportDoc: () => ControlsDoc;
  setStickMode: (mode: StickMode) => void;
  setDeadzone: (dz: number) => void;
  setPadKind: (pad: PadKind) => void;
  setCalibration: (padId: string, cal: PadCalibration | null) => void;
  setOnscreenSticks: (on: boolean) => void;
  markTourSeen: () => void;
  /** Settings › Controls › Replay controller tour: it shows again on the
   *  ground with a controller reporting. */
  replayTour: () => void;
  /** Read the ground station's input state (kiosk on a ground station only). */
  loadGroundInput: (signal?: AbortSignal) => Promise<void>;
  setStationKiosk: (on: boolean) => void;
}

const initial = load();

export const useControlsStore = create<ControlsState>((set, get) => {
  const snapshot = (): Persisted => {
    const s = get();
    return {
      doc: buildDoc(s.bindings, s.stickMode, s.deadzone, s.pad),
      calibration: s.calibration,
      onscreen_sticks: s.onscreenSticks,
      tour_seen: s.tourSeen,
    };
  };
  const commit = () => persist(snapshot());

  /** On a ground-station kiosk, write the document to the native input. */
  const pushToStation = async (): Promise<boolean> => {
    if (!get().stationKiosk) return true;
    set({ sync: { state: "saving" } });
    try {
      await apiFetch(GS_BINDINGS_PATH, { method: "PUT", body: get().exportDoc() });
      set({ sync: { state: "saved" } });
      return true;
    } catch (e) {
      if (e instanceof ApiError && e.status === 422) {
        set({ sync: { state: "refused", missing: missingFromServer(e.body) } });
      } else {
        set({ sync: { state: "error", message: e instanceof Error ? e.message : "Write failed" } });
      }
      return false;
    }
  };

  return {
    bindings: cloneBindings(initial.doc.bindings),
    draft: cloneBindings(initial.doc.bindings),
    stickMode: initial.doc.stick_mode,
    deadzone: initial.doc.deadzone,
    pad: initial.doc.pad,
    calibration: initial.calibration,
    onscreenSticks: initial.onscreen_sticks,
    tourSeen: initial.tour_seen,
    stationKiosk: false,
    groundInput: null,
    sync: { state: "idle" },

    setDraft: (rows) => set({ draft: rows }),
    discardDraft: () => set({ draft: cloneBindings(get().bindings) }),
    save: async () => {
      const draft = get().draft;
      if (findConflicts(draft).length > 0 || missingLocked(draft).length > 0) return false;
      set({ bindings: cloneBindings(draft) });
      commit();
      return pushToStation();
    },
    resetToDefault: () => set({ draft: defaultBindings() }),
    importText: (text) => {
      const r = parseControlsDoc(text);
      if (!r.ok) return r.errors;
      // All or nothing: a file the editor could not save is refused whole,
      // so stick mode and deadzone never go live ahead of its bindings.
      const conflicts = findConflicts(r.doc.bindings).length;
      const missing = missingLocked(r.doc.bindings).map(lockedLabel);
      const errors = [
        ...(conflicts > 0 ? [`${conflicts} binding conflict(s)`] : []),
        ...missing.map((m) => `Missing locked binding: ${m}`),
      ];
      if (errors.length > 0) return errors;
      set({
        draft: cloneBindings(r.doc.bindings),
        bindings: cloneBindings(r.doc.bindings),
        stickMode: r.doc.stick_mode,
        deadzone: r.doc.deadzone,
      });
      commit();
      void pushToStation();
      return [];
    },
    exportDoc: () => {
      const s = get();
      return buildDoc(s.bindings, s.stickMode, s.deadzone, s.pad);
    },
    setStickMode: (stickMode) => {
      set({ stickMode });
      commit();
      void pushToStation();
    },
    setDeadzone: (dz) => {
      set({ deadzone: clampDeadzone(dz) });
      commit();
    },
    setPadKind: (pad) => {
      if (get().pad !== pad) set({ pad });
    },
    setCalibration: (padId, cal) => {
      const calibration = { ...get().calibration };
      if (cal) calibration[padId] = cal;
      else delete calibration[padId];
      set({ calibration });
      commit();
    },
    setOnscreenSticks: (onscreenSticks) => {
      set({ onscreenSticks });
      commit();
    },
    markTourSeen: () => {
      set({ tourSeen: true });
      commit();
    },
    replayTour: () => {
      set({ tourSeen: false });
      commit();
    },
    loadGroundInput: async (signal) => {
      try {
        const raw = await apiFetch(GS_INPUT_PATH, { signal });
        set({ groundInput: parseGroundInput(raw) });
      } catch {
        if (!signal?.aborted) set({ groundInput: null });
      }
    },
    setStationKiosk: (on) => {
      if (get().stationKiosk !== on) set({ stationKiosk: on && isKiosk() });
    },
  };
});

/** Labels of the locked actions, for the editor's notes. */
export const LOCKED_LABELS = LOCKED_ACTIONS.map((l) => SPEC_ACTION_BY_ID[l.action]?.label ?? l.action);

/** The line under the actions list when the ground station refused a sync. */
export function syncRefusedText(missing: readonly string[]): string {
  return specText("controls.sync.refused", { actions: missing.join(", ") });
}

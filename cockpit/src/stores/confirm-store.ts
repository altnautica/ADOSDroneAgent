// The open skill confirmation, shared by every input path: the touch sheet,
// a held controller button or keyboard chord (through the cockpit action's
// press/release), a held Enter key, and a long-press of the panel's activate
// button all drive the same hold here, so a sheet opened from any source can
// be completed from any other. A `hold_confirm` skill (kill) moves from its
// hold stage to an explicit confirm stage once the hold completes.

import { create } from "zustand";

import { LAUNCH } from "@/lib/cockpit-flow";
import type { Skill } from "@/lib/skills";

export interface PendingConfirm {
  skill: Skill;
  altitudeM: number;
  /** The gamepad button that opened the sheet; holding it satisfies the hold. */
  gamepadButton: number | null;
  /** `hold`: the hold is running or awaited; `confirm`: the hold of a
   *  `hold_confirm` skill completed and an explicit confirm is due. */
  stage: "hold" | "confirm";
}

export type AckKind = "ok" | "warn" | "err";
export interface AckLine {
  text: string;
  kind: AckKind;
}

interface ConfirmState {
  pending: PendingConfirm | null;
  /** When a non-pointer hold (controller, chord, Enter) began, else null. */
  heldSince: number | null;
  /** Bumped by a panel long-press or a controller-timed hold, which complete
   *  the hold (or the confirm) in one gesture. */
  panelConfirmNonce: number;
  /** The last command outcome. */
  ack: AckLine | null;

  open: (skill: Skill, gamepadButton?: number | null, stage?: "hold" | "confirm") => void;
  cancel: () => void;
  setAltitude: (m: number) => void;
  toConfirmStage: () => void;
  setHeld: (down: boolean) => void;
  panelConfirm: () => void;
  setAck: (ack: AckLine | null) => void;
}

export const useConfirmStore = create<ConfirmState>((set, get) => ({
  pending: null,
  heldSince: null,
  panelConfirmNonce: 0,
  ack: null,

  open: (skill, gamepadButton = null, stage = "hold") =>
    set({ pending: { skill, altitudeM: LAUNCH.altDefaultM, gamepadButton, stage }, heldSince: null }),
  cancel: () => set({ pending: null, heldSince: null }),
  setAltitude: (m) => set((s) => (s.pending ? { pending: { ...s.pending, altitudeM: m } } : s)),
  toConfirmStage: () =>
    set((s) => (s.pending ? { pending: { ...s.pending, stage: "confirm" }, heldSince: null } : s)),
  setHeld: (down) => {
    const { heldSince, pending } = get();
    if (!pending) return;
    if (down && heldSince === null) set({ heldSince: performance.now() });
    else if (!down && heldSince !== null) set({ heldSince: null });
  },
  panelConfirm: () => set((s) => ({ panelConfirmNonce: s.panelConfirmNonce + 1 })),
  setAck: (ack) => set({ ack }),
}));

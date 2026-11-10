// The open skill confirmation, shared by every input path: the touch sheet,
// a held controller button or keyboard chord (through the cockpit action's
// press/release), a held Enter key, and a long-press of the panel's activate
// button all drive the same hold here, so a sheet opened from any source can
// be completed from any other. Every hold, the emergency stop's included,
// fires the skill once it completes; there is no second confirm step.

import { create } from "zustand";

import { LAUNCH } from "@/lib/cockpit-flow";
import type { Skill } from "@/lib/skills";

export interface PendingConfirm {
  skill: Skill;
  altitudeM: number;
  /** The gamepad button that opened the sheet; holding it satisfies the hold. */
  gamepadButton: number | null;
  /** When a panel long press of select armed the emergency stop
   *  (`performance.now()`), else null. Only a panel long select arms it, so
   *  a sheet opened any other way (short select, touch, pad) still takes two
   *  long selects; the second fires once it is `PANEL_STOP_ARM_MS` later. */
  panelArmedAt: number | null;
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
  /** Bumped by a panel long-press, which completes the hold in one
   *  gesture. */
  panelConfirmNonce: number;
  /** The last command outcome. */
  ack: AckLine | null;

  open: (skill: Skill, gamepadButton?: number | null) => void;
  cancel: () => void;
  setAltitude: (m: number) => void;
  setHeld: (down: boolean) => void;
  panelConfirm: () => void;
  /** Record the panel long select that armed the open sheet's stop. */
  armPanel: (at: number) => void;
  setAck: (ack: AckLine | null) => void;
}

export const useConfirmStore = create<ConfirmState>((set, get) => ({
  pending: null,
  heldSince: null,
  panelConfirmNonce: 0,
  ack: null,

  open: (skill, gamepadButton = null) =>
    set({ pending: { skill, altitudeM: LAUNCH.altDefaultM, gamepadButton, panelArmedAt: null }, heldSince: null }),
  cancel: () => set({ pending: null, heldSince: null }),
  setAltitude: (m) => set((s) => (s.pending ? { pending: { ...s.pending, altitudeM: m } } : s)),
  setHeld: (down) => {
    const { heldSince, pending } = get();
    if (!pending) return;
    if (down && heldSince === null) set({ heldSince: performance.now() });
    else if (!down && heldSince !== null) set({ heldSince: null });
  },
  panelConfirm: () => set((s) => ({ panelConfirmNonce: s.panelConfirmNonce + 1 })),
  armPanel: (at) => set((s) => (s.pending ? { pending: { ...s.pending, panelArmedAt: at } } : s)),
  setAck: (ack) => set({ ack }),
}));

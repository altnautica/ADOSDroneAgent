// The open skill confirmation, shared by every input path: the touch sheet,
// a held gamepad button, a held Enter key, and a long-press of the panel's
// activate button all drive the same hold here, so a sheet opened from any
// source can be completed from any other.

import { create } from "zustand";

import { DEFAULT_TAKEOFF_ALT_M, GUARD_WINDOW_MS, type Skill } from "@/lib/skills";

export interface PendingConfirm {
  skill: Skill;
  altitudeM: number;
  /** The gamepad button that opened the sheet; holding it satisfies the hold. */
  gamepadButton: number | null;
}

export type AckKind = "ok" | "warn" | "err";
export interface AckLine {
  text: string;
  kind: AckKind;
}

interface ConfirmState {
  pending: PendingConfirm | null;
  /** Kill guard: armed until this `performance.now()` time, else null. */
  guardUntil: number | null;
  /** When a non-pointer hold (gamepad, Enter) began, else null. */
  heldSince: number | null;
  /** Bumped by a panel long-press, which completes a hold in one gesture. */
  panelConfirmNonce: number;
  /** A command is in flight; every skill is held off until it settles. */
  busy: boolean;
  /** The last command outcome, shown above the Skill Bar. */
  ack: AckLine | null;

  open: (skill: Skill, gamepadButton?: number | null) => void;
  cancel: () => void;
  setAltitude: (m: number) => void;
  armGuard: () => void;
  setHeld: (down: boolean) => void;
  panelConfirm: () => void;
  setBusy: (busy: boolean) => void;
  setAck: (ack: AckLine | null) => void;
}

export const useConfirmStore = create<ConfirmState>((set, get) => ({
  pending: null,
  guardUntil: null,
  heldSince: null,
  panelConfirmNonce: 0,
  busy: false,
  ack: null,

  open: (skill, gamepadButton = null) =>
    set({
      pending: { skill, altitudeM: DEFAULT_TAKEOFF_ALT_M, gamepadButton },
      heldSince: null,
      guardUntil: null,
    }),
  cancel: () => set({ pending: null, heldSince: null, guardUntil: null }),
  setAltitude: (m) =>
    set((s) => (s.pending ? { pending: { ...s.pending, altitudeM: m } } : s)),
  armGuard: () => set({ guardUntil: performance.now() + GUARD_WINDOW_MS, heldSince: null }),
  setHeld: (down) => {
    const { heldSince, pending } = get();
    if (!pending) return;
    if (down && heldSince === null) set({ heldSince: performance.now() });
    else if (!down && heldSince !== null) set({ heldSince: null });
  },
  panelConfirm: () => set((s) => ({ panelConfirmNonce: s.panelConfirmNonce + 1 })),
  setBusy: (busy) => set({ busy }),
  setAck: (ack) => set({ ack }),
}));

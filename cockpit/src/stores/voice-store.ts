// Voice prompts (Settings › Sound › Voice prompts, default on): spoken
// handovers, RTH, low battery and mission-state changes through the browser's
// speech synthesis. Persisted to localStorage per surface.

import { create } from "zustand";

const KEY = "ados-cockpit-voice";

function load(): boolean {
  if (typeof localStorage === "undefined") return true;
  try {
    return localStorage.getItem(KEY) !== "off";
  } catch {
    return true;
  }
}

interface VoiceState {
  enabled: boolean;
  setEnabled: (on: boolean) => void;
}

export const useVoiceStore = create<VoiceState>((set) => ({
  enabled: load(),
  setEnabled: (enabled) => {
    set({ enabled });
    try {
      localStorage.setItem(KEY, enabled ? "on" : "off");
    } catch {
      // storage blocked: the choice lasts the session
    }
  },
}));

/** Whether this browser can speak at all. */
export function voiceSupported(): boolean {
  return typeof window !== "undefined" && "speechSynthesis" in window && typeof SpeechSynthesisUtterance !== "undefined";
}

/** Speak a prompt now, replacing anything still queued. */
export function speak(text: string): void {
  if (!useVoiceStore.getState().enabled || !voiceSupported() || !text) return;
  window.speechSynthesis.cancel();
  window.speechSynthesis.speak(new SpeechSynthesisUtterance(text));
}

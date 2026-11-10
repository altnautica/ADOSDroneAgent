// The gimbal rate driver: whether the vehicle takes gimbal rates (polled while
// the Feed is up), and the one sender for the rates the gimbal pad and the
// controller's L2/R2 ask for. Requests are paced by `createGimbalRateSender`:
// at most one per 100 ms while the rate changes, a resend before the agent's
// hold runs out while it is held, and one zero rate on release.

import { useEffect, useRef, useState } from "react";
import { create } from "zustand";

import { createGimbalRateSender, gimbalRateAvailability, sendGimbalRate, type GimbalRateSender } from "@/lib/gimbal-rate";
import { useInputStore } from "@/stores/input-store";

const AVAILABILITY_POLL_MS = 5000;

interface GimbalPadState {
  /** Deg/s from the on-screen pad; pitch + up, yaw + right. */
  pitch: number;
  yaw: number;
  setRate: (pitch: number, yaw: number) => void;
}

export const useGimbalPadStore = create<GimbalPadState>((set) => ({
  pitch: 0,
  yaw: 0,
  setRate: (pitch, yaw) => set({ pitch, yaw }),
}));

/** Whether the gimbal pad shows: the vehicle answers that it takes rates. */
export function useGimbalAvailable(enabled: boolean): boolean {
  const [available, setAvailable] = useState(false);
  useEffect(() => {
    if (!enabled) return;
    const ac = new AbortController();
    const poll = () =>
      gimbalRateAvailability(ac.signal).then(
        (a) => setAvailable(a.available),
        () => setAvailable(false),
      );
    void poll();
    const id = setInterval(() => void poll(), AVAILABILITY_POLL_MS);
    return () => {
      clearInterval(id);
      ac.abort();
    };
  }, [enabled]);
  return enabled && available;
}

/** Sends the held rate (pad, else L2/R2 pitch) while `available`. */
export function useGimbalDriver(available: boolean): void {
  const padPitch = useGimbalPadStore((s) => s.pitch);
  const padYaw = useGimbalPadStore((s) => s.yaw);
  const triggerPitch = useInputStore((s) => s.gimbalRateDps);
  const pad = padPitch !== 0 || padYaw !== 0;
  const pitch = pad ? padPitch : triggerPitch;
  const yaw = pad ? padYaw : 0;
  const sender = useRef<GimbalRateSender | null>(null);

  useEffect(() => {
    if (!available) return;
    // The sender reads the answer: one request in flight, and a 429 backs off.
    const s = createGimbalRateSender(sendGimbalRate);
    sender.current = s;
    return () => {
      sender.current = null;
      s.dispose();
    };
  }, [available]);

  useEffect(() => {
    sender.current?.set(pitch, yaw);
  }, [available, pitch, yaw]);
}

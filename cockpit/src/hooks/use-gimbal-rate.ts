// The gimbal rate driver: whether the vehicle takes gimbal rates (polled while
// the Feed is up), and the one sender for the rates the gimbal pad and the
// controller's L2/R2 ask for. While a rate is held it is resent before the
// agent's hold runs out; on release one zero rate stops the gimbal at once.

import { useEffect, useState } from "react";
import { create } from "zustand";

import { GIMBAL_RESEND_MS, gimbalRateAvailability, sendGimbalRate } from "@/lib/gimbal-rate";
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
  const pitch = padPitch !== 0 || padYaw !== 0 ? padPitch : triggerPitch;
  const yaw = padPitch !== 0 || padYaw !== 0 ? padYaw : 0;
  const moving = available && (pitch !== 0 || yaw !== 0);

  useEffect(() => {
    if (!moving) return;
    const send = () => void sendGimbalRate(pitch, yaw).catch(() => undefined);
    send();
    const id = setInterval(send, GIMBAL_RESEND_MS);
    return () => {
      clearInterval(id);
    };
  }, [moving, pitch, yaw]);

  // One zero rate when a held rate ends.
  useEffect(() => {
    if (!moving) return;
    return () => {
      void sendGimbalRate(0, 0).catch(() => undefined);
    };
  }, [moving]);
}

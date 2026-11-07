// The controller stick stream to the drone's pilot (mounted by the Feed).
// While the pilot reports control mode `ados`, the Feed is the input target and
// a standard-layout pad is live, it sends one `input` frame every 20 ms (50 Hz)
// from the latest pad sample: the sticks through the stick mode into
// `[roll, pitch, thr, yaw]` (pilot.v1 signs) and the buttons as the
// standard-index bitfield. It stops, and sends `release` so the pilot brakes
// to a hold, when the pad disconnects, the tab is hidden, the poller stops
// reading the pad for more than 150 ms, or another screen or the quick menu
// takes the controller (`lib/pad/pilot-stream.ts`).
//
// Liveness is the poll time, not `Gamepad.timestamp`: browsers advance that
// only when a value changes, so a stick held still would read as lost.
//
// In `fc_manual` and `elrs` the cockpit streams nothing: the FC or the RC link
// has the sticks. On a ground station whose native input flies the sticks
// (`native_sticks`), the kiosk browser streams nothing either.

import { useEffect, useState } from "react";

import { buttonsBitfield, pilotAxes } from "@/lib/pad/conditioning";
import { STREAM_INTERVAL_MS } from "@/lib/pad/ds4-map";
import { PilotStreamer, type StopReason } from "@/lib/pad/pilot-stream";
import { releasePilotInput, sendPilotInput } from "@/lib/pilot-link";
import { nativeInputActive } from "@/hooks/use-gamepad";
import { useControlsStore } from "@/stores/controls-store";
import { useInputStore } from "@/stores/input-store";
import { activeScreenId, useNavStore } from "@/stores/nav-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

export interface PilotInputState {
  /** Why the controller is not streaming, or null while it streams. */
  stopped: StopReason | null;
}

export function usePilotInput(): PilotInputState {
  const [stopped, setStopped] = useState<StopReason | null>("disabled");

  useEffect(() => {
    const streamer = new PilotStreamer({ send: sendPilotInput, release: releasePilotInput });
    let last: StopReason | null = "disabled";

    const tick = () => {
      const pad = useInputStore.getState().pad;
      const controls = useControlsStore.getState();
      const usable = pad !== null && pad.standard;
      const nav = useNavStore.getState();
      const target = activeScreenId(nav) === "feed" && !nav.quickMenuOpen;
      const pilot = useNavStreamStore.getState().pilot?.data ?? null;
      const reason = streamer.tick({
        now: performance.now(),
        enabled: pilot?.controlMode === "ados" && !nativeInputActive(),
        connected: usable,
        hidden: document.visibilityState === "hidden",
        target,
        sampleAt: pad?.polledAt ?? null,
        axes: pad ? pilotAxes(pad.sticks, controls.stickMode) : [0, 0, 0, 0],
        buttons: pad ? buttonsBitfield(pad.pressed, pad.values) : 0,
      });
      if (reason !== last) {
        last = reason;
        setStopped(reason);
      }
    };

    // A hidden tab throttles timers: release at once rather than on the next
    // (possibly seconds-late) tick.
    const onVisibility = () => tick();
    document.addEventListener("visibilitychange", onVisibility);
    const timer = setInterval(tick, STREAM_INTERVAL_MS);
    return () => {
      clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibility);
      streamer.stop();
    };
  }, []);

  return { stopped };
}

// Holds a screen wake lock so the HDMI panel never blanks while the cockpit is
// up. The lock is dropped by the browser when the tab is hidden (e.g. a VT
// switch), so it is re-acquired whenever the page becomes visible again. A
// no-op where the Screen Wake Lock API is unavailable (the kiosk keeps the
// panel awake at the OS level too).

import { useEffect, useRef, useState } from "react";

export interface WakeLockState {
  /** True while a wake lock is currently held. */
  held: boolean;
  /** True when the browser exposes the Screen Wake Lock API at all. */
  supported: boolean;
}

export function useWakeLock(): WakeLockState {
  const supported =
    typeof navigator !== "undefined" && "wakeLock" in navigator;
  const [held, setHeld] = useState(false);
  const sentinel = useRef<WakeLockSentinel | null>(null);

  useEffect(() => {
    if (!supported) return;
    let cancelled = false;

    const acquire = async () => {
      if (cancelled || document.visibilityState !== "visible") return;
      if (sentinel.current) return;
      try {
        const lock = await navigator.wakeLock.request("screen");
        if (cancelled) {
          void lock.release();
          return;
        }
        sentinel.current = lock;
        setHeld(true);
        lock.addEventListener("release", () => {
          sentinel.current = null;
          setHeld(false);
        });
      } catch {
        // Denied (not visible, policy) — a later visibility change retries.
        setHeld(false);
      }
    };

    const onVisibility = () => {
      if (document.visibilityState === "visible") void acquire();
    };

    void acquire();
    document.addEventListener("visibilitychange", onVisibility);

    return () => {
      cancelled = true;
      document.removeEventListener("visibilitychange", onVisibility);
      const lock = sentinel.current;
      sentinel.current = null;
      if (lock) void lock.release();
    };
  }, [supported]);

  return { held, supported };
}

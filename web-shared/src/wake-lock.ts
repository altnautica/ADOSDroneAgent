// Screen wake lock while `enabled` and the document is visible. The browser
// drops the lock when the tab is hidden, so it is re-acquired on the next
// visibility change. A no-op where the Screen Wake Lock API is unavailable.

import { useEffect, useRef, useState } from "react";

export interface WakeLockState {
  /** True while a wake lock is currently held. */
  held: boolean;
  /** True when the browser exposes the Screen Wake Lock API at all. */
  supported: boolean;
}

export function useWakeLock(enabled = true): WakeLockState {
  const supported = typeof navigator !== "undefined" && "wakeLock" in navigator;
  const [held, setHeld] = useState(false);
  const sentinel = useRef<WakeLockSentinel | null>(null);

  useEffect(() => {
    if (!supported || !enabled) return;
    let cancelled = false;

    const acquire = async () => {
      if (cancelled || document.visibilityState !== "visible" || sentinel.current) return;
      try {
        const lock = await navigator.wakeLock.request("screen");
        if (cancelled) {
          void lock.release().catch(() => undefined);
          return;
        }
        sentinel.current = lock;
        setHeld(true);
        lock.addEventListener("release", () => {
          sentinel.current = null;
          setHeld(false);
        });
      } catch {
        // Denied (policy, battery saver): a later visibility change retries.
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
      if (lock) void lock.release().catch(() => undefined);
      setHeld(false);
    };
  }, [supported, enabled]);

  return { held, supported };
}

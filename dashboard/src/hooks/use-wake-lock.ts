import { useEffect, useRef } from "react";

// Keep the display awake while the console is open. A laptop or the
// ground-station console left on the dashboard should not blank its screen
// mid-flight. The Screen Wake Lock API auto-releases when the tab is hidden,
// so we re-acquire on visibilitychange. Requires a secure context — the agent
// is reached over localhost / the LAN, which qualifies. Unsupported engines
// (older Safari/Firefox) are a no-op.
export function useWakeLock() {
  const lockRef = useRef<WakeLockSentinel | null>(null);

  useEffect(() => {
    const wakeLock = navigator.wakeLock;
    if (!wakeLock) return;

    let cancelled = false;

    const acquire = async () => {
      if (document.visibilityState !== "visible" || lockRef.current) return;
      try {
        const sentinel = await wakeLock.request("screen");
        if (cancelled) {
          void sentinel.release().catch(() => {});
          return;
        }
        lockRef.current = sentinel;
        sentinel.addEventListener("release", () => {
          lockRef.current = null;
        });
      } catch {
        // Denied (e.g. low battery, policy). Nothing actionable.
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
      void lockRef.current?.release().catch(() => {});
      lockRef.current = null;
    };
  }, []);
}

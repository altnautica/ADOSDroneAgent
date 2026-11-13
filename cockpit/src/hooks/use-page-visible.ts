// Whether the page is visible. Every lease the cockpit holds on the drone's
// work (the nav-stream view lease, the vision demand and detection leases)
// is released while the page is hidden (cockpit spec `layers.json`
// `view_lease.release_when: document_hidden`) and re-taken when it shows.

import { useEffect, useState } from "react";

/** True while the page is visible. */
export function usePageVisible(): boolean {
  const [visible, setVisible] = useState(() => typeof document === "undefined" || document.visibilityState !== "hidden");
  useEffect(() => {
    const on = () => setVisible(document.visibilityState !== "hidden");
    document.addEventListener("visibilitychange", on);
    return () => document.removeEventListener("visibilitychange", on);
  }, []);
  return visible;
}

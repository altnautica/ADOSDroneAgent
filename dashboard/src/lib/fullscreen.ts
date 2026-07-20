// Request true fullscreen on the whole document (best effort). Used by the
// cockpit launcher so a laptop browser matches the appliance view. Fullscreen
// can be refused (no user activation, browser policy, unsupported engine) — a
// caller should proceed regardless of the outcome.
export async function requestDocumentFullscreen(): Promise<void> {
  if (typeof document === "undefined" || document.fullscreenElement) return;
  const el = document.documentElement as HTMLElement & {
    webkitRequestFullscreen?: () => Promise<void> | void;
  };
  try {
    if (el.requestFullscreen) {
      await el.requestFullscreen();
    } else if (el.webkitRequestFullscreen) {
      await el.webkitRequestFullscreen();
    }
  } catch {
    // Refused or interrupted; the caller navigates anyway.
  }
}

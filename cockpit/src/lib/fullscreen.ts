// The Feed's full screen. In a browser the Feed sits inside the shell beside
// the menu; the top bar's Full screen button asks the browser for full screen
// and draws the Feed edge to edge. The flag is set whether or not the browser
// grants it (an embedded view may refuse), so the button always does what it
// says on the page. The kiosk is always full screen and offers no exit.

import { isKiosk } from "@/lib/kiosk";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";

type FullscreenElement = HTMLElement & { webkitRequestFullscreen?: () => Promise<void> | void };
type FullscreenDocument = Document & { webkitExitFullscreen?: () => Promise<void> | void; webkitFullscreenElement?: Element | null };

export async function enterFlightFullscreen(): Promise<void> {
  if (isKiosk()) {
    useCockpitUiStore.getState().setFlightFullscreen(true);
    return;
  }
  const el = document.documentElement as FullscreenElement;
  // Asked inside the press (the browser wants a user gesture), before the
  // page goes full bleed.
  const asked = el.requestFullscreen ? el.requestFullscreen() : el.webkitRequestFullscreen?.();
  useCockpitUiStore.getState().setFlightFullscreen(true);
  try {
    await asked;
  } catch {
    // Refused (an embedded view, a policy): the page is full bleed anyway.
  }
}

export async function exitFlightFullscreen(): Promise<void> {
  const doc = document as FullscreenDocument;
  try {
    if (doc.fullscreenElement) await doc.exitFullscreen();
    else if (doc.webkitFullscreenElement) await doc.webkitExitFullscreen?.();
  } catch {
    // Already left.
  }
  useCockpitUiStore.getState().setFlightFullscreen(false);
}

/** Leaving the browser's full screen (Escape, the browser's own control)
 *  frames the Feed again. The kiosk never leaves. Returns the unsubscribe. */
export function watchFullscreenExit(): () => void {
  if (isKiosk()) return () => {};
  const onChange = () => {
    const doc = document as FullscreenDocument;
    if (!doc.fullscreenElement && !doc.webkitFullscreenElement) useCockpitUiStore.getState().setFlightFullscreen(false);
  };
  document.addEventListener("fullscreenchange", onChange);
  document.addEventListener("webkitfullscreenchange", onChange);
  return () => {
    document.removeEventListener("fullscreenchange", onChange);
    document.removeEventListener("webkitfullscreenchange", onChange);
  };
}

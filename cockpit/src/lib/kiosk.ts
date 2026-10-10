// Whether the cockpit runs as the node's kiosk display (`?kiosk=1`), where it
// is the whole screen and offers no exit to the dashboard. Read once at load:
// the kiosk launcher sets it in the URL and nothing changes it afterwards.

const KIOSK = typeof window !== "undefined" && new URLSearchParams(window.location.search).get("kiosk") === "1";

export function isKiosk(): boolean {
  return KIOSK;
}

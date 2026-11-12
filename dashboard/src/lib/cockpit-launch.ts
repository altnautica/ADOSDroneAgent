// /cockpit is a separate agent-served bundle, not an SPA route, so the
// dashboard leaves with a hard navigation. The browser cockpit stays a framed
// page; only the attached panel runs it full-bleed, in its kiosk.
export function enterCockpit(): void {
  window.location.assign("/cockpit");
}

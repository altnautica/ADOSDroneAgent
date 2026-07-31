/** The cockpit's render profile, selected with `?layer=minimal` on the URL.
 *
 *  The kiosk has appended `layer=minimal` for a long time — on an explicit
 *  config flag and automatically on a low-RAM board — and nothing has ever read
 *  it, so the switch and its automatic trigger did nothing on screen.
 *
 *  What it costs to ignore: the cockpit floats its chrome over full-bleed live
 *  video behind `backdrop-filter: blur()`. A blurred region over a surface that
 *  changes every frame forces the compositor to re-read and re-blur its
 *  backdrop at the video's frame rate, which on a small embedded GPU is the
 *  single most expensive thing the page does.
 *
 *  Minimal trades that visual effect away. It must never trade away a DATUM:
 *  every value on screen in full mode is on screen in minimal mode, with the
 *  same freshness rules. Reduced fidelity, never reduced truth.
 */
export type RenderProfile = "full" | "minimal";

/** Resolve the profile from a query string. Anything other than `minimal` is
 *  `full`, so a typo degrades to the richer mode rather than silently
 *  stripping the UI. Pure, so it is testable without a DOM. */
export function renderProfileFrom(search: string): RenderProfile {
  return new URLSearchParams(search).get("layer") === "minimal"
    ? "minimal"
    : "full";
}

/** Read the profile from the current URL. */
export function renderProfile(): RenderProfile {
  if (typeof window === "undefined") return "full";
  return renderProfileFrom(window.location.search);
}

/** The class the stylesheet keys off. Applied once at the shell root. */
export function renderProfileClass(profile: RenderProfile): string {
  return profile === "minimal" ? "layer-minimal" : "";
}

/** Poll interval scaled for the active profile.
 *
 *  Minimal mode slows the fastest polls, because each one is a React render
 *  over the same compositor that is already the bottleneck. It does NOT change
 *  which values are shown or how staleness is judged — a slower poll only means
 *  a value updates less often, and any freshness gate keyed on age keeps
 *  working unchanged.
 */
export function pollIntervalMs(base: number, profile: RenderProfile): number {
  return profile === "minimal" ? Math.round(base * 2.5) : base;
}

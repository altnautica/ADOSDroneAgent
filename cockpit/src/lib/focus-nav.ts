// DOM focus movement for the controller paths. The panel buttons and the
// gamepad fold onto prev/next/activate; inside an overlay or a console screen
// those move real DOM focus between the focusable controls and click the
// focused one, and the focused control carries the shared `data-focused` ring
// (a programmatic focus does not always light `:focus-visible`).

export const FOCUSABLE =
  'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), iframe, [tabindex]:not([tabindex="-1"])';

/** The focusable controls inside `root`, in document order. */
export function focusablesIn(root: HTMLElement): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
    (el) => !el.closest("[inert]") && el.getAttribute("aria-hidden") !== "true",
  );
}

/** The element itself when it is focusable, else its first focusable child. */
export function firstFocusable(el: HTMLElement | null): HTMLElement | null {
  if (!el) return null;
  if (el.matches(FOCUSABLE)) return el;
  return focusablesIn(el)[0] ?? null;
}

/** Whether DOM focus is inside `root`. */
export function holdsFocus(root: HTMLElement | null): boolean {
  return root !== null && document.activeElement instanceof HTMLElement && root.contains(document.activeElement);
}

/** Move focus one control forward or back inside `root`, wrapping. */
export function moveFocus(root: HTMLElement, step: 1 | -1): void {
  const nodes = focusablesIn(root);
  if (nodes.length === 0) return;
  const here = nodes.indexOf(document.activeElement as HTMLElement);
  const next = here < 0 ? (step > 0 ? 0 : nodes.length - 1) : (here + step + nodes.length) % nodes.length;
  nodes[next].focus();
}

/** Click the focused control inside `root`. True when there was one. */
export function activateFocused(root: HTMLElement): boolean {
  const el = document.activeElement;
  if (!(el instanceof HTMLElement) || !root.contains(el)) return false;
  el.click();
  return true;
}

/** Keep `data-focused` on whichever control inside `root` holds focus.
 *  Returns the listener removal. */
export function trackFocusRing(root: HTMLElement): () => void {
  const onIn = (e: FocusEvent) => {
    root.querySelectorAll("[data-focused]").forEach((n) => n.removeAttribute("data-focused"));
    if (e.target instanceof HTMLElement) e.target.setAttribute("data-focused", "true");
  };
  const onOut = (e: FocusEvent) => {
    if (e.target instanceof HTMLElement) e.target.removeAttribute("data-focused");
  };
  root.addEventListener("focusin", onIn);
  root.addEventListener("focusout", onOut);
  return () => {
    root.removeEventListener("focusin", onIn);
    root.removeEventListener("focusout", onOut);
  };
}

/** Keep Tab and Shift+Tab inside `root`. */
export function trapTab(root: HTMLElement, e: KeyboardEvent): void {
  if (e.key !== "Tab") return;
  const nodes = focusablesIn(root);
  if (nodes.length === 0) return;
  const first = nodes[0];
  const last = nodes[nodes.length - 1];
  if (e.shiftKey && document.activeElement === first) {
    e.preventDefault();
    last.focus();
  } else if (!e.shiftKey && document.activeElement === last) {
    e.preventDefault();
    first.focus();
  }
}

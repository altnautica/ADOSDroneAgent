// Toast host. Re-exports the toast() helper from components.js so callers
// have a single import path for "fire a toast." mountToastHost ensures the
// host element lives at the top of the layout.

import { el, setToastHost, toast as _toast } from "../components.js";

export function mountToastHost(rootEl) {
  const host = el("div", {
    className: "toast-host",
    role: "status",
    "aria-live": "polite",
  });
  rootEl.appendChild(host);
  setToastHost(host);
  return host;
}

export const toast = _toast;

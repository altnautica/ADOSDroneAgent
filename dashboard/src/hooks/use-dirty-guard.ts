// Prompts the user via the browser's native beforeunload dialog when
// they try to close or reload the tab while a settings form has
// unsaved changes. The custom message is ignored by all modern
// browsers; setting `returnValue` is what triggers the prompt.
//
// Note: this does NOT fire for in-app `navigate()` calls. React
// Router's NavLink won't trigger it. It only catches real tab close,
// reload, or external-URL navigation. That's the intended scope.

import { useEffect } from "react";

export function useDirtyGuard(dirty: boolean) {
  useEffect(() => {
    if (!dirty) return;
    const onBeforeUnload = (e: BeforeUnloadEvent) => {
      e.preventDefault();
      e.returnValue = "";
    };
    window.addEventListener("beforeunload", onBeforeUnload);
    return () => window.removeEventListener("beforeunload", onBeforeUnload);
  }, [dirty]);
}

// Guards a settings form with unsaved changes against both ways of leaving:
// - closing or reloading the tab: the browser's native beforeunload prompt
//   (the custom text is ignored by modern browsers; `returnValue` triggers it);
// - in-app navigation (a nav link, Back): the router blocks it and asks.

import { useEffect } from "react";
import { useBlocker } from "react-router-dom";

import i18n from "@/shared/i18n";

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

  const blocker = useBlocker(
    ({ currentLocation, nextLocation }) => dirty && currentLocation.pathname !== nextLocation.pathname,
  );
  useEffect(() => {
    if (blocker.state !== "blocked") return;
    if (window.confirm(i18n.t("settings.shared.leaveUnsaved"))) blocker.proceed();
    else blocker.reset();
  }, [blocker]);
}

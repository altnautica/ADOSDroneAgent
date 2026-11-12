// Guards drafts with unsaved changes against both ways of leaving:
// - closing or reloading the tab: the browser's native beforeunload prompt
//   (the custom text is ignored by modern browsers; `returnValue` triggers it);
// - in-app navigation (a nav link, Back): the router blocks it and asks.
//
// Any form or field calls `useDirtyGuard(dirty)` for its own draft. The router
// supports one blocker at a time, so the blocking lives once, in
// `DirtyGuardHost`, which the app shell renders.

import { useEffect, useId, useSyncExternalStore } from "react";
import { useBlocker } from "react-router-dom";

import { dirtyRegistry } from "@/lib/dirty-registry";
import i18n from "@/shared/i18n";

export function useDirtyGuard(dirty: boolean) {
  const id = useId();
  useEffect(() => {
    dirtyRegistry.set(id, dirty);
  }, [id, dirty]);
  useEffect(() => () => dirtyRegistry.remove(id), [id]);
}

export function DirtyGuardHost() {
  const dirty = useSyncExternalStore(dirtyRegistry.subscribe, dirtyRegistry.anyDirty);

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
  return null;
}

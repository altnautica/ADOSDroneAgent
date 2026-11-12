// The toast bus: one call shape for every screen's transient feedback, with no
// UI library behind it. Each app mounts exactly one host that subscribes here
// and draws the queue (the cockpit's `components/ui/toast.tsx`), so importing
// a toast renderer anywhere else is never necessary.

import { ApiError } from "../api-fetch";
import i18n from "../i18n";

export type ToastKind = "ok" | "err" | "info";

export interface ToastMessage {
  id: number;
  kind: ToastKind;
  message: string;
  description?: string;
}

type Listener = (toast: ToastMessage) => void;

const listeners = new Set<Listener>();
let nextId = 1;

function emit(kind: ToastKind, message: string, description?: string): void {
  const toast: ToastMessage = { id: nextId++, kind, message, ...(description ? { description } : {}) };
  for (const listener of listeners) listener(toast);
}

/** Subscribe the app's toast host. Returns an unsubscribe. */
export function onToast(listener: Listener): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export const toast = {
  ok(message: string, description?: string) {
    emit("ok", message, description);
  },
  err(message: string, description?: string) {
    emit("err", message, description);
  },
  info(message: string, description?: string) {
    emit("info", message, description);
  },
};

/** Surface an unknown error from a mutation as a readable error toast. An
 *  armed refusal (`E_ARMED`: armed, or the arm state unknown because no flight
 *  controller reports it) reads as one sentence on every screen; any other refusal
 *  shows the agent's message without the HTTP status number. */
export function toastFromError(err: unknown, fallback = "Something went wrong."): void {
  if (err instanceof ApiError) {
    if (err.code === "E_ARMED") {
      toast.err(i18n.t("common.armedRefused"));
      return;
    }
    toast.err(err.message.replace(/^\d+\s/, "").trim() || fallback);
    return;
  }
  if (err instanceof Error) {
    toast.err(err.message || fallback);
    return;
  }
  toast.err(fallback);
}

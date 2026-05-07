// Thin wrapper around sonner so every route uses the same call shape
// and copy tone. Auto-dismiss is set on the Toaster mount in app-shell;
// these helpers just wrap toast.success / toast.error so importing
// sonner directly is never necessary outside the shell.

import { toast as sonnerToast } from "sonner";

import { ApiError } from "./api";

export const toast = {
  ok(message: string, description?: string) {
    sonnerToast.success(message, description ? { description } : undefined);
  },
  err(message: string, description?: string) {
    sonnerToast.error(message, description ? { description } : undefined);
  },
  info(message: string, description?: string) {
    sonnerToast(message, description ? { description } : undefined);
  },
};

// Common pattern: surface an unknown error from a mutation in a
// human-readable toast. Routes used to inline this; now they call
// toast.fromError(err).
export function toastFromError(err: unknown, fallback = "Something went wrong.") {
  if (err instanceof ApiError) {
    toast.err(`${err.status} ${err.message.replace(/^\d+\s/, "")}`);
    return;
  }
  if (err instanceof Error) {
    toast.err(err.message || fallback);
    return;
  }
  toast.err(fallback);
}

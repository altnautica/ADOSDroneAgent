import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

/** Merge conditional class lists, de-duplicating conflicting Tailwind classes. */
export function cn(...inputs: ClassValue[]): string {
  return twMerge(clsx(inputs));
}

/** Demo mode = `?demo=1` (or `?demo=true`) on the URL. It lets a developer see
 *  the detection overlay driven by a synthetic stream with no agent/camera
 *  attached (`npm run dev`, open `/cockpit/?demo=1`). It is never active on the
 *  real on-box build unless the operator explicitly adds the param, so it cannot
 *  fabricate data in normal use. */
export function isDemoMode(): boolean {
  if (typeof window === "undefined") return false;
  const v = new URLSearchParams(window.location.search).get("demo");
  return v === "1" || v === "true";
}

// Test kit for the status screens: a stand-in `useResource` that answers each
// call, in call order, from a queue of canned resources, so a server render of
// a screen sees a failed, empty or answered read without any network.

import { renderToStaticMarkup } from "react-dom/server";
import { createElement, type ComponentType } from "react";

import type { Resource } from "@/shared/use-resource";

let queue: Partial<Resource<unknown>>[] = [];
let fallback: Partial<Resource<unknown>> = {};
let calls = 0;

/** A read that failed with no data. */
export const FAILED: Partial<Resource<unknown>> = { data: null, error: "503 internal error", ready: true, stale: true };
/** A read that answered with `data`. */
export const ok = (data: unknown): Partial<Resource<unknown>> => ({ data, error: null, ready: true, stale: false });

/** Answer the screen's `useResource` calls with `answers`, in call order; calls
 *  past the end get `rest`. */
export function answerReads(answers: Partial<Resource<unknown>>[], rest: Partial<Resource<unknown>> = ok(null)): void {
  queue = answers;
  fallback = rest;
  calls = 0;
}

/** The mocked `useResource`. */
export function fakeUseResource<T>(): Resource<T> {
  const r = queue[calls] ?? fallback;
  calls += 1;
  return { data: null, error: null, ready: true, stale: false, status: null, refresh: () => {}, ...r } as Resource<T>;
}

/** Server-render `component` with `props` to its markup. */
export function renderScreen<P extends object>(component: ComponentType<P>, props: P): string {
  calls = 0;
  return renderToStaticMarkup(createElement(component, props));
}

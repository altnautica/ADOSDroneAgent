import { ApiError, isAuthChallenge } from "./api";

/**
 * The agent's own words when it answered and refused, or `null` when it did not
 * answer at all.
 *
 * The distinction is the whole point of this: "did not answer" and "answered,
 * and said no" are different faults with different fixes, and reporting the
 * second as the first sends the operator to check a power cable about a board
 * that is running fine.
 */
export function refusalDetail(error: unknown): string | null {
  if (!(error instanceof ApiError) || !isAuthChallenge(error.status)) return null;
  const body = error.body;
  if (body && typeof body === "object" && "detail" in body) {
    const detail = (body as { detail: unknown }).detail;
    if (typeof detail === "string" && detail.trim()) return detail;
  }
  return "This device refused the request.";
}

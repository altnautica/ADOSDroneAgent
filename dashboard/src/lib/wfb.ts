// Live WFB-tx / WFB-rx runtime read. The agent exposes the same
// /api/wfb endpoint on both profiles; the response carries TX-side
// and RX-side fields together and the caller decides which to render
// based on the agent profile and the reported state.

import { apiFetch } from "./api";
import type { WfbStatus } from "./types";

export function getWfbStatus() {
  return apiFetch<WfbStatus>("/api/wfb");
}

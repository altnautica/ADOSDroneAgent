// The Sensors screen: the hub, clock and per-stream table, plus the rig file
// upload. Reads are `/api/v1/sensors` and `/api/v1/clock`; the upload is
// `PUT /api/v1/sensors/rig`, which answers 400 `E_RIG_INVALID` for a document
// that fails the contract.

import { ApiError, apiFetch, errorDetail } from "@/shared/api-fetch";
import { CLOCK_PATH, SENSORS_PATH, rateVerdict, type ClockStatus, type SensorRow, type SensorsStatus } from "@/shared/api/sensors";

import type { Tone } from "./tone";

export function getSensors(signal?: AbortSignal): Promise<SensorsStatus> {
  return apiFetch<SensorsStatus>(SENSORS_PATH, { signal });
}
export function getClock(signal?: AbortSignal): Promise<ClockStatus> {
  return apiFetch<ClockStatus>(CLOCK_PATH, { signal });
}

/** The status tone of one stream by its measured against expected rate. */
export function sensorTone(row: Pick<SensorRow, "rate_hz" | "expected_hz">): Tone {
  switch (rateVerdict(row)) {
    case "ok":
      return "ok";
    case "degraded":
      return "warn";
    case "low":
      return "err";
    default:
      return "muted";
  }
}

/** The code and readable detail of a refused rig upload. */
export function rigRejection(err: unknown): { code: string; detail: string } {
  if (err instanceof ApiError) {
    return { code: err.code ?? String(err.status), detail: errorDetail(err.body, err.message) };
  }
  return { code: "?", detail: err instanceof Error ? err.message : String(err) };
}
